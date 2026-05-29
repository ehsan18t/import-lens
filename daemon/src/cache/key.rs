use crate::ipc::protocol::{ImportKind, ImportRequest};

pub fn cache_key_for_import(request: &ImportRequest) -> String {
    let exports = match request.import_kind {
        ImportKind::Default => "default".to_owned(),
        ImportKind::Namespace => "*".to_owned(),
        ImportKind::Dynamic => "dynamic".to_owned(),
        ImportKind::Named => {
            let mut named = request.named.clone();
            named.sort();
            named.join(",")
        }
    };

    format!("{}@{}::{}", request.specifier, request.version, exports)
}
