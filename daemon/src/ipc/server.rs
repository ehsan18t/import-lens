use crate::{
    cache::project::remove_legacy_central_cache,
    ipc::{
        codec::{decode_payload, message_frame_codec, payload_bytes},
        protocol::{
            AnalyzeDocumentRequest, AnalyzePackageJsonRequest, AnalyzePackageJsonResponse,
            AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse, CacheListRequest,
            CacheListResponse, CacheRemoveRequest, CacheRemoveResponse, CacheRemoveScope,
            CacheStatusRequest, CacheStatusResponse, ClientMessage, CompleteImportMembersRequest,
            CompleteImportMembersResponse, FreshnessKind, ImportDiagnostic, PROTOCOL_VERSION,
            RefreshRegistryHintsResponse, RefreshedResultsResponse, RegistryHintResult,
            RegistryHintTarget, WorkspaceReportRequest, WorkspaceReportResponse,
            WorkspaceReportSummary, is_supported_protocol_version,
        },
    },
    lifecycle::{LifecycleState, record_recycle_timestamp},
    logging::{self, parse_log_level, set_log_level},
    pipeline::analyze::AnalysisContext,
    prefetch::{Prefetcher, prewarm_root},
    service::{
        ImportLensService, StreamedDocumentAnalysis, protocol_error_analyze_document_response,
        protocol_error_exports_response, protocol_error_file_size_document_response,
    },
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const LIFECYCLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Delay after Hello before the single cache-maintenance pass runs, letting the
/// cold-open analysis burst settle first (see `spawn_cache_maintenance`).
const CACHE_MAINTENANCE_DELAY: Duration = Duration::from_secs(60);

/// How long shutdown, an idle recycle, or a lost connection waits for the tasks it has already
/// asked to stop (SRS FR-004c).
///
/// A bound, not a plain join: a build already inside Rolldown cannot be cancelled and runs to its
/// `BUILD_TIMEOUT` (8s), while the extension force-kills the daemon 5s after sending `shutdown`
/// (`extension/src/daemon/processLifecycle.ts`). An unbounded join would lose the flush. An
/// abandoned build costs only its own result, rebuilt next session (FR-026c).
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Aborts the wrapped task when dropped (connection end, or replacement by a
/// post-Hello respawn).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The active bulk registry-refresh block per source manifest for one connection. A newer bulk
/// request cancels only the block for the same source, and an ending connection cancels all of
/// them (decision-log D11). Cancellation flips a shared `AtomicBool` that registry lane jobs
/// re-read before each fetch; skipped work surfaces no error.
///
/// Keyed per source, never per connection: refreshing `backend/package.json` must not cancel the
/// in-flight `web/package.json` block, or its unfetched targets get a fabricated "worker did not
/// return a result" error.
struct RegistryRefreshLifecycle {
    active_by_source: HashMap<String, Arc<AtomicBool>>,
}

impl RegistryRefreshLifecycle {
    fn new() -> Self {
        Self {
            active_by_source: HashMap::new(),
        }
    }

    /// Cancels the previous block for this source and returns a fresh flag for the new one.
    /// Release pairs with the Acquire load each pool job does before fetching.
    fn start_new_block(&mut self, source: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Some(previous) = self
            .active_by_source
            .insert(source.to_owned(), Arc::clone(&flag))
        {
            previous.store(true, Ordering::Release);
        }
        flag
    }

    /// Cancel every source's block. `Drop` does this too, but only when the connection function
    /// returns, which is after the shutdown join this must shorten.
    fn cancel_all(&self) {
        for active in self.active_by_source.values() {
            active.store(true, Ordering::Release);
        }
    }
}

impl Drop for RegistryRefreshLifecycle {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// The final response of a bulk registry refresh: one result per target a job answered, in target
/// order. A slot no job filled is an error only while the block is live. Once a newer block for the
/// same source cancelled it, the empty slots are work it skipped, and they are left out: reported as
/// failures they would mark dependencies the newer block never named as failed, with nothing left
/// to fetch them again.
fn final_registry_results(
    ordered: Vec<Option<RegistryHintResult>>,
    targets: Vec<RegistryHintTarget>,
    cancelled: bool,
) -> Vec<RegistryHintResult> {
    ordered
        .into_iter()
        .zip(targets)
        .filter_map(|(result, target)| match result {
            Some(result) => Some(result),
            None if cancelled => None,
            None => Some(RegistryHintResult {
                target,
                hint: None,
                error: Some("registry refresh worker did not return a result".to_owned()),
                origin: None,
            }),
        })
        .collect()
}

/// Per-document cancellation for work a request left running or has not started yet: the SWR
/// revalidation after a stale size read, the pending-import builds a streamed document analysis
/// handed off, and the combined file-size build a queued size read has not entered yet. A newer
/// request for the SAME document flips the previous flag (the work is for a document state the
/// user has already replaced); the connection ending flips all of them.
///
/// One instance per kind of background work, never shared: the extension sends `AnalyzeDocument`
/// then `FileSizeDocument` for the same document, and a shared instance would let the size read
/// cancel the builds the analysis just handed off, leaving its imports at "Calculating…".
struct DocumentTaskLifecycle {
    active_by_document: HashMap<String, Arc<AtomicBool>>,
}

impl DocumentTaskLifecycle {
    fn new() -> Self {
        Self {
            active_by_document: HashMap::new(),
        }
    }

    fn start_document(&mut self, workspace_root: &str, document_path: &str) -> Arc<AtomicBool> {
        let key = document_key(workspace_root, document_path);
        let flag = Arc::new(AtomicBool::new(false));
        if let Some(previous) = self.active_by_document.insert(key, Arc::clone(&flag)) {
            previous.store(true, Ordering::Release);
        }
        flag
    }

    /// Cancel and forget the work of every document not in `visible`, which holds document paths.
    fn retain_visible(&mut self, visible: &HashSet<&str>) {
        self.active_by_document.retain(|key, active| {
            let keep = visible.contains(document_path_of(key));
            if !keep {
                active.store(true, Ordering::Release);
            }
            keep
        });
    }

    /// Cancel every document's work. `Drop` does this too, but only when the connection function
    /// returns, which is after the shutdown join this must shorten.
    fn cancel_all(&self) {
        for active in self.active_by_document.values() {
            active.store(true, Ordering::Release);
        }
    }
}

impl Drop for DocumentTaskLifecycle {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// At most one combined file-size build per document at a time.
///
/// The combined build (one Rolldown build per runtime, for the file's own totals) has no
/// supersession or single-flight of its own, and `FileSizeDocument` handlers run concurrently, one
/// per keystroke. Unserialized, they stack against the two-permit engine pool, each holding a
/// permit for up to `BUILD_TIMEOUT`. Paired with the supersession flag, the gate forms
/// [`CombinedBuildBound`].
struct DocumentBuildGate {
    gates: HashMap<String, Arc<tokio::sync::Semaphore>>,
}

impl DocumentBuildGate {
    fn new() -> Self {
        Self {
            gates: HashMap::new(),
        }
    }

    fn gate_for(
        &mut self,
        workspace_root: &str,
        document_path: &str,
    ) -> Arc<tokio::sync::Semaphore> {
        // A gate only this map holds (`strong_count == 1`) is inert; pruning it keeps the map
        // sized to the documents in flight.
        self.gates.retain(|_, gate| Arc::strong_count(gate) > 1);

        Arc::clone(
            self.gates
                .entry(document_key(workspace_root, document_path))
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1))),
        )
    }
}

/// The bound an INTERACTIVE combined file-size build runs under: wait for the document's in-flight
/// one, then build only if a newer size read has not replaced this one in the meantime.
///
/// Only a size read tagged with an analysis generation gets one, because only those stack (one
/// per keystroke). The "Show current file size" command and `importlens check` send untagged reads
/// that nothing supersedes; queueing them behind a parked build would push them past the client's
/// request timeout.
struct CombinedBuildBound {
    gate: Arc<tokio::sync::Semaphore>,
    superseded: Arc<AtomicBool>,
}

fn document_key(workspace_root: &str, document_path: &str) -> String {
    format!("{workspace_root}\0{document_path}")
}

fn document_path_of(key: &str) -> &str {
    key.split_once('\0').map_or(key, |(_, path)| path)
}

/// Every piece of background work one connection owns that can be asked to stop, grouped behind a
/// single `cancel_all` so teardown cannot miss one. A build already inside Rolldown cannot be
/// reached; [`TASK_JOIN_TIMEOUT`] bounds the wait for it.
struct ConnectionLifecycles {
    /// Cancels the in-flight bulk registry-refresh block, per source manifest.
    registry_refresh: RegistryRefreshLifecycle,
    /// Cancels the pending-import builds a superseded document analysis handed off. Separate from
    /// the SWR lifecycle: see [`DocumentTaskLifecycle`].
    document_stream: DocumentTaskLifecycle,
    /// Cancels the background revalidation a stale size read armed.
    swr_refresh: DocumentTaskLifecycle,
    /// Drops the combined file-size build of a size read a newer one has replaced.
    size_builds: DocumentTaskLifecycle,
}

impl ConnectionLifecycles {
    fn new() -> Self {
        Self {
            registry_refresh: RegistryRefreshLifecycle::new(),
            document_stream: DocumentTaskLifecycle::new(),
            swr_refresh: DocumentTaskLifecycle::new(),
            size_builds: DocumentTaskLifecycle::new(),
        }
    }

    /// Stop every background job that can be stopped, before the connection waits for the ones
    /// that cannot. Cancellation is cooperative: each job checks its flag before it starts.
    /// Prefetch jobs are abandoned rather than joined (NFR-004c).
    ///
    /// `Drop` does this too, but only when the connection function returns, which is after the
    /// join this must shorten.
    fn cancel_all(&self, prefetcher: &Prefetcher) {
        prefetcher.cancel();
        self.registry_refresh.cancel_all();
        self.document_stream.cancel_all();
        self.swr_refresh.cancel_all();
        self.size_builds.cancel_all();
    }

    /// Drop the queued work of every document the client no longer shows. The registry refresh
    /// belongs to a manifest, not a document, and the prefetcher already yields to analysis.
    fn retain_visible(&mut self, document_paths: &[String]) {
        let visible = document_paths
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        self.document_stream.retain_visible(&visible);
        self.swr_refresh.retain_visible(&visible);
        self.size_builds.retain_visible(&visible);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ipc_server_swr.rs"]
mod ipc_server_swr_tests;

#[cfg(test)]
#[path = "../../tests/unit/ipc_server_teardown.rs"]
mod ipc_server_teardown_tests;

/// Schedules one cache-maintenance pass (byte-budget eviction, compaction, registry retention,
/// orphan-shard sweep) a delay after Hello. There is no recurring tick (decision-log D3): a
/// project's cache converges to its distinct-import footprint, so one pass per project-open
/// suffices, at the cost of a long single-project session sitting up to ~2x the budget until the
/// next open. The pass runs on `spawn_blocking` so shard scans never stall the frame loop.
fn spawn_cache_maintenance(service: std::sync::Arc<ImportLensService>) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        tokio::time::sleep(CACHE_MAINTENANCE_DELAY).await;
        if spawn_blocking_noted(move || service.run_cache_maintenance())
            .await
            .is_err()
        {
            logging::log_warn("cache", "cache maintenance pass panicked");
        }
    }))
}

/// One frame, already encoded, waiting for the connection's single writer.
///
/// Every response and push leaves through this channel, never from the connection loop's body:
/// the loop only reads frames, hands each request to a task, and writes what tasks queue. A request
/// arm must never `.await` its handler inline, or the loop suspends inside the arm, the outbound
/// arm stops running, and one parked build holds every other frame's delivery.
type OutboundFrame = Bytes;

/// Encode one message and queue it for the connection's writer. Encoding happens on the producing
/// task, so the writer only ever moves bytes.
///
/// A failed encode is logged and dropped rather than killing the connection: the client's request
/// timeout covers the missing reply, and a teardown would discard the warm cache and every other
/// in-flight request.
fn queue_outbound<T: Serialize>(outbound: &mpsc::UnboundedSender<OutboundFrame>, message: &T) {
    match payload_bytes(message) {
        Ok(frame) => {
            let _ = outbound.send(frame);
        }
        Err(error) => logging::log_warn(
            "ipc",
            format!("dropping an outbound frame that failed to encode: {error}"),
        ),
    }
}

/// Queues a request's response. One that cannot be encoded is replaced by `fallback`, the request's
/// protocol error carrying the encoder's reason: a dropped response would leave the client waiting
/// out its timeout. Streamed partials go through [`queue_outbound`] instead, because the final
/// response carries everything a partial did.
fn queue_response<T: Serialize>(
    outbound: &mpsc::UnboundedSender<OutboundFrame>,
    response: &T,
    fallback: impl FnOnce(String) -> T,
) {
    match payload_bytes(response) {
        Ok(frame) => {
            let _ = outbound.send(frame);
        }
        Err(error) => {
            let message = format!("the response could not be encoded: {error}");
            logging::log_warn("ipc", message.clone());
            queue_outbound(outbound, &fallback(message));
        }
    }
}

/// Run one request's handler off the connection loop and queue its response on the outbound
/// channel, like any push.
///
/// `on_error` builds the request-scoped protocol error when the handler's blocking task panics or
/// is cancelled, or its response cannot be encoded.
fn spawn_request<T, R>(
    active_tasks: &mut Vec<JoinHandle<()>>,
    outbound: &mpsc::UnboundedSender<OutboundFrame>,
    request_for_error: R,
    on_error: impl Fn(&R, String) -> T + Send + Sync + 'static,
    handler: impl FnOnce() -> T + Send + 'static,
) where
    T: Serialize + Send + 'static,
    R: Send + Sync + 'static,
{
    let outbound = outbound.clone();
    let handle = tokio::spawn(async move {
        let response =
            response_from_join(spawn_blocking_noted(handler), &request_for_error, &on_error).await;
        queue_response(&outbound, &response, |message| {
            on_error(&request_for_error, message)
        });
    });
    track_active_task(active_tasks, handle);
}

/// Write whatever is still queued before the connection closes on *our* terms (a `Shutdown`
/// message, an idle recycle). A response a handler finished just as the client asked us to stop is
/// still owed to the client; a client that vanished is not, so the disconnect path does not drain.
async fn drain_outbound<S>(
    framed: &mut Framed<S, LengthDelimitedCodec>,
    outbound_rx: &mut mpsc::UnboundedReceiver<OutboundFrame>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Ok(frame) = outbound_rx.try_recv() {
        if let Err(error) = framed.send(frame).await {
            logging::log_warn(
                "ipc",
                format!("failed to flush a queued frame before closing: {error}"),
            );
            return;
        }
    }
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
    storage_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    use tokio::net::UnixListener;

    // `symlink_metadata`, so a dangling link at the path is removed instead of failing the bind.
    if std::fs::symlink_metadata(pipe_name).is_ok() {
        std::fs::remove_file(pipe_name)?;
    }

    // The client chooses the path. One past the platform's `sun_path` limit (104 bytes on macOS,
    // 108 on Linux, NUL included) fails here, and its length is what diagnoses that.
    let listener = UnixListener::bind(pipe_name).map_err(|error| {
        format!(
            "cannot bind IPC socket {pipe_name} ({} bytes): {error}",
            pipe_name.len()
        )
    })?;
    restrict_unix_socket_permissions(pipe_name)?;

    let service = std::sync::Arc::new(ImportLensService::new(None, false));
    let prefetcher = Prefetcher::new();

    let accepted = listener.accept().await;
    // One client per daemon, so the path is needed only until it connects. Unlinking it here
    // rather than on exit means no way out of the process, SIGKILL included, leaves it behind.
    drop(listener);
    if let Err(error) = std::fs::remove_file(pipe_name) {
        logging::log_warn(
            "ipc",
            format!("failed to remove IPC socket {pipe_name}: {error}"),
        );
    }
    let (stream, _) = accepted?;

    handle_connection(stream, storage_path, service, prefetcher).await
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
    // Spawned at Hello (the pre-Hello service has no storage); aborted on drop.
    let mut _maintenance_task: Option<AbortOnDrop> = None;
    let mut lifecycle = LifecycleState::new();
    let mut lifecycles = ConnectionLifecycles::new();
    // With the `size_builds` flag, bounds the combined file-size build: see `CombinedBuildBound`.
    let mut size_build_gate = DocumentBuildGate::new();
    let lifecycle_storage_path = storage_path;
    // Unbounded on purpose. A client that stops reading stalls the loop in the outbound arm, where
    // it reads no new frames, so the queue is bounded by requests already in flight. A bounded
    // channel would let a slow client stall a producer.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<OutboundFrame>();
    // Every task this connection spawns. Shutdown and idle-recycle join them, so nothing is still
    // writing to the cache after the flush.
    let mut active_tasks: Vec<JoinHandle<()>> = Vec::new();
    // Set while a cache invalidation runs. No frame is read until it settles, so every request
    // that follows an invalidation still sees its effect; frames already queued keep going out.
    let mut invalidation: Option<oneshot::Receiver<()>> = None;
    // The workspace root the client opened this connection for.
    let mut connection_workspace_root: Option<PathBuf> = None;

    loop {
        let payload = tokio::select! {
            () = invalidation_settled(&mut invalidation), if invalidation.is_some() => {
                invalidation = None;
                continue;
            }
            outbound = outbound_rx.recv() => {
                // The loop itself holds a sender, so `recv` cannot return None here.
                if let Some(frame) = outbound
                    && let Err(error) = framed.send(frame).await
                {
                    // The socket failed: drop the queue, but still flush the cache.
                    close_connection(
                        &service,
                        &prefetcher,
                        &lifecycles,
                        &mut active_tasks,
                        &mut invalidation,
                        &mut _maintenance_task,
                    )
                    .await;
                    return Err(Box::new(error));
                }
                continue;
            }
            payload = framed.next(), if invalidation.is_none() => match payload.transpose() {
                Ok(payload) => payload,
                Err(error) => {
                    close_connection(
                        &service,
                        &prefetcher,
                        &lifecycles,
                        &mut active_tasks,
                        &mut invalidation,
                        &mut _maintenance_task,
                    )
                    .await;
                    return Err(Box::new(error));
                }
            },
            _ = tokio::time::sleep(LIFECYCLE_CHECK_INTERVAL) => {
                // Only checks for an idle recycle; cache maintenance is its own task.
                if recycle_if_needed(
                    &lifecycle,
                    lifecycle_storage_path.as_deref(),
                    &prefetcher,
                    &service,
                    &mut active_tasks,
                    &mut _maintenance_task,
                )
                .await
                {
                    drain_outbound(&mut framed, &mut outbound_rx).await;
                    return Ok(());
                }
                continue;
            }
        };
        // EOF: the client closed the pipe or crashed without `shutdown`. Nobody is left to answer,
        // but the cache still gets flushed.
        let Some(payload) = payload else {
            close_connection(
                &service,
                &prefetcher,
                &lifecycles,
                &mut active_tasks,
                &mut invalidation,
                &mut _maintenance_task,
            )
            .await;
            break;
        };

        let message = match decode_payload::<ClientMessage>(&payload) {
            Ok(message) => message,
            Err(error) => {
                // An undecodable frame (corrupt payload, or an unknown message type from a newer
                // client) is skipped, not fatal. Framing-level errors stay fatal above.
                logging::log_warn("ipc", format!("ignoring undecodable client frame: {error}"));
                continue;
            }
        };

        crate::reclaim::note_activity();
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
                // A test-injected `RegistryHttpClient` (only those services set
                // `preserve_registry_across_hello`) must survive the handshake instead of being
                // replaced by a real `UreqRegistryHttpClient`.
                service = if service.preserve_registry_across_hello() {
                    match std::sync::Arc::try_unwrap(service) {
                        Ok(previous) => {
                            std::sync::Arc::new(previous.rebuild_cache_registry_for_hello(
                                Some(hello_storage_path),
                                hello.enable_disk_cache,
                                hello.cache_max_size_mb,
                                hello.registry_cache_max_size_mb,
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
                                hello.registry_cache_max_size_mb,
                            ))
                        }
                    }
                } else {
                    std::sync::Arc::new(ImportLensService::new_with_cache_policy(
                        Some(hello_storage_path),
                        hello.enable_disk_cache,
                        hello.cache_max_size_mb,
                        hello.registry_cache_max_size_mb,
                    ))
                };
                hello_received = true;
                if let Some(storage_path) = lifecycle_storage_path.as_deref()
                    && let Some(result) = remove_legacy_central_cache(storage_path)
                {
                    log_legacy_cache_removal(&result);
                }
                // Lift the process-global recency clock above every persisted shard's max seq
                // before any request can create an entry; otherwise a post-restart access (small
                // seq) sorts older than an untouched prior-session shard and the evictor picks the
                // active project. Inline on purpose: the loop finishes this Hello before reading
                // the next frame, so the seed precedes the first analysis.
                let seed_started_at = Instant::now();
                service.seed_recency_clock_from_disk();
                logging::log_debug(
                    "cache",
                    format!(
                        "hello recency seed finished in {}ms",
                        seed_started_at.elapsed().as_millis()
                    ),
                );
                // Replacing the handle aborts a previous pending pass if a client re-handshakes.
                _maintenance_task = Some(spawn_cache_maintenance(std::sync::Arc::clone(&service)));
                connection_workspace_root = Some(hello_workspace_root.clone());
                prefetcher.prewarm_recent_cache_entries(
                    std::sync::Arc::clone(&service),
                    hello_workspace_root,
                );

                if recycle_if_needed(
                    &lifecycle,
                    lifecycle_storage_path.as_deref(),
                    &prefetcher,
                    &service,
                    &mut active_tasks,
                    &mut _maintenance_task,
                )
                .await
                {
                    drain_outbound(&mut framed, &mut outbound_rx).await;
                    return Ok(());
                }
            }
            ClientMessage::AnalyzeDocument(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                // A newer analysis of the same document cancels this one's queued builds; other
                // documents are unaffected.
                let superseded = lifecycles
                    .document_stream
                    .start_document(&request.workspace_root, &request.active_document_path);
                track_active_task(
                    &mut active_tasks,
                    spawn_document_analysis(&service, &outbound_tx, request, superseded),
                );
            }
            ClientMessage::AnalyzeDocument(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_analyze_document_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::AnalyzePackageJson(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                if request.version >= 2 && request.streaming {
                    let request_for_error = request.clone();
                    let (partial_tx, partial_rx) = mpsc::unbounded_channel();
                    let response_handle = spawn_blocking_noted(move || {
                        svc.handle_analyze_package_json_streaming(request, move |partial| {
                            let _ = partial_tx.send(partial);
                        })
                    });
                    track_active_task(
                        &mut active_tasks,
                        spawn_streaming_forwarder(
                            &outbound_tx,
                            partial_rx,
                            response_handle,
                            request_for_error,
                            protocol_error_analyze_package_json_response,
                        ),
                    );
                } else {
                    spawn_request(
                        &mut active_tasks,
                        &outbound_tx,
                        request.clone(),
                        protocol_error_analyze_package_json_response,
                        move || svc.handle_analyze_package_json(request),
                    );
                }
            }
            ClientMessage::AnalyzePackageJson(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_analyze_package_json_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::AnalyzeSpecifiers(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                // Deliberately not streamed (FR-004b): it waits for every engine miss, but as a
                // task, so the connection keeps serving.
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_analyze_specifiers_response,
                    move || svc.handle_analyze_specifiers(request),
                );
            }
            ClientMessage::AnalyzeSpecifiers(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_analyze_specifiers_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::CacheInvalidate(message) if hello_received => {
                prefetcher.cancel();
                invalidation = Some(spawn_invalidation(
                    &mut active_tasks,
                    &service,
                    move |service| service.invalidate_package(&message.package_name),
                ));
            }
            ClientMessage::CacheInvalidateAll(_) if hello_received => {
                prefetcher.cancel();
                invalidation = Some(spawn_invalidation(
                    &mut active_tasks,
                    &service,
                    ImportLensService::invalidate_all,
                ));
            }
            ClientMessage::CacheStatus(request) if hello_received => {
                let svc = std::sync::Arc::clone(&service);
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_cache_status_response,
                    move || svc.cache_status(request),
                );
            }
            ClientMessage::CacheStatus(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_cache_status_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::CacheList(request) if hello_received => {
                let svc = std::sync::Arc::clone(&service);
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_cache_list_response,
                    move || svc.list_cache(request),
                );
            }
            ClientMessage::CacheList(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_cache_list_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::CacheRemove(request) if hello_received => {
                prefetcher.cancel();
                let svc = std::sync::Arc::clone(&service);
                let storage_path = lifecycle_storage_path.clone();
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_cache_remove_response,
                    move || {
                        let remove_legacy_cache = matches!(request.scope, CacheRemoveScope::All);
                        let mut response = svc.remove_cache(request);
                        if remove_legacy_cache
                            && let Some(storage_path) = storage_path.as_deref()
                            && let Some(result) = remove_legacy_central_cache(storage_path)
                        {
                            log_legacy_cache_removal(&result);
                            if result.removed {
                                response.removed.push(result);
                            } else {
                                response.failed.push(result);
                            }
                        }
                        response
                    },
                );
            }
            ClientMessage::CacheRemove(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_cache_remove_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::RefreshRegistryHints(request) if hello_received => {
                if !is_supported_protocol_version(request.version) {
                    let message = format!("unsupported protocol version {}", request.version);
                    queue_outbound(
                        &outbound_tx,
                        &RefreshRegistryHintsResponse {
                            version: request.version.min(PROTOCOL_VERSION),
                            request_id: request.request_id,
                            results: Vec::new(),
                            indexes: None,
                            error: Some(message.clone()),
                            diagnostics: vec![ImportDiagnostic::for_stage("protocol", &message)],
                        },
                    );
                    continue;
                }

                let version = request.version;
                let request_id = request.request_id;
                let mode = request.mode;
                let targets = request.targets;
                let now_ms = crate::time::unix_millis_now();
                let (partial_tx, mut partial_rx) = mpsc::unbounded_channel();
                let outbound = outbound_tx.clone();

                // A request without `source` (older client) uses the empty key, so all such
                // requests supersede each other connection-wide.
                let source = request.source.clone().unwrap_or_default();
                let cancelled = lifecycles.registry_refresh.start_new_block(&source);
                let block_cancelled = Arc::clone(&cancelled);
                let final_targets = targets.clone();
                let target_count = targets.len();

                service.spawn_registry_refresh_block(
                    targets,
                    mode,
                    now_ms,
                    cancelled,
                    move |index, result| {
                        // A skipped job reports `None`; `final_registry_results` decides what its slot means.
                        if let Some(result) = result {
                            let _ = partial_tx.send((index, result));
                        }
                    },
                );
                let flush_service = std::sync::Arc::clone(&service);

                // Tracked (FR-004c): it owns the response and the registry snapshot flush.
                let forwarder = tokio::spawn(async move {
                    let mut ordered_results = vec![None; target_count];
                    while let Some((index, result)) = partial_rx.recv().await {
                        let mut indexes = vec![index];
                        let mut results = vec![result];
                        while let Ok((index, result)) = partial_rx.try_recv() {
                            indexes.push(index);
                            results.push(result);
                        }
                        for (index, result) in indexes.iter().zip(results.iter()) {
                            ordered_results[*index] = Some(result.clone());
                        }
                        queue_outbound(
                            &outbound,
                            &RefreshRegistryHintsResponse {
                                version,
                                request_id,
                                results,
                                indexes: Some(indexes),
                                error: None,
                                diagnostics: Vec::new(),
                            },
                        );
                    }
                    // Every job finished or was skipped: persist in one snapshot write.
                    flush_service.flush_registry_hints();

                    let results = final_registry_results(
                        ordered_results,
                        final_targets,
                        block_cancelled.load(Ordering::Acquire),
                    );

                    queue_response(
                        &outbound,
                        &RefreshRegistryHintsResponse {
                            version,
                            request_id,
                            results,
                            indexes: None,
                            error: None,
                            diagnostics: Vec::new(),
                        },
                        |message| RefreshRegistryHintsResponse {
                            version,
                            request_id,
                            results: Vec::new(),
                            indexes: None,
                            error: Some(message.clone()),
                            diagnostics: vec![ImportDiagnostic::for_stage("protocol", &message)],
                        },
                    );
                });
                track_active_task(&mut active_tasks, forwarder);
                continue;
            }
            ClientMessage::RefreshRegistryHints(request) => {
                let message = "hello message not received".to_owned();
                queue_outbound(
                    &outbound_tx,
                    &RefreshRegistryHintsResponse {
                        version: request.version.min(PROTOCOL_VERSION),
                        request_id: request.request_id,
                        results: Vec::new(),
                        indexes: None,
                        error: Some(message.clone()),
                        diagnostics: vec![ImportDiagnostic::for_stage("protocol", &message)],
                    },
                );
            }
            ClientMessage::WorkspaceReport(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let request_for_error = request.clone();
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                service.spawn_workspace_report(request, response_tx);
                let outbound = outbound_tx.clone();
                // Tracked (FR-004c).
                let forwarder = tokio::spawn(async move {
                    let response = response_rx.await.unwrap_or_else(|_| {
                        workspace_report_protocol_error(
                            &request_for_error,
                            "workspace report worker stopped before sending a response",
                        )
                    });
                    queue_response(&outbound, &response, |message| {
                        workspace_report_protocol_error(&request_for_error, &message)
                    });
                });
                track_active_task(&mut active_tasks, forwarder);
                continue;
            }
            ClientMessage::WorkspaceReport(request) => {
                queue_outbound(
                    &outbound_tx,
                    &workspace_report_protocol_error(&request, "hello message not received"),
                );
            }
            ClientMessage::PrewarmPackageJson(message) if hello_received => {
                let package_json_path = PathBuf::from(message.package_json_path);
                let root = message.workspace_root.map_or_else(
                    || prewarm_root(connection_workspace_root.as_deref(), &package_json_path),
                    PathBuf::from,
                );
                prefetcher.prewarm_package_json(
                    std::sync::Arc::clone(&service),
                    root,
                    package_json_path,
                    PathBuf::from(message.active_document_path),
                );
            }
            ClientMessage::NodeModulesChanged(message) if hello_received => {
                // An empty batch invalidates nothing.
                if message.package_json_paths.is_empty() && message.tsconfig_paths.is_empty() {
                    continue;
                }
                prefetcher.cancel();
                // Both halves always run: a batch can carry an install AND a tsconfig edit.
                invalidation = Some(spawn_invalidation(
                    &mut active_tasks,
                    &service,
                    move |service| {
                        service.invalidate_package_json_paths(&message.package_json_paths);
                        service.invalidate_workspace_config_paths(&message.tsconfig_paths);
                    },
                ));
            }
            ClientMessage::VisibleDocuments(message) if hello_received => {
                lifecycles.retain_visible(&message.document_paths);
            }
            ClientMessage::EnumerateExports(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_exports_response,
                    move || svc.enumerate_exports(request),
                );
            }
            ClientMessage::EnumerateExports(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_exports_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::FileSizeDocument(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let swr_cancelled = lifecycles
                    .swr_refresh
                    .start_document(&request.workspace_root, &request.active_document_path);
                // Only an interactive size read is bounded: see `CombinedBuildBound`.
                let combined_build = request.analysis_generation.map(|_| CombinedBuildBound {
                    gate: size_build_gate
                        .gate_for(&request.workspace_root, &request.active_document_path),
                    superseded: lifecycles
                        .size_builds
                        .start_document(&request.workspace_root, &request.active_document_path),
                });
                track_active_task(
                    &mut active_tasks,
                    spawn_file_size_document(
                        &service,
                        &outbound_tx,
                        request,
                        swr_cancelled,
                        combined_build,
                    ),
                );
            }
            ClientMessage::FileSizeDocument(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_file_size_document_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::CompleteImportMembers(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_complete_import_members_response,
                    move || svc.complete_import_members(request),
                );
            }
            ClientMessage::CompleteImportMembers(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_complete_import_members_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::Shutdown(_) => {
                close_connection(
                    &service,
                    &prefetcher,
                    &lifecycles,
                    &mut active_tasks,
                    &mut invalidation,
                    &mut _maintenance_task,
                )
                .await;
                // Frames the tasks queued on their way out are still owed to a client that asked
                // to stop. A vanished client is owed nothing, so other close paths do not drain.
                drain_outbound(&mut framed, &mut outbound_rx).await;
                return Ok(());
            }
            ClientMessage::PrewarmPackageJson(_)
            | ClientMessage::NodeModulesChanged(_)
            | ClientMessage::VisibleDocuments(_)
            | ClientMessage::CacheInvalidate(_)
            | ClientMessage::CacheInvalidateAll(_) => {}
        }
    }

    Ok(())
}

/// Every blocking handler runs here, holding a `reclaim::Work` so the sweep waits for it and then
/// follows the frees it made as it finished.
fn spawn_blocking_noted<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    tokio::task::spawn_blocking(move || {
        let _work = crate::reclaim::work();
        work()
    })
}

/// Run a cache invalidation off the connection loop. It rewrites every shard on disk, blocking redb
/// I/O that grows with the number of projects ever opened, and the loop must keep writing frames
/// meanwhile. The returned receiver settles when the invalidation has finished.
fn spawn_invalidation(
    active_tasks: &mut Vec<JoinHandle<()>>,
    service: &std::sync::Arc<ImportLensService>,
    invalidate: impl FnOnce(&ImportLensService) + Send + 'static,
) -> oneshot::Receiver<()> {
    let service = std::sync::Arc::clone(service);
    let (done_tx, done_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        if let Err(error) = spawn_blocking_noted(move || invalidate(&service)).await {
            logging::log_warn("cache", format!("cache invalidation failed: {error}"));
        }
        let _ = done_tx.send(());
    });
    track_active_task(active_tasks, handle);
    done_rx
}

/// Resolves once the pending invalidation has finished, or its task is gone.
async fn invalidation_settled(pending: &mut Option<oneshot::Receiver<()>>) {
    if let Some(done) = pending {
        let _ = done.await;
    }
}

/// Registers a task the connection owns. Finished handles are pruned on each push, so a long-lived
/// connection does not accumulate them.
fn track_active_task(active_tasks: &mut Vec<JoinHandle<()>>, handle: JoinHandle<()>) {
    reap_finished_tasks(active_tasks);
    active_tasks.push(handle);
}

/// Drop the handles of tasks that have already finished.
///
/// Load-bearing: `recycle_if_needed` reads `active_tasks.is_empty()` as "nothing in flight", and
/// an unreaped finished handle would defer every recycle by a 60s tick.
fn reap_finished_tasks(active_tasks: &mut Vec<JoinHandle<()>>) {
    active_tasks.retain(|active| !active.is_finished());
}

/// Join the tasks this connection spawned, giving up after [`TASK_JOIN_TIMEOUT`]. Returns whether
/// every one of them finished; the handles that did not are left in `active_tasks`.
///
/// Callers must cancel what they can first (`ConnectionLifecycles::cancel_all`): the wait is for
/// work that cannot be cancelled.
async fn wait_for_active_tasks(active_tasks: &mut Vec<JoinHandle<()>>) -> bool {
    let deadline = tokio::time::Instant::now() + TASK_JOIN_TIMEOUT;
    let mut unfinished = Vec::new();

    for mut handle in std::mem::take(active_tasks) {
        match tokio::time::timeout_at(deadline, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                logging::log_warn("ipc", format!("connection task failed: {error}"));
            }
            // Past the deadline. `timeout_at` returns immediately for every remaining handle, so
            // the whole loop costs the bound once, not once per task.
            Err(_) => unfinished.push(handle),
        }
    }

    if !unfinished.is_empty() {
        logging::log_warn(
            "ipc",
            format!(
                "{} connection task(s) did not finish within {}s; continuing without them",
                unfinished.len(),
                TASK_JOIN_TIMEOUT.as_secs()
            ),
        );
    }

    let finished = unfinished.is_empty();
    *active_tasks = unfinished;
    finished
}

/// The one way a connection stops serving, however it ends: the client's `shutdown`, EOF, or a
/// socket failure.
///
/// Cancel, join under a deadline, then flush the cache unconditionally (FR-004c), even when a task
/// outlived the join. The flush must not be left to `Drop`, which reaches only entries already
/// queued for the batched commit, never a dirty one whose insert failed nor earned recency.
async fn close_connection(
    service: &ImportLensService,
    prefetcher: &Prefetcher,
    lifecycles: &ConnectionLifecycles,
    active_tasks: &mut Vec<JoinHandle<()>>,
    invalidation: &mut Option<oneshot::Receiver<()>>,
    maintenance_task: &mut Option<AbortOnDrop>,
) {
    // Abort the pending maintenance pass first: it is scheduled, not started, and a pass that
    // begins while we are flushing is compacting the shards the flush is writing to.
    *maintenance_task = None;
    lifecycles.cancel_all(prefetcher);
    // Unbounded, unlike the join below: an invalidation is pure redb I/O that cannot park inside
    // Rolldown, and one cut off part-way leaves later shards serving entries the client was told
    // are gone, with nothing to send the invalidation again.
    invalidation_settled(invalidation).await;
    // Bounded: see `TASK_JOIN_TIMEOUT`.
    wait_for_active_tasks(active_tasks).await;

    if let Err(error) = service.flush_cache() {
        logging::log_warn(
            "lifecycle",
            format!("failed to flush cache while closing the connection: {error}"),
        );
    }
}

/// Answer a document analysis from the cache, then build its misses and push each one as it lands.
///
/// Both halves run in one task, which preserves the ordering pushes depend on: the response (cache
/// hits plus `loading` placeholders) goes out before any push that updates it, and the outbound
/// channel is FIFO per sender.
///
/// Tracked by the caller, not detached, so no build writes to the cache after the shutdown flush.
fn spawn_document_analysis(
    service: &std::sync::Arc<ImportLensService>,
    outbound_tx: &mpsc::UnboundedSender<OutboundFrame>,
    request: AnalyzeDocumentRequest,
    superseded: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let service = std::sync::Arc::clone(service);
    let outbound = outbound_tx.clone();

    tokio::spawn(async move {
        let request_for_error = request.clone();
        let analysis_service = std::sync::Arc::clone(&service);
        let analysis_handle = spawn_blocking_noted(move || {
            analysis_service.handle_analyze_document_streaming(
                request,
                &crate::document::IgnoreRuleResolver::default(),
            )
        });
        let analysis =
            response_from_join(analysis_handle, &request_for_error, |request, message| {
                StreamedDocumentAnalysis::settled(protocol_error_analyze_document_response(
                    request, message,
                ))
            })
            .await;

        queue_response(&outbound, &analysis.response, |message| {
            protocol_error_analyze_document_response(&request_for_error, message)
        });

        if analysis.pending.is_empty() {
            return;
        }

        let request = request_for_error;
        let build = spawn_blocking_noted(move || {
            let context = AnalysisContext {
                workspace_root: PathBuf::from(&request.workspace_root),
                active_document_path: PathBuf::from(&request.active_document_path),
            };
            service.complete_pending_imports(
                &context,
                analysis.measured,
                analysis.pending,
                || !superseded.load(Ordering::Acquire),
                |results, identities| {
                    queue_outbound(
                        &outbound,
                        &RefreshedResultsResponse {
                            message_type: "refreshed_results".to_owned(),
                            version: PROTOCOL_VERSION,
                            workspace_root: request.workspace_root.clone(),
                            document_path: request.active_document_path.clone(),
                            results,
                            identities,
                            // The analysis request id is the client's freshness generation, so a
                            // push for a since-edited document is dropped like a stale SWR push.
                            generation: Some(request.request_id),
                        },
                    );
                },
            );
        });
        if let Err(error) = build.await {
            logging::log_warn("ipc", format!("streamed import build failed: {error}"));
        }
    })
}

/// Size a document, then revalidate anything served stale in the background and push the fresh
/// results. For an interactive read, `combined_build` admits one combined build per document at a
/// time and skips a read a newer one has replaced: see [`CombinedBuildBound`].
fn spawn_file_size_document(
    service: &std::sync::Arc<ImportLensService>,
    outbound_tx: &mpsc::UnboundedSender<OutboundFrame>,
    request: crate::ipc::protocol::FileSizeDocumentRequest,
    swr_cancelled: Arc<AtomicBool>,
    combined_build: Option<CombinedBuildBound>,
) -> JoinHandle<()> {
    let service = std::sync::Arc::clone(service);
    let outbound = outbound_tx.clone();

    tokio::spawn(async move {
        let request_for_error = request.clone();
        // Wait here, not in the engine: an engine permit is daemon-wide and held for the whole
        // build once acquired.
        let mut permit = None;

        if let Some(bound) = combined_build {
            let Ok(acquired) = Arc::clone(&bound.gate).acquire_owned().await else {
                // The gate is never closed; unreachable in practice.
                return;
            };
            permit = Some(acquired);

            if bound.superseded.load(Ordering::Acquire) {
                // Superseded while waiting: answer with an error, which the client drops on its
                // generation guard (FR-004a); the newer read queued behind produces the number.
                queue_outbound(
                    &outbound,
                    &protocol_error_file_size_document_response(
                        &request_for_error,
                        "superseded by a newer size read for this document".to_owned(),
                    ),
                );
                return;
            }
        }

        let size_service = std::sync::Arc::clone(&service);
        // The file totals come from a real combined build; per-import misses come back `loading`
        // (the preceding `AnalyzeDocument` is building them). A force-fresh request is served
        // complete by this same call.
        let response_handle =
            spawn_blocking_noted(move || size_service.handle_file_size_document_streaming(request));
        let response = response_from_join(
            response_handle,
            &request_for_error,
            protocol_error_file_size_document_response,
        )
        .await;
        // SWR: recompute only the imports served Stale; a fresh sibling must not be re-analyzed.
        let stale_specifiers = response
            .imports
            .iter()
            .filter(|result| matches!(result.freshness.kind, FreshnessKind::Stale))
            .map(|result| result.specifier.clone())
            .collect::<std::collections::HashSet<_>>();
        queue_response(&outbound, &response, |message| {
            protocol_error_file_size_document_response(&request_for_error, message)
        });
        // Release the gate: revalidation has its own supersession and single-flight.
        drop(permit);

        if stale_specifiers.is_empty() {
            return;
        }

        // What the client was just served: the revalidation re-derives shared bytes over it.
        let served = response.states;
        // Cancellation is per document: only a newer size read of this document supersedes it.
        let revalidation = spawn_blocking_noted(move || {
            if let Some((workspace_root, document_path, results, identities)) = service
                .revalidate_document_sizes(&request_for_error, &stale_specifiers, &served, || {
                    !swr_cancelled.load(Ordering::Acquire)
                })
            {
                queue_outbound(
                    &outbound,
                    &RefreshedResultsResponse {
                        message_type: "refreshed_results".to_owned(),
                        version: PROTOCOL_VERSION,
                        workspace_root,
                        document_path,
                        results,
                        identities,
                        // Lets the client drop this push if a newer analysis superseded it.
                        generation: request_for_error.analysis_generation,
                    },
                );
            }
        });
        if let Err(error) = revalidation.await {
            logging::log_warn("ipc", format!("size revalidation failed: {error}"));
        }
    })
}

fn spawn_streaming_forwarder<T, R>(
    outbound_tx: &mpsc::UnboundedSender<OutboundFrame>,
    mut partial_rx: mpsc::UnboundedReceiver<T>,
    response_handle: JoinHandle<T>,
    request_for_error: R,
    on_error: impl Fn(&R, String) -> T + Send + Sync + 'static,
) -> JoinHandle<()>
where
    T: Serialize + Send + 'static,
    R: Send + Sync + 'static,
{
    let outbound = outbound_tx.clone();
    tokio::spawn(async move {
        while let Some(partial) = partial_rx.recv().await {
            queue_outbound(&outbound, &partial);
        }

        let final_response =
            response_from_join(response_handle, &request_for_error, &on_error).await;
        queue_response(&outbound, &final_response, |message| {
            on_error(&request_for_error, message)
        });
    })
}

pub async fn response_from_join<T, R>(
    response_handle: JoinHandle<T>,
    request: &R,
    on_error: impl FnOnce(&R, String) -> T,
) -> T {
    match response_handle.await {
        Ok(response) => response,
        Err(error) => on_error(request, join_error_message(error)),
    }
}

fn join_error_message(error: tokio::task::JoinError) -> String {
    format!("analysis worker failed: {error}")
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
        current_project: None,
        total_bytes: 0,
        budget_bytes: 0,
        registry_size_bytes: 0,
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
        scrubbed_entries: 0,
        registry_entries_removed: 0,
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics(message),
    }
}

fn protocol_diagnostics(message: String) -> Vec<ImportDiagnostic> {
    vec![ImportDiagnostic::for_stage("protocol", message)]
}

async fn recycle_if_needed(
    lifecycle: &LifecycleState,
    storage_path: Option<&Path>,
    prefetcher: &Prefetcher,
    service: &ImportLensService,
    active_tasks: &mut Vec<JoinHandle<()>>,
    maintenance_task: &mut Option<AbortOnDrop>,
) -> bool {
    let Some(reason) = lifecycle.should_recycle(Instant::now()) else {
        return false;
    };

    prefetcher.cancel();
    reap_finished_tasks(active_tasks);
    if !active_tasks.is_empty() {
        wait_for_active_tasks(active_tasks).await;
        return false;
    }

    // Abort the pending maintenance pass, or it may compact the shards this flush writes.
    *maintenance_task = None;

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
    use super::{
        DocumentBuildGate, DocumentTaskLifecycle, RegistryHintResult, RegistryHintTarget,
        RegistryRefreshLifecycle, TASK_JOIN_TIMEOUT, final_registry_results, queue_response,
        reap_finished_tasks, wait_for_active_tasks,
    };
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::task::JoinHandle;

    /// A reply that serializes to its text, or fails to serialize at all.
    enum Reply {
        Unencodable,
        Text(String),
    }

    impl serde::Serialize for Reply {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Self::Unencodable => Err(serde::ser::Error::custom("refuses to encode")),
                Self::Text(text) => serializer.serialize_str(text),
            }
        }
    }

    /// A response that cannot be encoded still answers its request, with the fallback naming why,
    /// instead of leaving the client to wait out its timeout.
    #[test]
    fn an_unencodable_response_is_replaced_by_its_fallback() {
        let (outbound, mut frames) = tokio::sync::mpsc::unbounded_channel();

        queue_response(&outbound, &Reply::Unencodable, Reply::Text);

        let frame = frames
            .try_recv()
            .expect("the request must still be answered");
        let text: String = crate::ipc::codec::decode_payload(&frame).expect("fallback frame");
        assert!(text.contains("refuses to encode"), "{text}");
        assert!(frames.try_recv().is_err(), "exactly one frame per response");
    }

    /// Shutdown must not be hostage to a build it cannot cancel (see `TASK_JOIN_TIMEOUT`). The
    /// handles it gives up on are kept and reported, not silently dropped.
    #[tokio::test]
    async fn waiting_for_active_tasks_gives_up_on_a_task_that_outlives_the_bound() {
        let parked = TASK_JOIN_TIMEOUT * 15;
        let mut active_tasks: Vec<JoinHandle<()>> = vec![tokio::spawn(async move {
            // Stands in for a build parked inside the bundler: far longer than the bound, and
            // nothing here can cancel it.
            tokio::time::sleep(parked).await;
        })];

        let started_at = std::time::Instant::now();
        let finished = wait_for_active_tasks(&mut active_tasks).await;

        assert!(
            !finished,
            "a task that outlives the bound must be reported as unfinished, not waited out"
        );
        assert!(
            started_at.elapsed() < parked / 2,
            "the wait must end at the bound, not at the task: waited {:?}",
            started_at.elapsed()
        );
        assert_eq!(
            active_tasks.len(),
            1,
            "the handle it gave up on is kept, so a later pass can still join it"
        );
    }

    #[tokio::test]
    async fn waiting_for_active_tasks_joins_the_tasks_that_do_finish() {
        let mut active_tasks: Vec<JoinHandle<()>> =
            vec![tokio::spawn(async {}), tokio::spawn(async {})];

        assert!(wait_for_active_tasks(&mut active_tasks).await);
        assert!(active_tasks.is_empty());
    }

    /// `recycle_if_needed` reads `active_tasks.is_empty()` as "nothing in flight", so finished
    /// handles must be reaped or every recycle is deferred by a 60s tick.
    #[tokio::test]
    async fn finished_task_handles_are_reaped() {
        let finished = tokio::spawn(async {});
        let running = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        finished.await.expect("the finished task cannot panic");
        // `await` above consumed the handle, so re-create the pair the loop would be holding.
        let mut active_tasks: Vec<JoinHandle<()>> = vec![tokio::spawn(async {}), running];
        tokio::task::yield_now().await;

        reap_finished_tasks(&mut active_tasks);

        assert_eq!(
            active_tasks.len(),
            1,
            "only the still-running task may be left"
        );
        assert!(!active_tasks[0].is_finished());
    }

    /// The combined file-size build's bound: one gate per document, shared by every size read of
    /// that document, and a separate one for every other document (a build for `a.ts` must never
    /// wait on a build for `b.ts`).
    #[test]
    fn the_document_build_gate_is_shared_per_document_and_pruned_when_idle() {
        let mut gate = DocumentBuildGate::new();

        let first = gate.gate_for("C:/ws", "C:/ws/a.ts");
        let same_document = gate.gate_for("C:/ws", "C:/ws/a.ts");
        let other_document = gate.gate_for("C:/ws", "C:/ws/b.ts");

        assert!(
            Arc::ptr_eq(&first, &same_document),
            "two size reads of the same document must queue on the SAME gate"
        );
        assert!(
            !Arc::ptr_eq(&first, &other_document),
            "a size read of another document must not wait behind this one"
        );

        // The map must not grow by one entry per document the session ever sized.
        drop(first);
        drop(same_document);
        drop(other_document);
        let _fresh = gate.gate_for("C:/ws", "C:/ws/c.ts");
        assert_eq!(
            gate.gates.len(),
            1,
            "gates nobody holds are inert and must be pruned"
        );
    }

    /// Shutdown cancels before it joins; `Drop` runs too late, after the join.
    #[test]
    fn cancelling_a_document_lifecycle_flips_every_documents_flag() {
        let mut lifecycle = DocumentTaskLifecycle::new();
        let first = lifecycle.start_document("C:/ws", "C:/ws/a.ts");
        let second = lifecycle.start_document("C:/ws", "C:/ws/b.ts");
        assert!(!first.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));

        lifecycle.cancel_all();

        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
    }

    /// A document the client stopped showing loses its queued builds; a shown one keeps them, and
    /// showing the hidden one again starts it afresh instead of inheriting the cancellation.
    #[test]
    fn retaining_visible_documents_cancels_only_the_hidden_ones() {
        let mut lifecycle = DocumentTaskLifecycle::new();
        let shown = lifecycle.start_document("C:/ws", "C:/ws/a.ts");
        let hidden = lifecycle.start_document("C:/ws", "C:/ws/b.ts");

        lifecycle.retain_visible(&["C:/ws/a.ts"].into_iter().collect());

        assert!(!shown.load(Ordering::Acquire));
        assert!(hidden.load(Ordering::Acquire));
        let reopened = lifecycle.start_document("C:/ws", "C:/ws/b.ts");
        assert!(!reopened.load(Ordering::Acquire));
    }

    #[test]
    fn a_cancelled_registry_block_omits_the_targets_it_skipped() {
        let target = |name: &str| RegistryHintTarget {
            name: name.to_owned(),
            installed_version: None,
        };
        let answered = RegistryHintResult {
            target: target("react"),
            hint: None,
            error: None,
            origin: None,
        };
        let ordered = vec![Some(answered.clone()), None];
        let targets = vec![target("react"), target("lodash")];

        assert_eq!(
            final_registry_results(ordered.clone(), targets.clone(), true),
            vec![answered.clone()],
            "a superseded block's skipped target is not a failure"
        );

        let live = final_registry_results(ordered, targets, false);
        assert_eq!(live.len(), 2);
        assert_eq!(live[0], answered);
        assert!(
            live[1].error.is_some(),
            "a live block with no answer for a target reports the failure"
        );
    }

    #[test]
    fn registry_refresh_lifecycle_supersedes_only_within_the_same_source() {
        let mut lifecycle = RegistryRefreshLifecycle::new();
        let first = lifecycle.start_new_block("web/package.json");
        assert!(!first.load(Ordering::Acquire), "a fresh block starts live");

        // A block for a different source must not cancel an unrelated in-flight block
        // (decision-log D11).
        let other = lifecycle.start_new_block("backend/package.json");
        assert!(
            !first.load(Ordering::Acquire),
            "a block for a different source must not cancel another source's block"
        );
        assert!(!other.load(Ordering::Acquire), "the new block starts live");

        // A newer bulk request for the same source supersedes the block it replaces.
        let second = lifecycle.start_new_block("web/package.json");
        assert!(
            first.load(Ordering::Acquire),
            "a new bulk block must cancel the same-source block it supersedes"
        );
        assert!(
            !second.load(Ordering::Acquire),
            "the superseding block itself starts live"
        );

        // Connection end (guard drop) cancels every source's still-draining block.
        drop(lifecycle);
        assert!(
            second.load(Ordering::Acquire),
            "dropping the connection lifecycle must cancel the active web block"
        );
        assert!(
            other.load(Ordering::Acquire),
            "dropping the connection lifecycle must cancel the active backend block"
        );
    }
}
