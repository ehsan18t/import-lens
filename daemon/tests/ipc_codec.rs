use import_lens_daemon::ipc::codec::{FrameDecoder, decode_payload, encode_frame};
use import_lens_daemon::ipc::protocol::{
    ClientMessage, ImportKind, ImportRequest, PROTOCOL_VERSION, ShutdownMessage,
};

const OVERSIZED_FRAME_BYTES: usize = (32 * 1024 * 1024) + 1;

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

#[test]
fn frame_decoder_rejects_oversized_frames_before_buffering_payload() {
    let mut decoder = FrameDecoder::default();
    let mut header = Vec::new();
    header.extend_from_slice(&(OVERSIZED_FRAME_BYTES as u32).to_be_bytes());

    let error = decoder
        .push(&header)
        .expect_err("oversized frame should be rejected");

    assert!(
        error.to_string().contains("too large"),
        "unexpected error: {error}"
    );
}

#[test]
fn import_request_defaults_missing_runtime_to_component() {
    let payload = rmp_serde::to_vec(&serde_json::json!({
        "specifier": "tiny-lib",
        "package": "tiny-lib",
        "version": "1.0.0",
        "named": ["value"],
        "import_kind": "named"
    }))
    .expect("legacy request should encode");

    let request: ImportRequest =
        rmp_serde::from_slice(&payload).expect("legacy request should decode");

    assert_eq!(request.specifier, "tiny-lib");
    assert_eq!(request.import_kind, ImportKind::Named);
    assert_eq!(request.runtime.as_str(), "component");
}

#[test]
fn client_message_decodes_daemon_first_v5_requests() {
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "analyze_document",
            "version": PROTOCOL_VERSION,
            "request_id": 1,
            "workspace_root": "C:/workspace",
            "active_document_path": "C:/workspace/src/index.ts",
            "source": "import x from 'tiny-lib';"
        })),
        ClientMessage::AnalyzeDocument(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "analyze_package_json",
            "version": PROTOCOL_VERSION,
            "request_id": 2,
            "workspace_root": "C:/workspace",
            "active_document_path": "C:/workspace/package.json",
            "source": "{\"dependencies\":{\"tiny-lib\":\"^1.0.0\"}}",
            "include_registry_hints": false,
            "force_registry_refresh": false
        })),
        ClientMessage::AnalyzePackageJson(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "analyze_specifiers",
            "version": PROTOCOL_VERSION,
            "request_id": 3,
            "workspace_root": "C:/workspace",
            "active_document_path": "C:/workspace/src/index.ts",
            "specifiers": ["tiny-lib"]
        })),
        ClientMessage::AnalyzeSpecifiers(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "file_size_document",
            "version": PROTOCOL_VERSION,
            "request_id": 4,
            "workspace_root": "C:/workspace",
            "active_document_path": "C:/workspace/src/index.ts",
            "source": "import x from 'tiny-lib';"
        })),
        ClientMessage::FileSizeDocument(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "complete_import_members",
            "version": PROTOCOL_VERSION,
            "request_id": 5,
            "workspace_root": "C:/workspace",
            "active_document_path": "C:/workspace/src/index.ts",
            "source": "import { value } from 'tiny-lib';",
            "cursor_offset": 9
        })),
        ClientMessage::CompleteImportMembers(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "node_modules_changed",
            "package_json_paths": ["C:/workspace/node_modules/tiny-lib/package.json"]
        })),
        ClientMessage::NodeModulesChanged(_),
    ));
}

fn decode_client_message(value: serde_json::Value) -> ClientMessage {
    let payload = rmp_serde::to_vec(&value).expect("message should encode");
    rmp_serde::from_slice(&payload).expect("message should decode")
}
