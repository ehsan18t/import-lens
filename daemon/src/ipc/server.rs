use crate::{
    ipc::{
        codec::{FrameDecoder, decode_payload, encode_frame},
        protocol::ClientMessage,
    },
    lifecycle::{LifecycleState, record_recycle_timestamp},
    prefetch::Prefetcher,
    service::{ImportLensService, protocol_error_batch_response},
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let (stream, _) = listener.accept().await?;

    let service = std::sync::Arc::new(ImportLensService::new(None, false));
    let prefetcher = Prefetcher::new();

    handle_connection(stream, storage_path, service, prefetcher).await
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
                if recycle_if_needed(&lifecycle, service.cache_len(), storage_path.as_deref()) {
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
                    let hello_storage_path = PathBuf::from(&hello.storage_path);
                    service = std::sync::Arc::new(ImportLensService::new(
                        Some(hello_storage_path.clone()),
                        hello.enable_disk_cache,
                    ));
                    storage_path = Some(hello_storage_path);
                    hello_received = true;

                    if recycle_if_needed(&lifecycle, service.cache_len(), storage_path.as_deref()) {
                        return Ok(());
                    }
                }
                ClientMessage::Batch(request) if hello_received => {
                    prefetcher.cancel();
                    lifecycle.record_batch();
                    let svc = std::sync::Arc::clone(&service);
                    if request.version >= 2 && request.streaming {
                        let responses = tokio::task::spawn_blocking(move || {
                            svc.handle_batch_streaming(request)
                        })
                        .await
                        .expect("spawn_blocking failed");
                        for response in responses {
                            stream.write_all(&encode_frame(&response)?).await?;
                        }
                    } else {
                        let response =
                            tokio::task::spawn_blocking(move || svc.handle_batch(request))
                                .await
                                .expect("spawn_blocking failed");
                        stream.write_all(&encode_frame(&response)?).await?;
                    }

                    if recycle_if_needed(&lifecycle, service.cache_len(), storage_path.as_deref()) {
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
                ClientMessage::Shutdown(_) => {
                    return Ok(());
                }
                ClientMessage::PrewarmPackageJson(_)
                | ClientMessage::CacheInvalidate(_)
                | ClientMessage::CacheInvalidateAll(_) => {}
            }
        }
    }

    Ok(())
}

fn recycle_if_needed(
    lifecycle: &LifecycleState,
    cache_len: usize,
    storage_path: Option<&Path>,
) -> bool {
    let Some(reason) = lifecycle.should_recycle(Instant::now(), cache_len) else {
        return false;
    };

    if let Some(storage_path) = storage_path
        && let Err(error) = record_recycle_timestamp(storage_path, SystemTime::now())
    {
        eprintln!("[import-lens-daemon] failed to record recycle timestamp: {error}");
    }

    eprintln!("[import-lens-daemon] lifecycle recycle requested: {reason:?}");
    true
}
