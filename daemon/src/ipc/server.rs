use crate::{
    ipc::{
        codec::{FrameDecoder, decode_payload, encode_frame},
        protocol::ClientMessage,
    },
    service::ImportLensService,
};
use std::{error::Error, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(windows)]
use tokio::net::windows::named_pipe::ServerOptions;

#[cfg(windows)]
pub async fn run_server(pipe_name: &str, _workspace_root: PathBuf) -> Result<(), Box<dyn Error>> {
    let mut pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe_name)?;
    pipe.connect().await?;

    let mut decoder = FrameDecoder::default();
    let mut service = std::sync::Arc::new(ImportLensService::new());
    let mut hello_received = false;
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let read = pipe.read(&mut buffer).await?;

        if read == 0 {
            break;
        }

        for payload in decoder.push(&buffer[..read])? {
            let message = decode_payload::<ClientMessage>(&payload)?;

            match message {
                ClientMessage::Hello(_) => {
                    service = std::sync::Arc::new(ImportLensService::new());
                    hello_received = true;
                }
                ClientMessage::Batch(request) if hello_received => {
                    let svc = std::sync::Arc::clone(&service);
                    let response = tokio::task::spawn_blocking(move || svc.handle_batch(request))
                        .await
                        .expect("spawn_blocking failed");
                    pipe.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::CacheInvalidate(message) if hello_received => {
                    service.invalidate_package(&message.package_name);
                }
                ClientMessage::CacheInvalidateAll(_) if hello_received => {
                    service.invalidate_all();
                }
                ClientMessage::Shutdown(_) => {
                    return Ok(());
                }
                ClientMessage::Batch(request) => {
                    let response = tokio::task::spawn_blocking(move || {
                        ImportLensService::new().handle_batch(request)
                    })
                    .await
                    .expect("spawn_blocking failed");
                    pipe.write_all(&encode_frame(&response)?).await?;
                }
                ClientMessage::CacheInvalidate(_) | ClientMessage::CacheInvalidateAll(_) => {}
            }
        }
    }

    Ok(())
}

#[cfg(not(windows))]
pub async fn run_server(_pipe_name: &str, _workspace_root: PathBuf) -> Result<(), Box<dyn Error>> {
    Err("native server is only implemented for Windows in this alpha".into())
}
