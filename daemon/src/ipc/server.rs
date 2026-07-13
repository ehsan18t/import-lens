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
            WorkspaceReportRequest, WorkspaceReportResponse, WorkspaceReportSummary,
            is_supported_protocol_version,
        },
    },
    lifecycle::{LifecycleState, record_recycle_timestamp},
    logging::{self, parse_log_level, set_log_level},
    pipeline::analyze::AnalysisContext,
    prefetch::Prefetcher,
    service::{
        ImportLensService, StreamedDocumentAnalysis, protocol_error_analyze_document_response,
        protocol_error_batch_response, protocol_error_exports_response,
        protocol_error_file_size_document_response, protocol_error_file_size_response,
    },
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::{
    collections::HashMap,
    error::Error,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const LIFECYCLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Delay after Hello before the single cache-maintenance pass runs, letting the
/// cold-open analysis burst settle first (see `spawn_cache_maintenance`).
const CACHE_MAINTENANCE_DELAY: Duration = Duration::from_secs(60);

/// How long shutdown, an idle recycle, or a lost connection waits for the tasks it has already
/// asked to stop (SRS FR-004c).
///
/// It has to be a *bound*, not a plain join, because one class of task cannot be asked to stop: a
/// build already inside Rolldown runs until its own `BUILD_TIMEOUT` (8s) and nothing can cancel it.
/// The extension force-kills the daemon 5s after sending `shutdown`
/// (`extension/src/daemon/processLifecycle.ts`), so an unbounded join hands the process to the
/// killer *before* `flush_cache` ever runs — and the flush is the whole point of a graceful
/// shutdown. Waiting 2s and flushing anyway trades the one thing an abandoned build can cost (its
/// own result is not persisted, so it is rebuilt next session — and a build that hit the timeout
/// was never cacheable anyway, FR-026c) against the thing losing the flush costs: every entry the
/// session computed.
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Aborts the wrapped task when dropped (connection end, or replacement by a
/// post-Hello respawn).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Tracks the active bulk registry-refresh block **per source manifest** for one
/// connection so a newer bulk request supersedes (cancels) only the block for the
/// SAME source it replaces, and so an ending connection cancels whatever is still
/// draining (D7 / §6.1, keyed per-source per D11). Cancellation only flips a
/// shared `AtomicBool` that the isolated registry pool's jobs re-read before each
/// network fetch — a superseded/abandoned block skips its remaining fetches, with
/// no error surfaced for the skipped work.
///
/// Keying per source (mirroring `DocumentTaskLifecycle`, decision-log D9) is what
/// keeps a cold-cache multi-manifest prewarm honest: refreshing `backend/
/// package.json` must not cancel the still-in-flight `web/package.json` block,
/// which would otherwise strand every not-yet-fetched target with a fabricated
/// "worker did not return a result" error (regression P1-5).
struct RegistryRefreshLifecycle {
    active_by_source: HashMap<String, Arc<AtomicBool>>,
}

impl RegistryRefreshLifecycle {
    fn new() -> Self {
        Self {
            active_by_source: HashMap::new(),
        }
    }

    /// Cancels the previous block for this source (so its queued jobs skip their
    /// remaining fetches) and hands back a fresh cancel flag for the new block.
    /// Release pairs with the Acquire load each pool job does before fetching.
    /// Blocks for other sources are left draining untouched.
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

    /// Cancel every source's block. `Drop` does this too, but `Drop` runs when the connection
    /// function RETURNS — which on the shutdown path is after the join it was supposed to shorten.
    fn cancel_all(&self) {
        for active in self.active_by_source.values() {
            active.store(true, Ordering::Release);
        }
    }
}

impl Drop for RegistryRefreshLifecycle {
    fn drop(&mut self) {
        // Connection ended (disconnect, idle recycle, shutdown): cancel every
        // source's still-draining block so its queued jobs skip remaining fetches.
        self.cancel_all();
    }
}

/// Per-document cancellation for work a request left running or has not started yet: the SWR
/// revalidation after a stale size read, the pending-import builds a streamed document analysis
/// handed off, and the combined file-size build a queued size read has not entered yet. A newer
/// request for the SAME document flips the previous flag (the work is for a document state the
/// user has already replaced); the connection ending flips all of them.
///
/// One instance per KIND of background work, never one shared between them: the extension sends
/// `AnalyzeDocument` and then `FileSizeDocument` for the same document, so a shared instance
/// would have the file-size request cancel the very builds the analysis had just handed off, and
/// the document's imports would sit at "Calculating…" forever.
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

    /// Cancel every document's work. `Drop` does this too, but `Drop` runs when the connection
    /// function RETURNS — which on the shutdown path is after the join it was supposed to shorten,
    /// making it useless exactly when it matters most.
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

/// At most ONE combined file-size build per document at a time.
///
/// The combined build (one Rolldown build per runtime, for the file's own totals) is the one piece
/// of engine work with neither supersession nor single-flight of its own — `FileSizeDocument` is
/// sent on every keystroke's analysis, and since the connection loop became a multiplexer those
/// handlers run CONCURRENTLY. Nothing then stopped a user typing in an Astro file from stacking
/// combined builds against the two-permit engine pool, each holding a permit for up to
/// `BUILD_TIMEOUT`, while the per-import builds of every other document queued behind them.
///
/// The gate serializes them per document. Paired with the supersession flag it forms
/// [`CombinedBuildBound`], which is what the interactive size reads run under, and which is where
/// the whole bound is described.
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
        // A gate nobody holds or waits on (`strong_count == 1`: only this map) is inert, so
        // dropping it loses nothing and keeps the map the size of the documents actually in
        // flight rather than of every document the session ever sized.
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
/// Only a size read tagged with the analysis generation it belongs to gets one, because only those
/// can stack: the extension sends one per keystroke. The "Show current file size" command and
/// `importlens check` send a size read that is the user's whole request — a human cannot stack them,
/// nothing supersedes them, and making them queue behind a parked build (up to `BUILD_TIMEOUT`, on
/// top of their own) would turn a slow answer into the client's request timeout, which is no answer
/// at all.
struct CombinedBuildBound {
    gate: Arc<tokio::sync::Semaphore>,
    superseded: Arc<AtomicBool>,
}

fn document_key(workspace_root: &str, document_path: &str) -> String {
    format!("{workspace_root}\0{document_path}")
}

/// Every piece of background work one connection owns that can be ASKED to stop.
///
/// Grouped so the teardown cannot forget one of them: there is a single `cancel_all`, and the one
/// function that ends a connection calls it. What cancellation cannot reach — a build already
/// inside Rolldown — is what [`TASK_JOIN_TIMEOUT`] is for.
struct ConnectionLifecycles {
    /// Cancels the in-flight bulk registry-refresh block, per source manifest.
    registry_refresh: RegistryRefreshLifecycle,
    /// Cancels the pending-import builds a superseded document analysis handed off. Separate from
    /// the SWR lifecycle on purpose — see [`DocumentTaskLifecycle`].
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

    /// Stop every background job this connection owns that CAN be stopped, before the connection
    /// waits for the ones that cannot.
    ///
    /// Cancellation here is cooperative and cheap: a superseded/abandoned build, revalidation or
    /// registry fetch checks its flag before it starts and skips. What it cannot reach is a build
    /// already inside Rolldown — hence [`TASK_JOIN_TIMEOUT`] — and the prefetch jobs, which are
    /// abandoned rather than joined (NFR-004c).
    ///
    /// `Drop` does this too, but `Drop` runs when the connection function RETURNS — which is after
    /// the join it was supposed to shorten, making it useless exactly when it matters most.
    fn cancel_all(&self, prefetcher: &Prefetcher) {
        prefetcher.cancel();
        self.registry_refresh.cancel_all();
        self.document_stream.cancel_all();
        self.swr_refresh.cancel_all();
        self.size_builds.cancel_all();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ipc_server_swr.rs"]
mod ipc_server_swr_tests;

#[cfg(test)]
#[path = "../../tests/unit/ipc_server_teardown.rs"]
mod ipc_server_teardown_tests;

/// Schedules ONE cache-maintenance pass (byte-budget eviction + compaction +
/// registry retention + orphan-shard sweep) a short delay after Hello, then
/// stops — no recurring tick.
///
/// Rationale (design 2026-07-08): a project's cache converges to its
/// distinct-import footprint (re-analysis is a cache hit, not growth), so it
/// cannot grow unboundedly over a session — continuous polling is wasted work.
/// One pass per project-open, run after the cold-analysis burst has settled (the
/// delay), reclaims/compacts/prunes exactly when there is something to do. Each
/// new project-open (new connection) schedules its own pass, so multi-project
/// growth stays bounded; the only cost is that a heavy long single-project
/// session may sit up to ~2x the budget until the next open/relaunch — bounded,
/// cheap, and self-correcting. The pass runs via `spawn_blocking` so its shard
/// scans never stall the connection's frame loop, and `AbortOnDrop` cancels it if
/// the window closes before it fires.
fn spawn_cache_maintenance(service: std::sync::Arc<ImportLensService>) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        tokio::time::sleep(CACHE_MAINTENANCE_DELAY).await;
        if tokio::task::spawn_blocking(move || service.run_cache_maintenance())
            .await
            .is_err()
        {
            logging::log_warn("cache", "cache maintenance pass panicked");
        }
    }))
}

/// One frame, already encoded, waiting for the connection's single writer.
///
/// **Every** response and every push leaves the daemon through this channel — never through the
/// connection loop's own body. That is what makes the loop a pure multiplexer: it reads frames,
/// hands each request to a task, and writes whatever the tasks queue. Nothing an individual
/// handler does — a combined file-size build that parks for the full `BUILD_TIMEOUT` included —
/// can stall the delivery of a frame that belongs to somebody else.
///
/// It did before. Each request arm used to `.await` its handler inline, so while that await was
/// pending the loop sat suspended INSIDE the arm rather than in its `select!`, and the outbound
/// arm never ran. Streamed import results were computed on time and then simply not written to
/// the socket: the extension sends `AnalyzeDocument` and immediately `FileSizeDocument` for the
/// same document, and one parked combined build held every push behind it for the whole build
/// timeout — long enough for the next analysis to blow its deadline and for the client to discard
/// the entire document, cache hits included. That is the very loss the streaming design exists to
/// close.
type OutboundFrame = Bytes;

/// Encode one message and queue it for the connection's writer. Encoding happens on the producing
/// task, so the writer only ever moves bytes.
///
/// A failed encode is logged and dropped rather than killing the connection: it can only come from
/// a malformed response value, the client's own request timeout already covers a missing reply,
/// and tearing the connection down would take the warm cache and every other in-flight request
/// with it.
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

/// Run one request's handler off the connection loop and queue its response on the outbound
/// channel, like any push.
///
/// `on_error` builds the request-scoped protocol error when the handler's blocking task panics or
/// is cancelled — the same contract the arms had when they awaited `response_from_join` inline.
fn spawn_request<T, R>(
    active_tasks: &mut Vec<JoinHandle<()>>,
    outbound: &mpsc::UnboundedSender<OutboundFrame>,
    request_for_error: R,
    on_error: impl FnOnce(&R, String) -> T + Send + 'static,
    handler: impl FnOnce() -> T + Send + 'static,
) where
    T: Serialize + Send + 'static,
    R: Send + Sync + 'static,
{
    let outbound = outbound.clone();
    let handle = tokio::spawn(async move {
        let response = response_from_join(
            tokio::task::spawn_blocking(handler),
            &request_for_error,
            on_error,
        )
        .await;
        queue_outbound(&outbound, &response);
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
    // Detached byte-budget maintenance; spawned at Hello (the pre-Hello service
    // has no storage). Aborted on drop when the connection ends.
    let mut _maintenance_task: Option<AbortOnDrop> = None;
    let mut lifecycle = LifecycleState::new();
    // Every cancellable background job this connection owns, in one place so the teardown cannot
    // miss one. Each is superseded per document / per source while the connection runs, and all of
    // them are cancelled when it ends.
    let mut lifecycles = ConnectionLifecycles::new();
    // Bounds the combined file-size build: one per document at a time (the gate), and a queued one
    // a newer size read has replaced never runs at all (the `size_builds` lifecycle flag).
    let mut size_build_gate = DocumentBuildGate::new();
    let lifecycle_storage_path = storage_path;
    // Unbounded on purpose, and bounded in practice. The loop is the socket's only writer, so a
    // client that stops reading backs the write up in the outbound arm — and while the loop is
    // suspended there it is not reading frames either, so no NEW request can be admitted. What can
    // still accumulate is the work already in flight: one response per in-flight request, plus one
    // push per still-building import of the documents already being analysed. That is bounded by
    // the requests the client itself issued, not by anything the daemon does on its own. A bounded
    // channel would instead make a slow client able to stall a *producer* — the very coupling this
    // channel exists to remove.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<OutboundFrame>();
    // Every task this connection spawns: request handlers, streamed-import builds, SWR
    // revalidations. Shutdown and idle-recycle join them, so nothing is still writing to the cache
    // after the flush.
    let mut active_tasks: Vec<JoinHandle<()>> = Vec::new();

    loop {
        let payload = tokio::select! {
            outbound = outbound_rx.recv() => {
                // The loop itself holds a sender, so `recv` cannot return None here.
                if let Some(frame) = outbound
                    && let Err(error) = framed.send(frame).await
                {
                    // The socket failed under us. The client is not owed the rest of its queue, but
                    // the cache is owed everything this session measured.
                    close_connection(
                        &service,
                        &prefetcher,
                        &lifecycles,
                        &mut active_tasks,
                        &mut _maintenance_task,
                    )
                    .await;
                    return Err(Box::new(error));
                }
                continue;
            }
            payload = framed.next() => match payload.transpose() {
                Ok(payload) => payload,
                Err(error) => {
                    close_connection(
                        &service,
                        &prefetcher,
                        &lifecycles,
                        &mut active_tasks,
                        &mut _maintenance_task,
                    )
                    .await;
                    return Err(Box::new(error));
                }
            },
            _ = tokio::time::sleep(LIFECYCLE_CHECK_INTERVAL) => {
                // Byte-budget maintenance runs on its own interval task (spawned
                // at Hello); this arm only checks for an idle recycle.
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
        // EOF: the client is gone — it closed the pipe, or the extension host crashed without ever
        // sending `shutdown`. There is nobody left to answer, and every import this session
        // measured is still owed to the cache.
        let Some(payload) = payload else {
            close_connection(
                &service,
                &prefetcher,
                &lifecycles,
                &mut active_tasks,
                &mut _maintenance_task,
            )
            .await;
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
                // Recency seed (C5 / Finding 10d, §3.3): lift the process-global
                // recency clock above every persisted shard's max seq BEFORE any
                // request can create a new entry. The clock resets to 1 each process
                // start, so without this a fresh post-restart access (small seq)
                // could sort as older than an untouched prior-session shard, letting
                // the evictor pick the active project as its victim. Run inline here
                // (like the legacy-cache removal above): the connection loop
                // processes this Hello to completion before it reads the first
                // Batch/Analyze frame, so the seed is guaranteed to finish before the
                // first `analyze_and_cache`.
                let seed_started_at = Instant::now();
                service.seed_recency_clock_from_disk();
                logging::log_debug(
                    "cache",
                    format!(
                        "hello recency seed finished in {}ms",
                        seed_started_at.elapsed().as_millis()
                    ),
                );
                // Byte-budget enforcement + compaction: a detached interval task
                // whose first pass runs right after this handshake (via
                // spawn_blocking — the old inline cleanup blocked the handshake on
                // full shard scans). Replacing the handle aborts the previous
                // task if a client re-handshakes.
                _maintenance_task = Some(spawn_cache_maintenance(std::sync::Arc::clone(&service)));
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
            ClientMessage::Batch(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                if request.version >= 2 && request.streaming {
                    let request_for_error = request.clone();
                    let (partial_tx, partial_rx) = mpsc::unbounded_channel();
                    let response_handle = tokio::task::spawn_blocking(move || {
                        svc.handle_batch_streaming(request, move |partial| {
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
                            protocol_error_batch_response,
                        ),
                    );
                } else {
                    spawn_request(
                        &mut active_tasks,
                        &outbound_tx,
                        request.clone(),
                        protocol_error_batch_response,
                        move || svc.handle_batch(request),
                    );
                }

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
            ClientMessage::Batch(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_batch_response(
                        &request,
                        "hello message not received".to_owned(),
                    ),
                );
            }
            ClientMessage::AnalyzeDocument(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                // A newer analysis of the same document supersedes this one: its still-queued
                // builds stop before they start. Keyed per document, like the SWR lifecycle, so
                // analyzing one file never cancels another's pending imports.
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
                    let response_handle = tokio::task::spawn_blocking(move || {
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
                // NOT streamed, deliberately (SRS FR-004b): both callers are one-shot commands
                // with no per-import rows for a push to merge into, and a comparison assembled
                // from half-measured imports is worse than "comparison failed". It therefore
                // still waits for every engine miss it names — the one request in the daemon that
                // does — and its total time is bounded only by `BUILD_TIMEOUT` per build. What it
                // no longer does is hold the connection: it runs as a task like everything else.
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
                service.invalidate_package(&message.package_name);
            }
            ClientMessage::CacheInvalidateAll(_) if hello_received => {
                prefetcher.cancel();
                service.invalidate_all();
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

                // A newer bulk request for the SAME source manifest supersedes
                // the previous block: flip that block's cancel flag so its still-
                // queued jobs skip their remaining fetches. Blocks for other
                // manifests keep draining. The returned flag governs THIS block;
                // each of its pool jobs re-reads it before its network fetch.
                // Absent source (older client) → all share the empty-key bucket,
                // preserving the pre-D10 connection-global supersede for them.
                let source = request.source.clone().unwrap_or_default();
                let cancelled = lifecycles.registry_refresh.start_new_block(&source);
                let final_targets = targets.clone();
                let target_count = targets.len();

                service.spawn_registry_refresh_block(
                    targets,
                    mode,
                    now_ms,
                    cancelled,
                    move |index, result| {
                        // A cancelled (skipped) job reports `None` and streams no
                        // partial; the collector fills its slot from the fallback.
                        if let Some(result) = result {
                            let _ = partial_tx.send((index, result));
                        }
                    },
                );
                let flush_service = std::sync::Arc::clone(&service);

                // Tracked like every other task (FR-004c): it owns the client's response AND the
                // registry snapshot flush, so a shutdown that did not join it could exit with the
                // fetched metadata unwritten and the response unsent.
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
                    // All refresh workers have finished or been skipped; persist
                    // any fetched metadata in one snapshot write.
                    flush_service.flush_registry_hints();

                    let results = ordered_results
                        .into_iter()
                        .zip(final_targets)
                        .map(|(result, target)| {
                            result.unwrap_or(RegistryHintResult {
                                target,
                                hint: None,
                                error: Some(
                                    "registry refresh worker did not return a result".to_owned(),
                                ),
                                origin: None,
                            })
                        })
                        .collect();

                    queue_outbound(
                        &outbound,
                        &RefreshRegistryHintsResponse {
                            version,
                            request_id,
                            results,
                            indexes: None,
                            error: None,
                            diagnostics: Vec::new(),
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
                // Tracked like every other task (FR-004c).
                let forwarder = tokio::spawn(async move {
                    let response = response_rx.await.unwrap_or_else(|_| {
                        workspace_report_protocol_error(
                            &request_for_error,
                            "workspace report worker stopped before sending a response",
                        )
                    });
                    queue_outbound(&outbound, &response);
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
            ClientMessage::FileSize(request) if hello_received => {
                prefetcher.cancel();
                lifecycle.record_batch();
                let svc = std::sync::Arc::clone(&service);
                spawn_request(
                    &mut active_tasks,
                    &outbound_tx,
                    request.clone(),
                    protocol_error_file_size_response,
                    move || svc.handle_file_size(request),
                );
            }
            ClientMessage::FileSize(request) => {
                queue_outbound(
                    &outbound_tx,
                    &protocol_error_file_size_response(
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
                // Only an interactive size read is bounded, and only those need to be: see
                // `CombinedBuildBound`.
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
                    &mut _maintenance_task,
                )
                .await;
                // Anything those tasks queued on their way out — a response, a last streamed
                // import — is still owed to a client that asked us to stop on its own terms, and
                // the loop is no longer in the select that would have written it. A client that
                // vanished is owed nothing, so the paths above do not drain.
                drain_outbound(&mut framed, &mut outbound_rx).await;
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

/// Registers a task the connection owns. Finished handles are pruned on each push, so a long-lived
/// connection does not accumulate them.
fn track_active_task(active_tasks: &mut Vec<JoinHandle<()>>, handle: JoinHandle<()>) {
    reap_finished_tasks(active_tasks);
    active_tasks.push(handle);
}

/// Drop the handles of tasks that have already finished.
///
/// It is not only housekeeping: `recycle_if_needed` reads `active_tasks.is_empty()` as "this
/// connection has nothing in flight", and a finished handle is indistinguishable from a running one
/// to `is_empty`. Since every request now runs as a task, a connection that has served ANY request
/// carries handles for ever, so without this the first recycle check always found work, deferred,
/// and the recycle waited a further 60s tick for no reason.
fn reap_finished_tasks(active_tasks: &mut Vec<JoinHandle<()>>) {
    active_tasks.retain(|active| !active.is_finished());
}

/// Join the tasks this connection spawned, giving up after [`TASK_JOIN_TIMEOUT`]. Returns whether
/// every one of them finished; the handles that did not are left in `active_tasks`.
///
/// Callers must cancel what they can BEFORE calling this (see `cancel_background_work`) — the wait
/// is for work that cannot be cancelled, not a substitute for cancelling.
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

/// The ONE way a connection stops serving, whichever end it comes to: the client's `shutdown`, the
/// client vanishing (EOF), or the socket failing.
///
/// Cancel → join under a deadline → **flush the cache unconditionally** (SRS FR-004c). It is one
/// function because it was three, and only one of them flushed: an extension host that crashed
/// (no `shutdown` — the daemon just reads EOF) took every import that session had measured with it.
/// The cache does not care WHY the connection ended.
///
/// Unconditional, even when a task outlived the join: whatever that task would have added is worth
/// one rebuild, and everything already computed is worth the session. And it does not rely on `Drop`
/// running before the process exits — `Drop` reaches only the entries already queued for the batched
/// commit, never a dirty one whose insert failed, nor the recency a session's cache hits earned.
async fn close_connection(
    service: &ImportLensService,
    prefetcher: &Prefetcher,
    lifecycles: &ConnectionLifecycles,
    active_tasks: &mut Vec<JoinHandle<()>>,
    maintenance_task: &mut Option<AbortOnDrop>,
) {
    // Abort the pending maintenance pass first: it is scheduled, not started, and a pass that
    // begins while we are flushing is compacting the shards the flush is writing to.
    *maintenance_task = None;
    lifecycles.cancel_all(prefetcher);
    // Bounded: a build already inside Rolldown cannot be cancelled and runs to `BUILD_TIMEOUT`,
    // which is LONGER than the extension's force-kill grace — so joining it without a bound is how
    // a graceful shutdown loses its flush entirely.
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
/// Both halves run in ONE task, off the connection loop, which is what preserves the only ordering
/// this design depends on: the response goes out first — carrying every import the cache could
/// answer and a `loading` placeholder for the rest — and a pushed result can only update an import
/// state that response created. The outbound channel is FIFO per sender, so nothing can reorder
/// them.
///
/// Tracked by the caller, not detached: a client that disconnects mid-flight must not leave a build
/// still writing to the cache after the shutdown flush.
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
        let analysis_handle = tokio::task::spawn_blocking(move || {
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

        queue_outbound(&outbound, &analysis.response);

        if analysis.pending.is_empty() {
            return;
        }

        let request = request_for_error;
        let build = tokio::task::spawn_blocking(move || {
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
                            // The analysis request id IS the client's freshness generation for this
                            // document, so a push computed for a document the user has since edited
                            // is dropped by the same guard that drops a superseded SWR refresh.
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

/// Size a document, then — if anything was served stale — revalidate it in the background and push
/// the fresh results. One task, off the connection loop: the combined build this runs is the one
/// that used to hold the loop hostage while every streamed import queued up behind it.
///
/// Off the loop, but not unbounded — for an interactive read. `combined_build` admits ONE combined
/// build per document at a time and drops the build of a size read a newer keystroke has already
/// replaced. Without it, every keystroke in a document with imports stacked another combined build
/// against the two-permit engine pool, since the day the handlers started running concurrently. See
/// [`CombinedBuildBound`].
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
        // Waiting HERE, and not in the engine, is the point: an engine permit is daemon-wide, so a
        // combined build queued for one is not holding anything back — right up until it gets one,
        // after which it holds it for as long as its build takes.
        let mut permit = None;

        if let Some(bound) = combined_build {
            let Ok(acquired) = Arc::clone(&bound.gate).acquire_owned().await else {
                // The gate is never closed; unreachable in practice.
                return;
            };
            permit = Some(acquired);

            if bound.superseded.load(Ordering::Acquire) {
                // The user typed past this size read while it waited. Building now would measure a
                // document state nobody is looking at, so it is answered with an error instead —
                // which the client drops on the same generation guard that drops a superseded push
                // (FR-004a), and which costs it nothing: the newer read behind us in the queue is
                // about to produce the number it will actually show.
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
        // Streaming: the file's own totals still come from a real combined build, but the
        // per-import states are served from the cache and the misses come back `loading`.
        // The `AnalyzeDocument` the extension sent first is already building them.
        // A force-fresh request (CI) is served complete by this same call.
        let response_handle = tokio::task::spawn_blocking(move || {
            size_service.handle_file_size_document_streaming(request)
        });
        let response = response_from_join(
            response_handle,
            &request_for_error,
            protocol_error_file_size_document_response,
        )
        .await;
        // SWR: any served size flagged Stale was served from a changed cache entry. Recompute ONLY
        // those imports fresh in the background (a fresh sibling must not be re-analyzed) and push
        // the refreshed results to the client.
        let stale_specifiers = response
            .imports
            .iter()
            .filter(|result| matches!(result.freshness.kind, FreshnessKind::Stale))
            .map(|result| result.specifier.clone())
            .collect::<std::collections::HashSet<_>>();
        queue_outbound(&outbound, &response);
        // The combined build is done; the next size read for this document may start. What follows
        // is per-import revalidation, which has supersession and single-flight of its own.
        drop(permit);

        if stale_specifiers.is_empty() {
            return;
        }

        // F3-B pre-recompute cancellation is scoped to this document: a newer size read for the
        // same document supersedes the push, while unrelated prefetch/file-size work must not
        // starve SWR.
        let revalidation = tokio::task::spawn_blocking(move || {
            if let Some((workspace_root, document_path, results, identities)) = service
                .revalidate_document_sizes(&request_for_error, &stale_specifiers, || {
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
                        // Echo the generation so the client can drop this push if a newer analysis
                        // has since superseded the one it was computed for.
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
    on_error: impl FnOnce(&R, String) -> T + Send + 'static,
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
            response_from_join(response_handle, &request_for_error, on_error).await;
        queue_outbound(&outbound, &final_response);
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
    // A finished handle is not work in flight. Reap first, or a connection that has served any
    // request at all looks busy for ever and every recycle costs an extra 60s tick.
    reap_finished_tasks(active_tasks);
    if !active_tasks.is_empty() {
        wait_for_active_tasks(active_tasks).await;
        return false;
    }

    // Nothing is in flight, so nothing is competing with the flush — but the maintenance pass is
    // still scheduled, and a pass that starts during the flush compacts the shards it is writing.
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
        DocumentBuildGate, DocumentTaskLifecycle, RegistryRefreshLifecycle, TASK_JOIN_TIMEOUT,
        reap_finished_tasks, wait_for_active_tasks,
    };
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::task::JoinHandle;

    /// Shutdown must not be hostage to a build it cannot cancel.
    ///
    /// A build already inside Rolldown runs to `BUILD_TIMEOUT` (8s) whatever the daemon wants,
    /// and the extension force-kills the daemon 5s after asking it to stop — so a join that waits
    /// for that build is a join that ends with the process being killed before it flushes. The wait
    /// is therefore bounded, and the handles it gave up on are reported, not silently dropped.
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

    /// `recycle_if_needed` reads `active_tasks.is_empty()` as "nothing in flight". Now that every
    /// request runs as a task, a connection that served ONE request keeps its handle for ever
    /// unless finished handles are reaped — so the recycle check would find work that had long
    /// since completed and defer the recycle by a whole 60s tick, every time.
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

    /// Shutdown cancels before it joins. `Drop` cannot do this job: it runs when the connection
    /// function RETURNS, which is after the join it was meant to shorten.
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

    #[test]
    fn registry_refresh_lifecycle_supersedes_only_within_the_same_source() {
        let mut lifecycle = RegistryRefreshLifecycle::new();
        let first = lifecycle.start_new_block("web/package.json");
        assert!(!first.load(Ordering::Acquire), "a fresh block starts live");

        // A block for a DIFFERENT source (another package.json in the same
        // workspace) must not cancel an unrelated in-flight block — otherwise a
        // cold-cache multi-manifest prewarm loses the first manifest's hints
        // (regression P1-5 / decision-log D11).
        let other = lifecycle.start_new_block("backend/package.json");
        assert!(
            !first.load(Ordering::Acquire),
            "a block for a different source must not cancel another source's block"
        );
        assert!(!other.load(Ordering::Acquire), "the new block starts live");

        // A newer bulk request for the SAME source still supersedes the block it
        // replaces: the prior flag flips, the new one starts live.
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
