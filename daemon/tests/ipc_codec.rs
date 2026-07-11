use bytes::BytesMut;
use import_lens_daemon::ipc::codec::{
    FrameDecoder, MAX_FRAME_BYTES, decode_payload, encode_frame, message_frame_codec,
};
use import_lens_daemon::ipc::protocol::{
    CacheRemoveScope, ClientMessage, ImportKind, ImportRequest, PROTOCOL_VERSION, ShutdownMessage,
};
use tokio_util::codec::Decoder;

const OVERSIZED_FRAME_BYTES: usize = MAX_FRAME_BYTES + 1;

/// serde_json is compiled with `arbitrary_precision` (forced globally by
/// rolldown_common's hard dependency), which makes `Value` numbers serialize
/// as private marker maps — NOT the msgpack integers a real client sends.
/// Mirror `json!` payloads through plain serde types before encoding.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum WireValue {
    Null(()),
    Bool(bool),
    UInt(u64),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<WireValue>),
    Map(std::collections::BTreeMap<String, WireValue>),
}

impl From<&serde_json::Value> for WireValue {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => WireValue::Null(()),
            serde_json::Value::Bool(flag) => WireValue::Bool(*flag),
            serde_json::Value::Number(number) => number
                .as_u64()
                .map(WireValue::UInt)
                .or_else(|| number.as_i64().map(WireValue::Int))
                .or_else(|| number.as_f64().map(WireValue::Float))
                .expect("test payload numbers should be representable"),
            serde_json::Value::String(text) => WireValue::Str(text.clone()),
            serde_json::Value::Array(items) => {
                WireValue::Array(items.iter().map(WireValue::from).collect())
            }
            serde_json::Value::Object(map) => WireValue::Map(
                map.iter()
                    .map(|(key, item)| (key.clone(), WireValue::from(item)))
                    .collect(),
            ),
        }
    }
}

fn msgpack(value: &serde_json::Value) -> Vec<u8> {
    rmp_serde::to_vec(&WireValue::from(value)).expect("test payload should encode")
}

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
fn length_delimited_codec_rejects_oversized_frames_before_payload() {
    let mut codec = message_frame_codec();
    let mut input = BytesMut::new();
    input.extend_from_slice(&(OVERSIZED_FRAME_BYTES as u32).to_be_bytes());

    let error = codec
        .decode(&mut input)
        .expect_err("oversized frame should be rejected");

    assert!(
        error.to_string().contains("frame size too big") || error.to_string().contains("too large"),
        "unexpected error: {error}",
    );
}

#[test]
fn import_request_defaults_missing_runtime_to_component() {
    let payload = msgpack(&serde_json::json!({
        "specifier": "tiny-lib",
        "package": "tiny-lib",
        "version": "1.0.0",
        "named": ["value"],
        "import_kind": "named"
    }));

    let request: ImportRequest =
        rmp_serde::from_slice(&payload).expect("legacy request should decode");

    assert_eq!(request.specifier, "tiny-lib");
    assert_eq!(request.import_kind, ImportKind::Named);
    assert_eq!(request.runtime.as_str(), "component");
}

#[test]
fn client_message_decodes_daemon_first_v7_requests() {
    assert_eq!(PROTOCOL_VERSION, 7);
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
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "refresh_registry_hints",
            "version": PROTOCOL_VERSION,
            "request_id": 6,
            "targets": [{"name": "react", "installedVersion": "18.2.0"}],
            "mode": "refresh_stale"
        })),
        ClientMessage::RefreshRegistryHints(_),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "workspace_report",
            "version": PROTOCOL_VERSION,
            "request_id": 7,
            "workspace_root": "C:/workspace"
        })),
        ClientMessage::WorkspaceReport(_),
    ));
}

#[test]
fn client_message_decodes_cache_management_requests() {
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "hello",
            "version": PROTOCOL_VERSION,
            "workspace_root": "C:/workspace",
            "storage_path": "C:/Code/User/workspaceStorage/importlens/daemon-cache",
            "enable_disk_cache": true,
            "cache_max_size_mb": 512,
            "registry_cache_max_size_mb": 64,
            "log_level": "info"
        })),
        ClientMessage::Hello(message)
            if message.cache_max_size_mb == 512 && message.registry_cache_max_size_mb == 64,
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "hello",
            "version": PROTOCOL_VERSION,
            "workspace_root": "C:/workspace",
            "storage_path": "C:/Code/User/workspaceStorage/importlens/daemon-cache",
            "enable_disk_cache": true,
            "cache_max_size_mb": 512,
            "log_level": "info"
        })),
        ClientMessage::Hello(message)
            if message.registry_cache_max_size_mb == 32,
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "cache_status",
            "version": PROTOCOL_VERSION,
            "request_id": 21,
            "workspace_root": "C:/workspace"
        })),
        ClientMessage::CacheStatus(message)
            if message.request_id == 21 && message.workspace_root.as_deref() == Some("C:/workspace"),
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "cache_list",
            "version": PROTOCOL_VERSION,
            "request_id": 23
        })),
        ClientMessage::CacheList(message) if message.request_id == 23,
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "cache_remove",
            "version": PROTOCOL_VERSION,
            "request_id": 24,
            "scope": "current_project",
            "workspace_root": "C:/workspace"
        })),
        ClientMessage::CacheRemove(message)
            if message.request_id == 24 && message.scope == CacheRemoveScope::CurrentProject,
    ));
    assert!(matches!(
        decode_client_message(serde_json::json!({
            "type": "cache_remove",
            "version": PROTOCOL_VERSION,
            "request_id": 25,
            "scope": "registry"
        })),
        ClientMessage::CacheRemove(message)
            if message.request_id == 25 && message.scope == CacheRemoveScope::Registry,
    ));
}

fn decode_client_message(value: serde_json::Value) -> ClientMessage {
    rmp_serde::from_slice(&msgpack(&value)).expect("message should decode")
}
