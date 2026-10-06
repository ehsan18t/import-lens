use bytes::Bytes;
use serde::{Serialize, de::DeserializeOwned};
use std::{error::Error, fmt};
use tokio_util::codec::LengthDelimitedCodec;

const FRAME_HEADER_BYTES: usize = 4;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum IpcCodecError {
    MessagePackEncode(String),
    MessagePackDecode(String),
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
        }
    }
}

impl Error for IpcCodecError {}

pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, IpcCodecError> {
    rmp_serde::to_vec_named(message)
        .map_err(|error| IpcCodecError::MessagePackEncode(error.to_string()))
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, IpcCodecError> {
    rmp_serde::from_slice(payload)
        .map_err(|error| IpcCodecError::MessagePackDecode(error.to_string()))
}

pub fn message_frame_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_length(FRAME_HEADER_BYTES)
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

pub fn payload_bytes<T: Serialize>(message: &T) -> Result<Bytes, IpcCodecError> {
    Ok(Bytes::from(encode_payload(message)?))
}
