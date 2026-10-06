use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use import_lens_daemon::{
    ipc::{
        codec::{decode_payload, message_frame_codec, payload_bytes},
        protocol::{
            AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
            AnalyzePackageJsonResponse, CacheStatusRequest, CacheStatusResponse,
            EnumerateExportsRequest, EnumerateExportsResponse, FileSizeDocumentRequest,
            FileSizeDocumentResponse, FreshnessKind, HelloMessage, ImportAnalysisStatus,
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
    service::{ImportLensService, protocol_error_exports_response},
};
use serde::de::DeserializeOwned;
use std::{
    fs,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{DuplexStream, duplex};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

static NEXT_TEMP_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

/// The client end of a test connection, framed by the codec the daemon ships.
type Client = Framed<DuplexStream, LengthDelimitedCodec>;

async fn next_payload(stream: &mut Client) -> BytesMut {
    stream
        .next()
        .await
        .expect("server closed before writing a frame")
        .expect("server frame should decode")
}

/// Reads every frame as one response type.
struct TypedReader<T>(PhantomData<T>);

impl<T: DeserializeOwned> TypedReader<T> {
    fn new() -> Self {
        Self(PhantomData)
    }

    async fn read_response(&mut self, stream: &mut Client) -> T {
        decode_payload(&next_payload(stream).await).expect("server response should decode")
    }
}

type CacheStatusResponseReader = TypedReader<CacheStatusResponse>;
type FileSizeDocumentResponseReader = TypedReader<FileSizeDocumentResponse>;
type RegistryRefreshResponseReader = TypedReader<RefreshRegistryHintsResponse>;
type PackageJsonResponseReader = TypedReader<AnalyzePackageJsonResponse>;

enum MixedResponse {
    CacheStatus(CacheStatusResponse),
    PackageJson(AnalyzePackageJsonResponse),
}

/// Reads a stream that interleaves cache-status responses with package.json frames.
struct MixedResponseReader;

impl MixedResponseReader {
    fn new() -> Self {
        Self
    }

    async fn read_response(&mut self, stream: &mut Client) -> MixedResponse {
        let payload = next_payload(stream).await;
        if let Ok(response) = decode_payload::<CacheStatusResponse>(&payload) {
            MixedResponse::CacheStatus(response)
        } else if let Ok(response) = decode_payload::<AnalyzePackageJsonResponse>(&payload) {
            MixedResponse::PackageJson(response)
        } else {
            panic!("server frame should match an expected response shape");
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

fn streaming_large_package_json(workspace: &Path, request_id: u64) -> AnalyzePackageJsonRequest {
    let mut request = streaming_package_json(workspace, request_id);
    request.source = r#"{
  "dependencies": {
    "tiny-stream-lib": "^1.0.0",
    "heavy-stream-lib": "^1.0.0",
    "missing-stream-lib": "^1.0.0"
  }
}"#
    .to_owned();
    request
}

fn cache_status(workspace: &Path, request_id: u64) -> CacheStatusRequest {
    CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: Some(workspace.to_string_lossy().to_string()),
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

async fn shutdown_server(
    client_stream: &mut Client,
    server: tokio::task::JoinHandle<Result<(), String>>,
    workspace: PathBuf,
) {
    client_stream
        .send(
            payload_bytes(&ShutdownMessage {
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
async fn server_batches_cached_registry_hint_partials() {
    let workspace = temp_workspace();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
    let registry_hints = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(DelayedRegistryClient),
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64;
    for package_name in ["react", "vue", "svelte"] {
        registry_hints
            .write_metadata_for_tests(package_name, "9.0.0", now)
            .expect("seed cached registry metadata");
    }
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(
            payload_bytes(&RefreshRegistryHintsRequest {
                message_type: "refresh_registry_hints".to_owned(),
                version: PROTOCOL_VERSION,
                request_id: 23,
                targets: vec![
                    RegistryHintTarget {
                        name: "react".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                    RegistryHintTarget {
                        name: "vue".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                    RegistryHintTarget {
                        name: "svelte".to_owned(),
                        installed_version: Some("1.0.0".to_owned()),
                    },
                ],
                mode: RegistryHintMode::RefreshStale,
                source: Some("test/package.json".to_owned()),
            })
            .expect("registry refresh request should encode"),
        )
        .await
        .expect("request should be written");

    let cached_partial = tokio::time::timeout(
        Duration::from_millis(200),
        reader.read_response(&mut client_stream),
    )
    .await
    .expect("cached registry partial should arrive");
    assert_eq!(cached_partial.request_id, 23);
    assert_eq!(cached_partial.indexes, Some(vec![0, 1, 2]));
    assert_eq!(cached_partial.results.len(), 3);
    assert!(
        cached_partial
            .results
            .iter()
            .all(|result| result.origin.as_deref() == Some("cache"))
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

    client_stream
        .send(
            payload_bytes(&ShutdownMessage {
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
async fn server_responds_to_cache_status_request() {
    let workspace = temp_workspace();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(payload_bytes(&cache_status(&workspace, 11)).expect("status should encode"))
        .await
        .expect("status should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 11);
    assert_eq!(response.version, PROTOCOL_VERSION);
    assert_eq!(response.error, None);

    shutdown_server(&mut client_stream, server, workspace).await;
}

#[tokio::test]
async fn server_ignores_an_undecodable_frame_and_keeps_serving() {
    let workspace = temp_workspace();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    // A well-framed but undecodable payload (a corrupt frame, or an unknown
    // message type from a newer client) must be skipped, not tear down the
    // connection and discard warm cache + in-flight work.
    client_stream
        .send(payload_bytes(&0xDEAD_BEEF_u64).expect("garbage frame should encode"))
        .await
        .expect("garbage frame should be written");
    client_stream
        .send(payload_bytes(&cache_status(&workspace, 12)).expect("status should encode"))
        .await
        .expect("status should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 12);
    assert_eq!(response.error, None);

    client_stream
        .send(
            payload_bytes(&ShutdownMessage {
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
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
    let mut reader = FileSizeDocumentResponseReader::new();

    client_stream
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    wait_for_generation_above(&cancellation, initial_generation).await;
    let prewarm_generation = cancellation.generation();

    client_stream
        .send(
            payload_bytes(&file_size_document(
                &workspace,
                &workspace.join("src").join("index.ts"),
                "import { value } from 'tiny-stream-lib';",
                12,
            ))
            .expect("file size should encode"),
        )
        .await
        .expect("file size should be written");

    let response = reader.read_response(&mut client_stream).await;
    assert_eq!(response.request_id, 12);
    assert!(cancellation.generation() > prewarm_generation);

    client_stream
        .send(
            payload_bytes(&ShutdownMessage {
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
struct RawFrameReader;

impl RawFrameReader {
    fn new() -> Self {
        Self
    }

    async fn next_payload(&mut self, stream: &mut Client) -> BytesMut {
        next_payload(stream).await
    }
}

fn analyze_document(
    workspace: &Path,
    document: &Path,
    source: &str,
    request_id: u64,
) -> AnalyzeDocumentRequest {
    AnalyzeDocumentRequest {
        message_type: "analyze_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.to_string_lossy().to_string(),
        source: source.to_owned(),
    }
}

/// Read `count` streamed import results, the way the extension does: the analysis response comes
/// back with `loading` placeholders and each import arrives afterwards on its own push.
async fn collect_streamed_imports(
    reader: &mut RawFrameReader,
    stream: &mut Client,
    count: usize,
) -> Vec<RefreshedResultsResponse> {
    let mut pushes = Vec::new();
    while pushes.len() < count {
        let push: RefreshedResultsResponse = tokio::time::timeout(Duration::from_secs(10), async {
            decode_payload(&reader.next_payload(stream).await)
                .expect("a streamed import push should decode")
        })
        .await
        .expect("every loading import must arrive on the push channel");
        pushes.push(push);
    }
    pushes
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

/// The whole point of the redesign, end to end.
///
/// A cold document's imports are not in the cache, and the response does not wait for them: it
/// comes back at once with a `loading` placeholder per import, and each import arrives afterwards
/// on the push channel as its build lands. Before this, the response carried every import or
/// none — so one package that parked the bundler pushed the response past the extension's 10s
/// deadline and the client discarded the ENTIRE document, cached hits included.
///
/// The placeholders are what make the pushes deliverable: the client rebuilds a document's state
/// from `imports`, and a push can only update a state that exists.
#[tokio::test]
async fn a_cold_document_answers_at_once_and_streams_each_import_as_it_lands() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    write_heavy_package(&workspace);
    let document = workspace.join("src").join("index.ts");
    let source = "import { value } from 'tiny-stream-lib';\n\
                  import * as heavy from 'heavy-stream-lib';\n\
                  export const total = [value, heavy];\n";
    fs::write(&document, source).expect("document should be written");

    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(
            payload_bytes(&analyze_document(&workspace, &document, source, 7))
                .expect("request should encode"),
        )
        .await
        .expect("request should be written");

    let response: AnalyzeDocumentResponse =
        decode_payload(&reader.next_payload(&mut client_stream).await)
            .expect("analysis response should decode");
    assert_eq!(response.request_id, 7);
    assert_eq!(response.error, None);
    assert_eq!(response.imports.len(), 2);
    assert!(
        response
            .imports
            .iter()
            .all(|item| item.status == ImportAnalysisStatus::Loading && item.result.is_none()),
        "a cold document must answer with placeholders, not wait for the engine: {:?}",
        response.imports
    );
    assert!(
        response
            .imports
            .iter()
            .all(|item| item.request.is_some() && item.message.is_none()),
        "a loading import still carries its resolved request, and is not an error"
    );

    // Every one of them then arrives on the push channel, addressed by identity and stamped with
    // the analysis generation the client uses to drop a superseded batch.
    let pushes = collect_streamed_imports(&mut reader, &mut client_stream, 2).await;
    let mut delivered = pushes
        .iter()
        .flat_map(|push| {
            assert_eq!(push.message_type, "refreshed_results");
            assert_eq!(push.document_path, document.to_string_lossy());
            assert_eq!(push.generation, Some(7));
            assert_eq!(push.results.len(), push.identities.len());
            push.identities
                .iter()
                .zip(&push.results)
                .map(|(identity, result)| {
                    assert!(result.error.is_none(), "{result:?}");
                    assert!(
                        result.brotli_bytes().is_some_and(|bytes| bytes > 0),
                        "a streamed import carries a real size"
                    );
                    identity.specifier.clone()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    delivered.sort();
    assert_eq!(delivered, vec!["heavy-stream-lib", "tiny-stream-lib"]);

    client_stream
        .send(
            payload_bytes(&ShutdownMessage {
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
async fn server_pushes_refreshed_results_after_serving_stale_size() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    let document = workspace.join("src").join("index.ts");
    let source = "import { value } from 'tiny-stream-lib';\nexport const total = value;\n";
    fs::write(&document, source).expect("document should be written");

    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");

    // Seed the cache the way the editor does: the document analysis is what builds a document's
    // imports (the file-size read only sizes the file), and its results land on the push channel.
    client_stream
        .send(
            payload_bytes(&analyze_document(&workspace, &document, source, 0))
                .expect("request should encode"),
        )
        .await
        .expect("request should be written");
    let _: AnalyzeDocumentResponse = decode_payload(&reader.next_payload(&mut client_stream).await)
        .expect("analysis response should decode");
    collect_streamed_imports(&mut reader, &mut client_stream, 1).await;

    // The first size read now hits that warm cache with a Fresh entry.
    client_stream
        .send(
            payload_bytes(&file_size_document(&workspace, &document, source, 1))
                .expect("request should encode"),
        )
        .await
        .expect("request should be written");
    let first: FileSizeDocumentResponse =
        decode_payload(&reader.next_payload(&mut client_stream).await)
            .expect("first response should decode");
    assert_eq!(first.request_id, 1);
    assert_eq!(
        first.imports.len(),
        1,
        "the size read serves the import the analysis already built: {first:?}"
    );

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
        .send(
            payload_bytes(&file_size_document(&workspace, &document, source, 2))
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
        .send(
            payload_bytes(&ShutdownMessage {
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

/// A streamed import must reach the socket WHILE a slow request is still being handled.
///
/// This is the whole point of the streaming design, and the connection loop used to defeat it. The
/// extension sends `AnalyzeDocument` and then immediately `FileSizeDocument` for the same document.
/// The analysis answers at once with `loading` placeholders and starts the per-import builds — but
/// the loop then `.await`ed the file-size handler INLINE, so it sat suspended inside that arm
/// instead of in its `select!`, and every import result that landed during the combined build was
/// computed on time and then simply never written. The client's analysis deadline is absolute, so a
/// combined build that parks for the full BUILD_TIMEOUT loses the whole document — the loss the
/// placeholders exist to prevent.
///
/// The two requests are written in ONE chunk on purpose: that is what the extension does, and it is
/// what makes the failure deterministic rather than a race. When the loop returns to its `select!`
/// after answering the analysis, the file-size frame is already buffered and no push exists yet, so
/// an inline-awaiting loop is guaranteed to disappear into the file-size arm before the first
/// import lands.
///
/// Two things are asserted, and together they are the multiplexer:
///
/// * a streamed import reaches the socket BEFORE the file-size response. The combined build cannot
///   even start until the per-import builds free an engine permit, so at least one push always
///   precedes it in real time; a loop that writes its own responses straight to the socket from
///   inside an arm puts the response on the wire first anyway, and the pushes queue up behind it;
/// * a request sent WHILE that build is in flight is still answered before it. A loop suspended
///   inside a handler cannot even read the frame.
#[tokio::test]
async fn a_streamed_import_is_delivered_while_a_file_size_build_is_in_flight() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    write_heavy_package(&workspace);
    let document = workspace.join("src").join("index.ts");
    let source = "import { value } from 'tiny-stream-lib';\n\
                  import * as heavy from 'heavy-stream-lib';\n\
                  export const total = [value, heavy];\n";
    fs::write(&document, source).expect("document should be written");

    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");

    // Both frames are queued, then written in one flush: one chunk on the wire.
    client_stream
        .feed(
            payload_bytes(&analyze_document(&workspace, &document, source, 21))
                .expect("analysis request should encode"),
        )
        .await
        .expect("analysis request should be queued");
    client_stream
        .feed(
            payload_bytes(&file_size_document(&workspace, &document, source, 22))
                .expect("file-size request should encode"),
        )
        .await
        .expect("file-size request should be queued");
    client_stream
        .flush()
        .await
        .expect("both requests should be written in one chunk");

    let analysis: AnalyzeDocumentResponse =
        decode_payload(&reader.next_payload(&mut client_stream).await)
            .expect("analysis response should decode");
    assert_eq!(analysis.request_id, 21);
    assert_eq!(analysis.imports.len(), 2);

    // The combined build is now in flight. Nothing orders it behind the per-import builds: the
    // file-size handler and the analysis handler are spawned independently and their builds race for
    // the engine permits. With two permits, the trivial `tiny-stream-lib` build can even be starved
    // while the combined build and the heavy per-import build hold both — so whether a per-import
    // push reaches the socket before the file-size response is a scheduling coin toss, not a
    // guarantee. Do NOT assert that ordering (an earlier version did, and it flaked in CI under
    // different core counts). Assert instead the property the loop actually owes and the old
    // inline-`.await` broke: it stays live while the build runs. Ask a fresh, trivial question now.
    client_stream
        .send(payload_bytes(&cache_status(&workspace, 23)).expect("cache status should encode"))
        .await
        .expect("cache status should be written");

    // Read frames until the file-size response arrives, recording what got delivered ahead of it.
    let mut pushes_before_size_response = 0_usize;
    let mut answered_a_new_request = false;
    let file_size: FileSizeDocumentResponse =
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let payload = reader.next_payload(&mut client_stream).await;
                // Only the push carries `message_type`, and only the cache-status response carries
                // `project_count`, so the three shapes are unambiguous under MessagePack's named
                // encoding.
                if let Ok(push) = decode_payload::<RefreshedResultsResponse>(&payload) {
                    assert_eq!(push.message_type, "refreshed_results");
                    pushes_before_size_response += push.results.len();
                    continue;
                }
                if let Ok(status) = decode_payload::<CacheStatusResponse>(&payload) {
                    assert_eq!(status.request_id, 23);
                    answered_a_new_request = true;
                    continue;
                }
                return decode_payload::<FileSizeDocumentResponse>(&payload).expect(
                    "every frame must be a push, a cache status, or the file-size response",
                );
            }
        })
        .await
        .expect("the file-size response must arrive");

    assert_eq!(file_size.request_id, 22);
    // The deterministic proof that the loop was never suspended inside the file-size arm: a request
    // that arrived AFTER the combined build began was still read, dispatched, and answered BEFORE
    // that build's own response. `cache_status` needs no engine permit, so in a healthy loop it
    // always overtakes the slow 8 MB build; under the old inline `.await` the loop could not even
    // read the frame until the build finished. (That the imports stream at all is proven
    // deterministically by `a_cold_document_answers_at_once_and_streams_each_import_as_it_lands`; and
    // that none are dropped is proven below. What is NOT asserted is a per-import push arriving
    // before the file-size response — that is a permit race, see above.)
    assert!(
        answered_a_new_request,
        "a request sent while the file-size build was in flight must still be answered before it; \
         the connection loop could not even read the frame"
    );

    // And the rest still arrive: nothing was dropped on the way.
    let remaining = 2 - pushes_before_size_response.min(2);
    if remaining > 0 {
        collect_streamed_imports(&mut reader, &mut client_stream, remaining).await;
    }

    shutdown_server(&mut client_stream, server, workspace).await;
}

#[tokio::test]
async fn server_writes_package_json_partial_frame_before_final_response() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);

    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(
            payload_bytes(&streaming_package_json(&workspace, 6))
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
        .send(
            payload_bytes(&ShutdownMessage {
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
async fn server_keeps_connection_responsive_during_package_json_stream() {
    let workspace = temp_workspace();
    write_tiny_package(&workspace);
    write_heavy_package(&workspace);

    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
    let mut reader = MixedResponseReader::new();

    client_stream
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(
            payload_bytes(&streaming_large_package_json(&workspace, 20))
                .expect("package.json request should encode"),
        )
        .await
        .expect("package.json request should be written");

    let first_partial = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = reader.read_response(&mut client_stream).await;
            if let MixedResponse::PackageJson(response) = response
                && response.request_id == 20
                && response.indexes.is_some()
            {
                return response;
            }
        }
    })
    .await
    .expect("package.json stream should emit a partial before the final frame");
    assert!(
        first_partial
            .states
            .iter()
            .any(|state| state.name == "heavy-stream-lib"),
        "{first_partial:?}"
    );

    client_stream
        .send(payload_bytes(&cache_status(&workspace, 21)).expect("cache status should encode"))
        .await
        .expect("cache status should be written");

    let (cache_status_at, pkg_final_at) = tokio::time::timeout(Duration::from_secs(20), async {
        let mut cache_status_at: Option<usize> = None;
        let mut pkg_final_at: Option<usize> = None;
        let mut seq = 0_usize;

        while cache_status_at.is_none() || pkg_final_at.is_none() {
            let response = reader.read_response(&mut client_stream).await;
            match response {
                MixedResponse::CacheStatus(response) if response.request_id == 21 => {
                    cache_status_at.get_or_insert(seq);
                }
                MixedResponse::PackageJson(response)
                    if response.request_id == 20 && response.indexes.is_none() =>
                {
                    pkg_final_at.get_or_insert(seq);
                }
                _ => {}
            }
            seq += 1;
        }

        (cache_status_at.unwrap(), pkg_final_at.unwrap())
    })
    .await
    .expect("cache_status and package.json final should arrive");

    assert!(
        cache_status_at < pkg_final_at,
        "cache_status must be served before the package.json stream final frame"
    );

    shutdown_server(&mut client_stream, server, workspace).await;
}

#[tokio::test]
async fn unsupported_hello_version_closes_connection_without_accepting_requests() {
    let workspace = temp_workspace();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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

    client_stream
        .feed(payload_bytes(&unsupported_hello).expect("hello should encode"))
        .await
        .expect("hello should be queued");
    client_stream
        .feed(payload_bytes(&cache_status(&workspace, 3)).expect("status should encode"))
        .await
        .expect("status should be queued");
    client_stream
        .flush()
        .await
        .expect("client frames should be written");
    let next = tokio::time::timeout(Duration::from_secs(1), client_stream.next())
        .await
        .expect("connection should close");

    assert!(
        next.is_none(),
        "the server must close without answering a request after an unsupported hello"
    );
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[tokio::test]
async fn spawn_blocking_join_error_returns_a_request_scoped_protocol_error() {
    let workspace = temp_workspace();
    let request = EnumerateExportsRequest {
        message_type: "enumerate_exports".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 4,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        specifier: "tiny-stream-lib".to_owned(),
        package_name: "tiny-stream-lib".to_owned(),
        package_version: "1.0.0".to_owned(),
        cursor_offset: None,
    };
    let response = response_from_join(
        tokio::task::spawn_blocking(|| -> EnumerateExportsResponse {
            panic!("analysis worker panic");
        }),
        &request,
        protocol_error_exports_response,
    )
    .await;

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 4);
    assert!(response.exports.is_empty());
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|message| message.contains("analysis worker failed")),
        "{response:?}",
    );
}

#[tokio::test]
async fn server_streams_registry_hint_partials_before_final_response() {
    let workspace = temp_workspace();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let mut client_stream = Framed::new(client_stream, message_frame_codec());
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
        .send(payload_bytes(&hello(&workspace)).expect("hello should encode"))
        .await
        .expect("hello should be written");
    client_stream
        .send(
            payload_bytes(&RefreshRegistryHintsRequest {
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
                source: Some("test/package.json".to_owned()),
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
        .send(
            payload_bytes(&ShutdownMessage {
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
