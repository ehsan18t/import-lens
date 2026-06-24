use crate::{
    ipc::{
        codec::{FrameDecoder, decode_payload, encode_frame},
        protocol::{
            AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
            AnalyzePackageJsonResponse, AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse,
            BatchRequest, BatchResponse, ClientMessage, CompleteImportMembersRequest,
            CompleteImportMembersResponse, EnumerateExportsRequest, EnumerateExportsResponse,
            FileSizeDocumentRequest, FileSizeDocumentResponse, FileSizeRequest, FileSizeResponse,
            ImportDiagnostic, PROTOCOL_VERSION,
        },
    },
    lifecycle::{LifecycleState, record_recycle_timestamp},
    logging::{self, parse_log_level, set_log_level},
    prefetch::Prefetcher,
    service::{
        ImportLensService, protocol_error_batch_response, protocol_error_exports_response,
        protocol_error_file_size_response,
    },
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const LIFECYCLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

#[cfg(windows)]
use tokio::net::windows::named_pipe::ServerOptions;

#[cfg(windows)]
pub async fn run_server(
    pipe_name: &str,
    _workspace_root: PathBuf,
    storage_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe_name)?;
    pipe.connect().await?;

    let service = std::sync::Arc::new(ImportLensService::new(None, false));
    let prefetcher = Prefetcher::new();

    handle_connection(pipe, storage_path, service, prefetcher).await
}

#[cfg(not(windows))]
pub async fn run_server(
    pipe_name: &str,
    _workspace_root: PathBuf,
    storage_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    use tokio::net::UnixListener;

    if std::fs::metadata(pipe_name).is_ok() {
        std::fs::remove_file(pipe_name)?;
    }

    let listener = UnixListener::bind(pipe_name)?;
    restrict_unix_socket_permissions(pipe_name)?;

    let service = std::sync::Arc::new(ImportLensService::new(None, false));
    let prefetcher = Prefetcher::new();

    let result = async {
        let (stream, _) = listener.accept().await?;
        handle_connection(stream, storage_path, service, prefetcher).await
    }
    .await;

    if let Err(error) = std::fs::remove_file(pipe_name) {
        logging::log_warn(
            "ipc",
            format!("failed to remove IPC socket {pipe_name}: {error}"),
        );
    }

    result
}

async fn handle_connection<S>(
    mut stream: S,
    storage_path: Option<PathBuf>,
    mut service: std::sync::Arc<ImportLensService>,
    prefetcher: Prefetcher,
) -> Result<(), Box<dyn Error>>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut decoder = FrameDecoder::default();
    let mut hello_received = false;
    let mut lifecycle = LifecycleState::new();
    let mut storage_path = storage_path;
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let read = tokio::select! {
            read = stream.read(&mut buffer) => read?,
            _ = tokio::time::sleep(LIFECYCLE_CHECK_INTERVAL) => {
                if recycle_if_needed(
                    &lifecycle,
                    service.cache_len(),
                    storage_path.as_deref(),
                    &prefetcher,
                    &service,
                ) {
                    return Ok(());
                }
                continue;
            }
        };

        if read == 0 {
            break;
        }

        for payload in decoder.push(&buffer[..read])? {
            let message = decode_payload::<ClientMessage>(&payload)?;

            match message {
                ClientMessage::Hello(hello) => {
                    if !is_supported_hello_version(hello.version) {
                        logging::log_warn(
                            "ipc",
                            format!("unsupported hello protocol version {}", hello.version),
                        );
                        return Ok(());
                    }

                    set_log_level(parse_log_level(&hello.log_level));
                    logging::log_info(
                        "ipc",
                        format!(
                            "hello accepted (protocol v{}, disk_cache={})",
                            hello.version, hello.enable_disk_cache
                        ),
                    );

                    let hello_storage_path = PathBuf::from(&hello.storage_path);
                    let hello_workspace_root = PathBuf::from(&hello.workspace_root);
                    service = std::sync::Arc::new(ImportLensService::new(
                        Some(hello_storage_path.clone()),
                        hello.enable_disk_cache,
                    ));
                    storage_path = Some(hello_storage_path);
                    hello_received = true;
                    prefetcher.prewarm_recent_cache_entries(
                        std::sync::Arc::clone(&service),
                        hello_workspace_root,
                    );

                    if recycle_if_needed(
                        &lifecycle,
                        service.cache_len(),
                        storage_path.as_deref(),
                        &prefetcher,
                        &service,
                    ) {
                        return Ok(());
                    }
                }
                ClientMessage::Batch(request) if hello_received => {
                    prefetcher.cancel();
                    lifecycle.record_batch();
                    let svc = std::sync::Arc::clone(&service);
                    if request.version >= 2 && request.streaming {
                        let request_for_error = request.clone();
                        let (partial_tx, mut partial_rx) = mpsc::unbounded_channel();
                        let response_handle = tokio::task::spawn_blocking(move || {
                            svc.handle_batch_streaming(request, move |partial| {
                                let _ = partial_tx.send(partial);
                            })
                        });
                        while let Some(response) = partial_rx.recv().await {
                            stream.write_all(&encode_frame(&response)?).await?;
                        }
                        let response =
                            batch_response_from_join(response_handle, &request_for_error).await;
                        stream.write_all(&encode_frame(&response)?).await?;
                    } else {
                        let request_for_error = request.clone();
                        let response_handle =
                            tokio::task::spawn_blocking(move || svc.handle_batch(request));
                        let response =
                            batch_response_from_join(response_handle, &request_for_error).await;
                        stream.write_all(&encode_frame(&response)?).await?;
                    }

                    if recycle_if_needed(
                        &lifecycle,
                        service.cache_len(),
                        storage_path.as_deref(),
                        &prefetcher,
                        &service,
                    ) {
                        return Ok(());
                    }
                }
                ClientMessage::Batch(request) => {
                    let response = protocol_error_batch_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzeDocument(request) if hello_received => {
                    prefetcher.cancel();
                    lifecycle.record_batch();
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.handle_analyze_document(request));
                    let response =
                        analyze_document_response_from_join(response_handle, &request_for_error)
                            .await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzeDocument(request) => {
                    let response = protocol_error_analyze_document_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzePackageJson(request) if hello_received => {
                    prefetcher.cancel();
                    lifecycle.record_batch();
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle = tokio::task::spawn_blocking(move || {
                        svc.handle_analyze_package_json(request)
                    });
                    let response = analyze_package_json_response_from_join(
                        response_handle,
                        &request_for_error,
                    )
                    .await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzePackageJson(request) => {
                    let response = protocol_error_analyze_package_json_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzeSpecifiers(request) if hello_received => {
                    prefetcher.cancel();
                    lifecycle.record_batch();
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.handle_analyze_specifiers(request));
                    let response =
                        analyze_specifiers_response_from_join(response_handle, &request_for_error)
                            .await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::AnalyzeSpecifiers(request) => {
                    let response = protocol_error_analyze_specifiers_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::CacheInvalidate(message) if hello_received => {
                    service.invalidate_package(&message.package_name);
                }
                ClientMessage::CacheInvalidateAll(_) if hello_received => {
                    service.invalidate_all();
                }
                ClientMessage::PrewarmPackageJson(message) if hello_received => {
                    prefetcher.prewarm_package_json(
                        std::sync::Arc::clone(&service),
                        PathBuf::from(message.package_json_path),
                        PathBuf::from(message.active_document_path),
                    );
                }
                ClientMessage::NodeModulesChanged(message) if hello_received => {
                    if service.invalidate_package_json_paths(&message.package_json_paths) {
                        prefetcher.cancel();
                    }
                }
                ClientMessage::EnumerateExports(request) if hello_received => {
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.enumerate_exports(request));
                    let response =
                        exports_response_from_join(response_handle, &request_for_error).await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::EnumerateExports(request) => {
                    let response = protocol_error_exports_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::FileSize(request) if hello_received => {
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.handle_file_size(request));
                    let response =
                        file_size_response_from_join(response_handle, &request_for_error).await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::FileSize(request) => {
                    let response = protocol_error_file_size_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::FileSizeDocument(request) if hello_received => {
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.handle_file_size_document(request));
                    let response =
                        file_size_document_response_from_join(response_handle, &request_for_error)
                            .await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::FileSizeDocument(request) => {
                    let response = protocol_error_file_size_document_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::CompleteImportMembers(request) if hello_received => {
                    let svc = std::sync::Arc::clone(&service);
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.complete_import_members(request));
                    let response = complete_import_members_response_from_join(
                        response_handle,
                        &request_for_error,
                    )
                    .await;
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::CompleteImportMembers(request) => {
                    let response = protocol_error_complete_import_members_response(
                        &request,
                        "hello message not received".to_owned(),
                    );
                    stream.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::Shutdown(_) => {
                    return Ok(());
                }
                ClientMessage::PrewarmPackageJson(_)
                | ClientMessage::NodeModulesChanged(_)
                | ClientMessage::CacheInvalidate(_)
                | ClientMessage::CacheInvalidateAll(_) => {}
            }
        }
    }

    Ok(())
}

async fn batch_response_from_join(
    response_handle: JoinHandle<BatchResponse>,
    request: &BatchRequest,
) -> BatchResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => protocol_error_batch_response(request, join_error_message(error)),
    }
}

async fn exports_response_from_join(
    response_handle: JoinHandle<EnumerateExportsResponse>,
    request: &EnumerateExportsRequest,
) -> EnumerateExportsResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => protocol_error_exports_response(request, join_error_message(error)),
    }
}

async fn file_size_response_from_join(
    response_handle: JoinHandle<FileSizeResponse>,
    request: &FileSizeRequest,
) -> FileSizeResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => protocol_error_file_size_response(request, join_error_message(error)),
    }
}

async fn analyze_document_response_from_join(
    response_handle: JoinHandle<AnalyzeDocumentResponse>,
    request: &AnalyzeDocumentRequest,
) -> AnalyzeDocumentResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => protocol_error_analyze_document_response(request, join_error_message(error)),
    }
}

async fn analyze_package_json_response_from_join(
    response_handle: JoinHandle<AnalyzePackageJsonResponse>,
    request: &AnalyzePackageJsonRequest,
) -> AnalyzePackageJsonResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => {
            protocol_error_analyze_package_json_response(request, join_error_message(error))
        }
    }
}

async fn analyze_specifiers_response_from_join(
    response_handle: JoinHandle<AnalyzeSpecifiersResponse>,
    request: &AnalyzeSpecifiersRequest,
) -> AnalyzeSpecifiersResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => {
            protocol_error_analyze_specifiers_response(request, join_error_message(error))
        }
    }
}

async fn file_size_document_response_from_join(
    response_handle: JoinHandle<FileSizeDocumentResponse>,
    request: &FileSizeDocumentRequest,
) -> FileSizeDocumentResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => {
            protocol_error_file_size_document_response(request, join_error_message(error))
        }
    }
}

async fn complete_import_members_response_from_join(
    response_handle: JoinHandle<CompleteImportMembersResponse>,
    request: &CompleteImportMembersRequest,
) -> CompleteImportMembersResponse {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => {
            protocol_error_complete_import_members_response(request, join_error_message(error))
        }
    }
}

fn join_error_message(error: tokio::task::JoinError) -> String {
    format!("analysis worker failed: {error}")
}

fn protocol_error_analyze_document_response(
    request: &AnalyzeDocumentRequest,
    message: String,
) -> AnalyzeDocumentResponse {
    AnalyzeDocumentResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        imports: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_analyze_package_json_response(
    request: &AnalyzePackageJsonRequest,
    message: String,
) -> AnalyzePackageJsonResponse {
    AnalyzePackageJsonResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        sections: Vec::new(),
        states: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_analyze_specifiers_response(
    request: &AnalyzeSpecifiersRequest,
    message: String,
) -> AnalyzeSpecifiersResponse {
    AnalyzeSpecifiersResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        imports: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_file_size_document_response(
    request: &FileSizeDocumentRequest,
    message: String,
) -> FileSizeDocumentResponse {
    FileSizeDocumentResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        imports: Vec::new(),
        states: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_complete_import_members_response(
    request: &CompleteImportMembersRequest,
    message: String,
) -> CompleteImportMembersResponse {
    CompleteImportMembersResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        specifier: None,
        exports: Vec::new(),
        imported_names: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_diagnostics(message: String) -> Vec<ImportDiagnostic> {
    vec![ImportDiagnostic {
        stage: "protocol".to_owned(),
        message,
        details: Vec::new(),
    }]
}

fn is_supported_hello_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
}

fn recycle_if_needed(
    lifecycle: &LifecycleState,
    cache_len: usize,
    storage_path: Option<&Path>,
    prefetcher: &Prefetcher,
    service: &ImportLensService,
) -> bool {
    let Some(reason) = lifecycle.should_recycle(Instant::now(), cache_len) else {
        return false;
    };

    prefetcher.cancel();

    if let Err(error) = service.flush_cache() {
        logging::log_warn(
            "lifecycle",
            format!("failed to flush cache before recycle: {error}"),
        );
    }

    if let Some(storage_path) = storage_path
        && let Err(error) = record_recycle_timestamp(storage_path, SystemTime::now())
    {
        logging::log_warn(
            "lifecycle",
            format!("failed to record recycle timestamp: {error}"),
        );
    }

    logging::log_info("lifecycle", format!("recycle requested: {reason:?}"));
    true
}

#[cfg(not(windows))]
fn restrict_unix_socket_permissions(pipe_name: &str) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(pipe_name, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{
        codec::{FrameDecoder, decode_payload, encode_frame},
        protocol::{
            BatchRequest, BatchResponse, HelloMessage, ImportKind, ImportRequest, ImportRuntime,
            PROTOCOL_VERSION, ShutdownMessage,
        },
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

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let id = NEXT_TEMP_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
        let process_id = std::process::id();
        let path =
            std::env::temp_dir().join(format!("import-lens-server-{process_id}-{suffix}-{id}"));
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
        frames
            .extend(encode_frame(&cache_warmup_batch(&workspace, 3)).expect("batch should encode"));

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
        let response = batch_response_from_join(
            tokio::task::spawn_blocking(|| -> BatchResponse {
                panic!("analysis worker panic");
            }),
            &request,
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
}
