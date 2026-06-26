use crate::{
    ipc::{
        codec::{decode_payload, message_frame_codec, payload_bytes},
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
use futures_util::{SinkExt, StreamExt};
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

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

pub async fn handle_connection<S>(
    stream: S,
    storage_path: Option<PathBuf>,
    mut service: std::sync::Arc<ImportLensService>,
    prefetcher: Prefetcher,
) -> Result<(), Box<dyn Error>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, message_frame_codec());
    let mut hello_received = false;
    let mut lifecycle = LifecycleState::new();
    let mut storage_path = storage_path;

    macro_rules! send_message {
        ($message:expr) => {
            framed.send(payload_bytes(&$message)?).await?;
        };
    }

    loop {
        let payload = tokio::select! {
            payload = framed.next() => payload.transpose()?,
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
        let Some(payload) = payload else {
            break;
        };

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
                        send_message!(response);
                    }
                    let response =
                        batch_response_from_join(response_handle, &request_for_error).await;
                    send_message!(response);
                } else {
                    let request_for_error = request.clone();
                    let response_handle =
                        tokio::task::spawn_blocking(move || svc.handle_batch(request));
                    let response =
                        batch_response_from_join(response_handle, &request_for_error).await;
                    send_message!(response);
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
                send_message!(response);
            }
            ClientMessage::AnalyzeDocument(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                let request_for_error = request.clone();
                let response_handle =
                    tokio::task::spawn_blocking(move || svc.handle_analyze_document(request));
                let response =
                    analyze_document_response_from_join(response_handle, &request_for_error).await;
                send_message!(response);
            }
            ClientMessage::AnalyzeDocument(request) => {
                let response = protocol_error_analyze_document_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::AnalyzePackageJson(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                if request.version >= 2 && request.streaming {
                    let request_for_error = request.clone();
                    let (partial_tx, mut partial_rx) = mpsc::unbounded_channel();
                    let response_handle = tokio::task::spawn_blocking(move || {
                        svc.handle_analyze_package_json_streaming(request, move |partial| {
                            let _ = partial_tx.send(partial);
                        })
                    });
                    while let Some(response) = partial_rx.recv().await {
                        send_message!(response);
                    }
                    let response = analyze_package_json_response_from_join(
                        response_handle,
                        &request_for_error,
                    )
                    .await;
                    send_message!(response);
                } else {
                    let request_for_error = request.clone();
                    let response_handle = tokio::task::spawn_blocking(move || {
                        svc.handle_analyze_package_json(request)
                    });
                    let response = analyze_package_json_response_from_join(
                        response_handle,
                        &request_for_error,
                    )
                    .await;
                    send_message!(response);
                }
            }
            ClientMessage::AnalyzePackageJson(request) => {
                let response = protocol_error_analyze_package_json_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
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
                send_message!(response);
            }
            ClientMessage::AnalyzeSpecifiers(request) => {
                let response = protocol_error_analyze_specifiers_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
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
                send_message!(response);
            }
            ClientMessage::EnumerateExports(request) => {
                let response = protocol_error_exports_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::FileSize(request) if hello_received => {
                let svc = std::sync::Arc::clone(&service);
                let request_for_error = request.clone();
                let response_handle =
                    tokio::task::spawn_blocking(move || svc.handle_file_size(request));
                let response =
                    file_size_response_from_join(response_handle, &request_for_error).await;
                send_message!(response);
            }
            ClientMessage::FileSize(request) => {
                let response = protocol_error_file_size_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::FileSizeDocument(request) if hello_received => {
                let svc = std::sync::Arc::clone(&service);
                let request_for_error = request.clone();
                let response_handle =
                    tokio::task::spawn_blocking(move || svc.handle_file_size_document(request));
                let response =
                    file_size_document_response_from_join(response_handle, &request_for_error)
                        .await;
                send_message!(response);
            }
            ClientMessage::FileSizeDocument(request) => {
                let response = protocol_error_file_size_document_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::CompleteImportMembers(request) if hello_received => {
                let svc = std::sync::Arc::clone(&service);
                let request_for_error = request.clone();
                let response_handle =
                    tokio::task::spawn_blocking(move || svc.complete_import_members(request));
                let response =
                    complete_import_members_response_from_join(response_handle, &request_for_error)
                        .await;
                send_message!(response);
            }
            ClientMessage::CompleteImportMembers(request) => {
                let response = protocol_error_complete_import_members_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
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

    Ok(())
}

pub async fn batch_response_from_join(
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
        indexes: None,
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
