use std::path::Path;

use super::AssetKind;

/// What the engine should do with a non-JavaScript file the graph imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetClass {
    /// Processed the way it ships and folded into the size.
    Counted(AssetKind),
    /// Ships as its own file, but outside the measured taxonomy: a media file or a compiled native
    /// addon.
    ///
    /// It must still be intercepted. Left to Rolldown, a `.png` fails its UTF-8 loader and an
    /// `.svg` is parsed as JavaScript; either way one such import would make the whole package
    /// unmeasurable. Stubbing it lets the JS graph measure and leaves the bytes to be disclosed.
    Unmeasured,
}

/// What a non-JavaScript file ships as, or `None` when the engine should leave it to Rolldown.
///
/// The same classification is used at both discovery boundaries: JavaScript graph imports in the
/// Rolldown plugin and local resources referenced by a bundled stylesheet. Keeping one vocabulary
/// prevents a font from being intercepted in one path and silently ignored in the other.
///
/// The `Unmeasured` list is deliberately an allowlist of extensions a bundler's file loader really
/// emits, not a catch-all: an unknown extension falls through to Rolldown, because stubbing
/// something we cannot name might drop real JavaScript.
///
/// `.node` cannot be JavaScript: Node loads the extension through `process.dlopen`, so it is a
/// compiled native addon by name. One rule here covers every addon instead of per-package
/// exceptions.
pub(crate) fn classify_asset_class(path: &Path) -> Option<AssetClass> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;

    match extension.as_str() {
        "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" => {
            Some(AssetClass::Counted(AssetKind::Css))
        }
        "wasm" => Some(AssetClass::Counted(AssetKind::Wasm)),
        "woff" | "woff2" | "ttf" | "otf" | "eot" => Some(AssetClass::Counted(AssetKind::Font)),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "ico" | "bmp" => {
            Some(AssetClass::Counted(AssetKind::Image))
        }
        "mp4" | "webm" | "mp3" | "wav" | "ogg" | "node" => Some(AssetClass::Unmeasured),
        _ => None,
    }
}
