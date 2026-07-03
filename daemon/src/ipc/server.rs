use crate::{
    cache::project::remove_legacy_central_cache,
    ipc::{
        codec::{decode_payload, message_frame_codec, payload_bytes},
        protocol::{
            AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
            AnalyzePackageJsonResponse, AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse,
            BatchRequest, BatchResponse, CacheCleanupRequest, CacheCleanupResponse,
            CacheListRequest, CacheListResponse, CacheRemoveRequest, CacheRemoveResponse,
            CacheRemoveScope, CacheStatusRequest, CacheStatusResponse, ClientMessage,
            CompleteImportMembersRequest, CompleteImportMembersResponse, EnumerateExportsRequest,
            EnumerateExportsResponse, FileSizeDocumentRequest, FileSizeDocumentResponse,
            FileSizeRequest, FileSizeResponse, ImportDiagnostic, PROTOCOL_VERSION,
            RefreshRegistryHintsResponse, RegistryHintResult, WorkspaceReportRequest,
            WorkspaceReportResponse, WorkspaceReportSummary, is_supported_protocol_version,
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
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const LIFECYCLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

enum ServerOutboundMessage {
    RefreshRegistryHints(RefreshRegistryHintsResponse),
    WorkspaceReport(WorkspaceReportResponse),
}

async fn send_outbound_message<S>(
    framed: &mut Framed<S, LengthDelimitedCodec>,
    message: ServerOutboundMessage,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match message {
        ServerOutboundMessage::RefreshRegistryHints(response) => {
            framed.send(payload_bytes(&response)?).await?
        }
        ServerOutboundMessage::WorkspaceReport(response) => {
            framed.send(payload_bytes(&response)?).await?
        }
    }
    Ok(())
}

fn workspace_report_protocol_error(
    request: &WorkspaceReportRequest,
    message: &str,
) -> WorkspaceReportResponse {
    WorkspaceReportResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        rows: Vec::new(),
        summary: WorkspaceReportSummary::default(),
        error: Some(message.to_owned()),
        diagnostics: vec![ImportDiagnostic::for_stage("workspace_report", message)],
    }
}

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
    let lifecycle_storage_path = storage_path;
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerOutboundMessage>();

    macro_rules! send_message {
        ($message:expr) => {
            framed.send(payload_bytes(&$message)?).await?;
        };
    }

    loop {
        let payload = tokio::select! {
            outbound = outbound_rx.recv() => {
                if let Some(message) = outbound {
                    send_outbound_message(&mut framed, message).await?;
                }
                continue;
            }
            payload = framed.next() => payload.transpose()?,
            _ = tokio::time::sleep(LIFECYCLE_CHECK_INTERVAL) => {
                if recycle_if_needed(
                    &lifecycle,
                    service.cache_len(),
                    lifecycle_storage_path.as_deref(),
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

        let message = match decode_payload::<ClientMessage>(&payload) {
            Ok(message) => message,
            Err(error) => {
                // A single undecodable frame (a corrupt payload, or an unknown
                // message type from a newer client) must not tear down the
                // connection and discard the warm cache and in-flight work.
                // Framing-level errors (oversized frames, io failures) still
                // propagate below and remain fatal.
                logging::log_warn("ipc", format!("ignoring undecodable client frame: {error}"));
                continue;
            }
        };

        match message {
            ClientMessage::Hello(hello) => {
                if !is_supported_protocol_version(hello.version) {
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
                // Integration tests that inject a fake `RegistryHttpClient` via
                // `ImportLensService::new_with_registry_hints_for_tests` need that
                // client to survive the Hello handshake; otherwise this
                // reconstruction would silently replace it with a real
                // `UreqRegistryHttpClient` built from `hello.storage_path`. Only
                // those test-constructed services set `preserve_registry_across_hello`.
                service = if service.preserve_registry_across_hello() {
                    match std::sync::Arc::try_unwrap(service) {
                        Ok(previous) => {
                            std::sync::Arc::new(previous.rebuild_cache_registry_for_hello(
                                Some(hello_storage_path),
                                hello.enable_disk_cache,
                                hello.cache_max_size_mb,
                                hello.cache_max_age_days,
                            ))
                        }
                        Err(_shared) => {
                            logging::log_debug(
                                "server",
                                "injected registry service was unexpectedly shared during hello; \
                                 building a fresh production service instead",
                            );
                            std::sync::Arc::new(ImportLensService::new_with_cache_policy(
                                Some(hello_storage_path),
                                hello.enable_disk_cache,
                                hello.cache_max_size_mb,
                                hello.cache_max_age_days,
                            ))
                        }
                    }
                } else {
                    std::sync::Arc::new(ImportLensService::new_with_cache_policy(
                        Some(hello_storage_path),
                        hello.enable_disk_cache,
                        hello.cache_max_size_mb,
                        hello.cache_max_age_days,
                    ))
                };
                hello_received = true;
                if let Some(storage_path) = lifecycle_storage_path.as_deref()
                    && let Some(result) = remove_legacy_central_cache(storage_path)
                {
                    log_legacy_cache_removal(&result);
                }
                let cleanup = service.cleanup_cache(CacheCleanupRequest {
                    message_type: "cache_cleanup".to_owned(),
                    version: PROTOCOL_VERSION,
                    request_id: 0,
                });
                if !cleanup.removed.is_empty() {
                    logging::log_info(
                        "cache",
                        format!(
                            "startup cache cleanup removed {} shard(s)",
                            cleanup.removed.len()
                        ),
                    );
                }
                if !cleanup.failed.is_empty() {
                    logging::log_warn(
                        "cache",
                        format!(
                            "startup cache cleanup failed for {} shard(s)",
                            cleanup.failed.len()
                        ),
                    );
                }
                prefetcher.prewarm_recent_cache_entries(
                    std::sync::Arc::clone(&service),
                    hello_workspace_root,
                );

                if recycle_if_needed(
                    &lifecycle,
                    service.cache_len(),
                    lifecycle_storage_path.as_deref(),
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
                    lifecycle_storage_path.as_deref(),
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
                let response_handle = tokio::task::spawn_blocking(move || {
                    svc.handle_analyze_document(
                        request,
                        &crate::document::IgnoreRuleResolver::default(),
                    )
                });
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
                prefetcher.cancel();
                service.invalidate_package(&message.package_name);
            }
            ClientMessage::CacheInvalidateAll(_) if hello_received => {
                prefetcher.cancel();
                service.invalidate_all();
            }
            ClientMessage::CacheStatus(request) if hello_received => {
                let response = service.cache_status(request);
                send_message!(response);
            }
            ClientMessage::CacheStatus(request) => {
                let response = protocol_error_cache_status_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::CacheCleanup(request) if hello_received => {
                prefetcher.cancel();
                let response = service.cleanup_cache(request);
                send_message!(response);
            }
            ClientMessage::CacheCleanup(request) => {
                let response = protocol_error_cache_cleanup_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::CacheList(request) if hello_received => {
                let response = service.list_cache(request);
                send_message!(response);
            }
            ClientMessage::CacheList(request) => {
                let response = protocol_error_cache_list_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::CacheRemove(request) if hello_received => {
                prefetcher.cancel();
                let remove_legacy_cache = matches!(request.scope, CacheRemoveScope::All);
                let mut response = service.remove_cache(request);
                if remove_legacy_cache
                    && let Some(storage_path) = lifecycle_storage_path.as_deref()
                    && let Some(result) = remove_legacy_central_cache(storage_path)
                {
                    log_legacy_cache_removal(&result);
                    if result.removed {
                        response.removed.push(result);
                    } else {
                        response.failed.push(result);
                    }
                }
                send_message!(response);
            }
            ClientMessage::CacheRemove(request) => {
                let response = protocol_error_cache_remove_response(
                    &request,
                    "hello message not received".to_owned(),
                );
                send_message!(response);
            }
            ClientMessage::RefreshRegistryHints(request) if hello_received => {
                if !is_supported_protocol_version(request.version) {
                    let message = format!("unsupported protocol version {}", request.version);
                    send_message!(RefreshRegistryHintsResponse {
                        version: request.version.min(PROTOCOL_VERSION),
                        request_id: request.request_id,
                        results: Vec::new(),
                        indexes: None,
                        error: Some(message.clone()),
                        diagnostics: vec![ImportDiagnostic::for_stage("protocol", &message)],
                    });
                    continue;
                }

                let version = request.version;
                let request_id = request.request_id;
                let mode = request.mode;
                let targets = request.targets;
                let now_ms = crate::time::unix_millis_now();
                let (partial_tx, mut partial_rx) = mpsc::unbounded_channel();
                let outbound = outbound_tx.clone();

                for (index, target) in targets.iter().cloned().enumerate() {
                    let svc = std::sync::Arc::clone(&service);
                    let tx = partial_tx.clone();
                    service.spawn_registry_refresh(move || {
                        let target_for_error = target.clone();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            svc.refresh_registry_hint_target(target, mode, now_ms)
                        }))
                        .unwrap_or_else(|_| {
                            logging::log_warn(
                                "registry",
                                format!("registry worker panicked for {}", target_for_error.name),
                            );
                            RegistryHintResult {
                                target: target_for_error,
                                hint: None,
                                error: Some("registry worker panicked".to_owned()),
                            }
                        });
                        let _ = tx.send((index, result));
                    });
                }
                drop(partial_tx);
                let flush_service = std::sync::Arc::clone(&service);

                tokio::spawn(async move {
                    let mut ordered_results = vec![None; targets.len()];
                    while let Some((index, result)) = partial_rx.recv().await {
                        ordered_results[index] = Some(result.clone());
                        let _ = outbound.send(ServerOutboundMessage::RefreshRegistryHints(
                            RefreshRegistryHintsResponse {
                                version,
                                request_id,
                                results: vec![result],
                                indexes: Some(vec![index]),
                                error: None,
                                diagnostics: Vec::new(),
                            },
                        ));
                    }
                    // All refresh workers have finished; persist their fetched
                    // metadata in one snapshot write.
                    flush_service.flush_registry_hints();

                    let results = ordered_results
                        .into_iter()
                        .zip(targets)
                        .map(|(result, target)| {
                            result.unwrap_or(RegistryHintResult {
                                target,
                                hint: None,
                                error: Some(
                                    "registry refresh worker did not return a result".to_owned(),
                                ),
                            })
                        })
                        .collect();

                    let _ = outbound.send(ServerOutboundMessage::RefreshRegistryHints(
                        RefreshRegistryHintsResponse {
                            version,
                            request_id,
                            results,
                            indexes: None,
                            error: None,
                            diagnostics: Vec::new(),
                        },
                    ));
                });
                continue;
            }
            ClientMessage::RefreshRegistryHints(request) => {
                let message = "hello message not received".to_owned();
                send_message!(RefreshRegistryHintsResponse {
                    version: request.version.min(PROTOCOL_VERSION),
                    request_id: request.request_id,
                    results: Vec::new(),
                    indexes: None,
                    error: Some(message.clone()),
                    diagnostics: vec![ImportDiagnostic::for_stage("protocol", &message)],
                });
            }
            ClientMessage::WorkspaceReport(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let request_for_error = request.clone();
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                service.spawn_workspace_report(request, response_tx);
                let outbound = outbound_tx.clone();
                tokio::spawn(async move {
                    let response = response_rx.await.unwrap_or_else(|_| {
                        workspace_report_protocol_error(
                            &request_for_error,
                            "workspace report worker stopped before sending a response",
                        )
                    });
                    let _ = outbound.send(ServerOutboundMessage::WorkspaceReport(response));
                });
                continue;
            }
            ClientMessage::WorkspaceReport(request) => {
                send_message!(workspace_report_protocol_error(
                    &request,
                    "hello message not received"
                ));
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
                prefetcher.cancel();
                lifecycle.record_batch();
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
                prefetcher.cancel();
                lifecycle.record_batch();
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
                prefetcher.cancel();
                lifecycle.record_batch();
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
                prefetcher.cancel();
                lifecycle.record_batch();
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
                prefetcher.cancel();
                service.flush_cache_recency_touches();
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

fn log_legacy_cache_removal(result: &crate::ipc::protocol::CacheOperationResult) {
    if result.removed {
        logging::log_info(
            "cache",
            format!("removed legacy central cache {}", result.cache_path),
        );
        return;
    }

    if let Some(error) = result.error.as_ref() {
        logging::log_warn(
            "cache",
            format!(
                "failed to remove legacy central cache {}: {}",
                result.cache_path, error
            ),
        );
    }
}

fn protocol_error_cache_status_response(
    request: &CacheStatusRequest,
    message: String,
) -> CacheStatusResponse {
    CacheStatusResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        total_size_bytes: 0,
        project_count: 0,
        max_size_mb: 0,
        max_age_days: 0,
        last_cleanup_millis: None,
        current_project: None,
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_cache_cleanup_response(
    request: &CacheCleanupRequest,
    message: String,
) -> CacheCleanupResponse {
    CacheCleanupResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        total_size_bytes: 0,
        removed: Vec::new(),
        failed: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_cache_list_response(
    request: &CacheListRequest,
    message: String,
) -> CacheListResponse {
    CacheListResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        shards: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_error_cache_remove_response(
    request: &CacheRemoveRequest,
    message: String,
) -> CacheRemoveResponse {
    CacheRemoveResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        removed: Vec::new(),
        failed: Vec::new(),
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_diagnostics(message: String) -> Vec<ImportDiagnostic> {
    vec![ImportDiagnostic::for_stage("protocol", message)]
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
