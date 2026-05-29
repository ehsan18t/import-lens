use import_lens_daemon::ipc::codec::{FrameDecoder, decode_payload, encode_frame};
use import_lens_daemon::ipc::protocol::{ClientMessage, ShutdownMessage};

#[test]
fn encode_frame_writes_big_endian_payload_length() {
    let frame = encode_frame(&ShutdownMessage {
        message_type: "shutdown".to_owned(),
    })
    .expect("message should encode");
    let payload_len = u32::from_be_bytes(frame[0..4].try_into().expect("header length")) as usize;

    assert_eq!(payload_len, frame.len() - 4);
}

#[test]
fn frame_decoder_buffers_partial_frames() {
    let first = encode_frame(&ShutdownMessage {
        message_type: "shutdown".to_owned(),
    })
    .expect("first message should encode");
    let second = encode_frame(&ShutdownMessage {
        message_type: "shutdown".to_owned(),
    })
    .expect("second message should encode");
    let mut decoder = FrameDecoder::default();

    assert!(
        decoder
            .push(&first[..3])
            .expect("partial frame should be accepted")
            .is_empty()
    );

    let frames = decoder
        .push(&[&first[3..], second.as_slice()].concat())
        .expect("complete frames should decode");

    assert_eq!(frames.len(), 2);
    assert_eq!(
        decode_payload::<ClientMessage>(&frames[0]).expect("first payload should decode"),
        ClientMessage::Shutdown(ShutdownMessage {
            message_type: "shutdown".to_owned()
        })
    );
}
