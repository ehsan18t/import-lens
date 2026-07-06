use import_lens_daemon::{
    ipc::{
        codec::{FrameDecoder, decode_payload, encode_frame},
        protocol::{
            AnalyzePackageJsonRequest, AnalyzePackageJsonResponse, BatchRequest, BatchResponse,
            CacheStatusRequest, CacheStatusResponse, FileSizeDocumentRequest,
            FileSizeDocumentResponse, FileSizeRequest, FileSizeResponse, FreshnessKind,
            HelloMessage, ImportAnalysisStatus, ImportKind, ImportRequest, ImportRuntime,
            PROTOCOL_VERSION, RefreshRegistryHintsRequest, RefreshRegistryHintsResponse,
            RefreshedResultsResponse, RegistryHintMode, RegistryHintTarget, ShutdownMessage,
        },
        server::{handle_connection, response_from_join},
    },
    prefetch::{CancellationToken, Prefetcher},
    registry::{
        cache::RegistryMetadataCache,
        service::RegistryHintService,
        types::{HttpRegistryResponse, RegistryHttpClient},
    },
    service::{ImportLensService, protocol_error_batch_response},
};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

static NEXT_TEMP_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

struct ResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<BatchResponse>,
}

struct CacheStatusResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<CacheStatusResponse>,
}

struct FileSizeResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<FileSizeResponse>,
}

impl FileSizeResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> FileSizeResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(
                    decode_payload::<FileSizeResponse>(&payload)
                        .expect("file-size response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}

impl CacheStatusResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> CacheStatusResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(
                    decode_payload::<CacheStatusResponse>(&payload)
                        .expect("cache status response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}

struct RegistryRefreshResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<RefreshRegistryHintsResponse>,
}

impl RegistryRefreshResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> RefreshRegistryHintsResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(
                    decode_payload::<RefreshRegistryHintsResponse>(&payload)
                        .expect("registry refresh response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}

struct DelayedRegistryClient;

impl RegistryHttpClient for DelayedRegistryClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        if package_name == "slow-lib" {
            std::thread::sleep(Duration::from_millis(300));
        }
        if package_name == "fail-lib" {
            return Err("simulated registry failure".to_owned());
        }
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{}},"time":{"2.0.0":"2026-01-01T00:00:00.000Z"}}"#.to_owned(),
        })
    }
}

impl ResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> BatchResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(
                    decode_payload::<BatchResponse>(&payload)
                        .expect("batch response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}

struct PackageJsonResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<AnalyzePackageJsonResponse>,
}

impl PackageJsonResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> AnalyzePackageJsonResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(
                    decode_payload::<AnalyzePackageJsonResponse>(&payload)
                        .expect("package.json response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}

fn temp_workspace() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let id = NEXT_TEMP_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();
    let path = std::env::temp_dir().join(format!("import-lens-server-{process_id}-{suffix}-{id}"));
    fs::create_dir_all(path.join("src")).expect("temp workspace should be created");
    path
}

fn write_tiny_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("tiny-stream-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn write_heavy_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("heavy-stream-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
    )
    .expect("package manifest should be written");

    let mut entry = String::new();
    for index in 0..8 {
        entry.push_str(&format!("import './payload-{index}.js';\n"));
        fs::write(
            package_root.join(format!("payload-{index}.js")),
            format!(
                "globalThis.__importLensPayload{index} = '{}';\n",
                "x".repeat(1024 * 1024)
            ),
        )
        .expect("payload module should be written");
    }
    entry.push_str("export const value = 1;\n");
    fs::write(package_root.join("index.js"), entry).expect("entry should be written");
}

fn hello(workspace: &Path) -> HelloMessage {
    HelloMessage {
        message_type: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        workspace_root: workspace.to_string_lossy().to_string(),
        storage_path: workspace.join(".import-lens").to_string_lossy().to_string(),
        enable_disk_cache: false,
        cache_max_size_mb: 512,
        registry_cache_max_size_mb: 32,
        log_level: "error".to_owned(),
    }
}

fn streaming_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    let active_document_path = workspace
        .join("src")
        .join("index.ts")
        .to_string_lossy()
        .to_string();

    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path,
        imports: vec![
            ImportRequest {
                specifier: "tiny-stream-lib".to_owned(),
                package_name: "tiny-stream-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["value".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
            ImportRequest {
                specifier: "heavy-stream-lib".to_owned(),
                package_name: "heavy-stream-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["value".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
        ],
        streaming: true,
    }
}

fn cache_warmup_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    let mut batch = streaming_batch(workspace, request_id);
    batch.imports.truncate(1);
    batch.streaming = false;
    batch
}

fn streaming_package_json(workspace: &Path, request_id: u64) -> AnalyzePackageJsonRequest {
    AnalyzePackageJsonRequest {
        message_type: "analyze_package_json".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
        source: r#"{
  "dependencies": { "tiny-stream-lib": "^1.0.0", "missing-stream-lib": "^1.0.0" }
}"#
        .to_owned(),
        include_registry_hints: false,
        force_registry_refresh: false,
        refresh_section: None,
        registry_hint_mode: None,
        streaming: true,
    }
}

fn cache_status(workspace: &Path, request_id: u64) -> CacheStatusRequest {
    CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: Some(workspace.to_string_lossy().to_string()),
    }
}

fn file_size(workspace: &Path, request_id: u64) -> FileSizeRequest {
    FileSizeRequest {
        message_type: "file_size".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "tiny-stream-lib".to_owned(),
            package_name: "tiny-stream-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
    }
}

async fn wait_for_generation_above(cancellation: &Arc<CancellationToken>, baseline: u64) {
    for _ in 0..20 {
        if cancellation.generation() > baseline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("prewarm generation should advance");
}

#[tokio::test]
async fn server_responds_to_cache_status_request() {
    let workspace = temp_workspace();
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = CacheStatusResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .write_all(&encode_frame(&cache_status(&workspace, 11)).expect("status should encode"))
        .await
        .expect("status should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 11);
    assert_eq!(response.version, PROTOCOL_VERSION);
    assert_eq!(response.error, None);

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn server_ignores_an_undecodable_frame_and_keeps_serving() {
    let workspace = temp_workspace();
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = CacheStatusResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    // A well-framed but undecodable payload (a corrupt frame, or an unknown
    // message type from a newer client) must be skipped, not tear down the
    // connection and discard warm cache + in-flight work.
    client_stream
        .write_all(&encode_frame(&0xDEAD_BEEF_u64).expect("garbage frame should encode"))
        .await
        .expect("garbage frame should be written");
    client_stream
        .write_all(&encode_frame(&cache_status(&workspace, 12)).expect("status should encode"))
        .await
        .expect("status should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 12);
    assert_eq!(response.error, None);

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn server_cancels_prewarm_before_file_size_requests() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let prefetcher = Prefetcher::new();
    let cancellation = Arc::clone(prefetcher.cancellation());
    let initial_generation = cancellation.generation();
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            prefetcher,
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = FileSizeResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    wait_for_generation_above(&cancellation, initial_generation).await;
    let prewarm_generation = cancellation.generation();

    client_stream
        .write_all(&encode_frame(&file_size(&workspace, 12)).expect("file size should encode"))
        .await
        .expect("file size should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 12);
    assert!(cancellation.generation() > prewarm_generation);

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

/// Reads raw framed payloads so one stream can carry two different response types in
/// sequence — the FileSizeDocument reply, then the unsolicited RefreshedResults push.
struct RawFrameReader {
    decoder: FrameDecoder,
    pending: VecDeque<Vec<u8>>,
}

impl RawFrameReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn next_payload(&mut self, stream: &mut DuplexStream) -> Vec<u8> {
        if let Some(payload) = self.pending.pop_front() {
            return payload;
        }
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing a frame");
            for payload in self
                .decoder
                .push(&buffer[..read])
                .expect("server frame should decode")
            {
                self.pending.push_back(payload);
            }
            if let Some(payload) = self.pending.pop_front() {
                return payload;
            }
        }
    }
}

fn file_size_document(
    workspace: &Path,
    document: &Path,
    source: &str,
    request_id: u64,
) -> FileSizeDocumentRequest {
    FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.to_string_lossy().to_string(),
        source: source.to_owned(),
        force_fresh: false,
        // Mirror the extension: tag the size read with the analysis generation so the
        // resulting SWR push echoes it back for the client's supersession guard.
        analysis_generation: Some(request_id),
    }
}

#[tokio::test]
async fn server_pushes_refreshed_results_after_serving_stale_size() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    let document = workspace.join("src").join("index.ts");
    let source = "import { value } from 'tiny-stream-lib';\nexport const total = value;\n";
    fs::write(&document, source).expect("document should be written");

    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = RawFrameReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");

    // First request populates the cache with a Fresh entry.
    client_stream
        .write_all(
            &encode_frame(&file_size_document(&workspace, &document, source, 1))
                .expect("request should encode"),
        )
        .await
        .expect("request should be written");
    let first: FileSizeDocumentResponse =
        decode_payload(&reader.next_payload(&mut client_stream).await)
            .expect("first response should decode");
    assert_eq!(first.request_id, 1);

    // Change the resolved dependency so the cached entry is stale, and bump the cache
    // generation so the next read takes the slow (re-validating) path.
    let package_index = workspace
        .join("node_modules")
        .join("tiny-stream-lib")
        .join("index.js");
    fs::write(&package_index, "export const value = 1234567890123456789;")
        .expect("dependency change should be written");
    import_lens_daemon::cache::memory::bump_cache_generation();

    // Second request serves the STALE value immediately, then pushes RefreshedResults.
    client_stream
        .write_all(
            &encode_frame(&file_size_document(&workspace, &document, source, 2))
                .expect("request should encode"),
        )
        .await
        .expect("request should be written");

    let second: FileSizeDocumentResponse =
        decode_payload(&reader.next_payload(&mut client_stream).await)
            .expect("second response should decode");
    assert_eq!(second.request_id, 2);
    assert!(
        second
            .imports
            .iter()
            .any(|result| matches!(result.freshness.kind, FreshnessKind::Stale)),
        "the immediate response serves the stale value flagged Stale"
    );

    // The unsolicited refreshed-results push arrives afterward with Fresh results.
    let refreshed: RefreshedResultsResponse = tokio::time::timeout(Duration::from_secs(5), async {
        decode_payload::<RefreshedResultsResponse>(&reader.next_payload(&mut client_stream).await)
            .expect("refreshed frame should decode")
    })
    .await
    .expect("a RefreshedResults push should arrive after the stale serve");
    assert_eq!(refreshed.message_type, "refreshed_results");
    assert_eq!(refreshed.document_path, document.to_string_lossy());
    assert!(
        !refreshed.results.is_empty(),
        "the refreshed push carries recomputed results"
    );
    assert!(
        refreshed
            .results
            .iter()
            .all(|result| matches!(result.freshness.kind, FreshnessKind::Fresh)),
        "recomputed results are Fresh"
    );
    // The push echoes the triggering size read's analysis generation (request_id 2)
    // so the client can drop it if a newer analysis has since superseded it.
    assert_eq!(
        refreshed.generation,
        Some(2),
        "the push echoes the triggering request's analysis generation"
    );
    // Each result is paired with a per-import identity so the client can disambiguate
    // same-specifier variants.
    assert_eq!(
        refreshed.identities.len(),
        refreshed.results.len(),
        "identities are index-aligned with results"
    );
    assert!(
        refreshed
            .identities
            .iter()
            .all(|identity| identity.specifier == "tiny-stream-lib"),
        "identities carry the import specifier: {:?}",
        refreshed.identities
    );

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn server_writes_streaming_partial_frame_before_final_response() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    write_heavy_package(&workspace);

    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = ResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .write_all(
            &encode_frame(&cache_warmup_batch(&workspace, 1))
                .expect("warmup request should encode"),
        )
        .await
        .expect("warmup should be written");
    let warmup = reader.read_response(&mut client_stream).await;
    assert_eq!(warmup.request_id, 1);
    assert_eq!(warmup.indexes, None);

    client_stream
        .write_all(
            &encode_frame(&streaming_batch(&workspace, 2))
                .expect("streaming request should encode"),
        )
        .await
        .expect("streaming request should be written");
    let first_partial = tokio::time::timeout(
        Duration::from_millis(200),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("cached import partial should arrive before the heavy import finishes");
    assert_eq!(first_partial.request_id, 2);
    assert_eq!(first_partial.indexes, Some(vec![0]));
    assert_eq!(first_partial.imports.len(), 1);

    let early_final = tokio::time::timeout(
        Duration::from_millis(20),
        reader.read_response(&mut client_stream),
    )
    .await;
    assert!(
        early_final.is_err(),
        "final response should not be buffered with the first partial",
    );

    let second_partial = tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("heavy import partial should arrive");
    assert_eq!(second_partial.indexes, Some(vec![1]));

    let final_response = tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("final response should arrive");
    assert_eq!(final_response.indexes, None);
    assert_eq!(final_response.imports.len(), 2);

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn server_writes_package_json_partial_frame_before_final_response() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);

    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = PackageJsonResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .write_all(
            &encode_frame(&streaming_package_json(&workspace, 6))
                .expect("package.json request should encode"),
        )
        .await
        .expect("package.json request should be written");

    let first_partial = tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("package.json loading partial should arrive before final response");
    assert_eq!(first_partial.request_id, 6);
    assert_eq!(first_partial.indexes, Some(vec![0, 1]));
    assert!(
        first_partial.states.iter().any(|state| {
            state.name == "tiny-stream-lib" && state.status == ImportAnalysisStatus::Loading
        }),
        "{first_partial:?}",
    );

    let final_response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = reader.read_response(&mut client_stream).await;
            if response.indexes.is_none() {
                return response;
            }
        }
    })
    .await
    .expect("final package.json response should arrive");
    assert_eq!(final_response.indexes, None);
    assert_eq!(final_response.states.len(), 2);

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn unsupported_hello_version_closes_connection_without_accepting_requests() {
    let workspace = temp_workspace();
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut unsupported_hello = hello(&workspace);
    unsupported_hello.version = PROTOCOL_VERSION + 1;
    let mut frames = encode_frame(&unsupported_hello).expect("hello should encode");
    frames.extend(encode_frame(&cache_warmup_batch(&workspace, 3)).expect("batch should encode"));

    client_stream
        .write_all(&frames)
        .await
        .expect("client frames should be written");
    let mut buffer = [0_u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(1), client_stream.read(&mut buffer))
        .await
        .expect("connection should close")
        .expect("client read should complete");

    assert_eq!(read, 0);
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn spawn_blocking_join_error_returns_protocol_batch_error() {
    let workspace = temp_workspace();
    let request = cache_warmup_batch(&workspace, 4);
    let response = response_from_join(
        tokio::task::spawn_blocking(|| -> BatchResponse {
            panic!("analysis worker panic");
        }),
        &request,
        protocol_error_batch_response,
    )
    .await;

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 4);
    assert_eq!(response.indexes, None);
    assert_eq!(response.imports.len(), 1);
    assert!(
        response.imports[0]
            .error
            .as_deref()
            .is_some_and(|message| message.contains("analysis worker failed")),
        "{response:?}",
    );
}

#[tokio::test]
async fn server_streams_registry_hint_partials_before_final_response() {
    let workspace = temp_workspace();
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let registry_hints = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(DelayedRegistryClient),
    );
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new_with_registry_hints_for_tests(
                registry_hints,
            )),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = RegistryRefreshResponseReader::new();

    client_stream
        .write_all(&encode_frame(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .write_all(
            &encode_frame(&RefreshRegistryHintsRequest {
                message_type: "refresh_registry_hints".to_owned(),
                version: PROTOCOL_VERSION,
                request_id: 8,
                targets: vec![
                    RegistryHintTarget {
                        name: "fast-lib".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                    RegistryHintTarget {
                        name: "slow-lib".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                    RegistryHintTarget {
                        name: "fail-lib".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                ],
                mode: RegistryHintMode::RefreshStale,
            })
            .expect("registry refresh request should encode"),
        )
        .await
        .expect("request should be written");

    let first_partial = tokio::time::timeout(
        Duration::from_millis(200),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("first registry partial should arrive before the slow package finishes");
    assert_eq!(first_partial.request_id, 8);
    assert!(first_partial.indexes.is_some());
    assert_eq!(first_partial.results.len(), 1);

    // This 20ms probe only proves the final response is not buffered with the
    // first partial because the two remaining targets are still in flight at
    // that point: `slow-lib` sleeps an explicit 300ms in
    // `DelayedRegistryClient`, and `fail-lib` errors on every attempt, so its
    // MAX_ATTEMPTS(3) fetch spends ~REGISTRY_RETRY_BASE_DELAY_MS(100) * (1+2)
    // = ~300ms in retry backoff. If those registry constants (in
    // `daemon/src/registry/constants.rs`) or the client's sleep shrink below
    // this probe window, the final response may legitimately arrive early and
    // this assertion will flake.
    let early_final = tokio::time::timeout(
        Duration::from_millis(20),
        reader.read_response(&mut client_stream),
    )
    .await;
    assert!(
        early_final.is_err(),
        "final response should not be buffered with the first partial"
    );

    let final_response = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = reader.read_response(&mut client_stream).await;
            if response.indexes.is_none() {
                return response;
            }
        }
    })
    .await
    .expect("final registry refresh response should arrive");
    assert_eq!(final_response.results.len(), 3);
    assert!(
        final_response
            .results
            .iter()
            .any(|result| result.target.name == "fail-lib" && result.error.is_some())
    );
    assert!(
        final_response
            .results
            .iter()
            .any(|result| result.target.name == "fast-lib" && result.hint.is_some())
    );

    client_stream
        .write_all(
            &encode_frame(&ShutdownMessage {
                message_type: "shutdown".to_owned(),
            })
            .expect("shutdown should encode"),
        )
        .await
        .expect("shutdown should be written");
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}
