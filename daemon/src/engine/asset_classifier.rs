use std::path::Path;

use super::AssetKind;

/// What a non-JavaScript file ships as, or `None` when the engine should leave it to Rolldown.
///
/// The same classification is used at both discovery boundaries: JavaScript graph imports in the
/// Rolldown plugin and local resources referenced by a bundled stylesheet. Keeping one vocabulary
/// prevents a font from being intercepted in one path and silently ignored in the other.
pub(crate) fn classify_asset(path: &Path) -> Option<AssetKind> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;

    match extension.as_str() {
        "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" => {
            Some(AssetKind::Css)
        }
        "wasm" => Some(AssetKind::Wasm),
        "woff" | "woff2" | "ttf" | "otf" | "eot" => Some(AssetKind::Font),
        _ => None,
    }
}
