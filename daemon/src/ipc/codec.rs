use serde::{Serialize, de::DeserializeOwned};
use std::{error::Error, fmt};

const FRAME_HEADER_BYTES: usize = 4;
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum IpcCodecError {
    MessagePackEncode(String),
    MessagePackDecode(String),
    FrameTooLarge(usize),
}

impl fmt::Display for IpcCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessagePackEncode(message) => {
                write!(formatter, "MessagePack encode failed: {message}")
            }
            Self::MessagePackDecode(message) => {
                write!(formatter, "MessagePack decode failed: {message}")
            }
            Self::FrameTooLarge(size) => write!(formatter, "IPC frame is too large: {size} bytes"),
        }
    }
}

impl Error for IpcCodecError {}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, IpcCodecError> {
    let payload = rmp_serde::to_vec_named(message)
        .map_err(|error| IpcCodecError::MessagePackEncode(error.to_string()))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(IpcCodecError::FrameTooLarge(payload.len()));
    }
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| IpcCodecError::FrameTooLarge(payload.len()))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, IpcCodecError> {
    rmp_serde::from_slice(payload)
        .map_err(|error| IpcCodecError::MessagePackDecode(error.to_string()))
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, IpcCodecError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        loop {
            if self.buffer.len() < FRAME_HEADER_BYTES {
                break;
            }

            let payload_len = u32::from_be_bytes(
                self.buffer[0..FRAME_HEADER_BYTES]
                    .try_into()
                    .expect("frame header slice is exactly 4 bytes"),
            ) as usize;
            if payload_len > MAX_FRAME_BYTES {
                self.buffer.clear();
                return Err(IpcCodecError::FrameTooLarge(payload_len));
            }

            let frame_len = FRAME_HEADER_BYTES
                .checked_add(payload_len)
                .ok_or(IpcCodecError::FrameTooLarge(payload_len))?;

            if self.buffer.len() < frame_len {
                break;
            }

            frames.push(self.buffer[FRAME_HEADER_BYTES..frame_len].to_vec());
            self.buffer.drain(0..frame_len);
        }

        Ok(frames)
    }
}
