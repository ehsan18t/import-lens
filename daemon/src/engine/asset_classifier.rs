use std::path::Path;

use super::AssetKind;

/// What the engine should do with a non-JavaScript file the graph imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetClass {
    /// Processed the way it ships and folded into the size (B2).
    Counted(AssetKind),
    /// Ships as its own file, but outside the measured taxonomy — an image, an icon, a media file,
    /// a compiled native addon.
    ///
    /// It must still be intercepted. Left to Rolldown, a `.png` is not UTF-8 and its loader fails
    /// on `InvalidData`; an `.svg` IS valid UTF-8, so it is handed to OXC and parsed as JavaScript,
    /// which fails differently. Either way ONE such import made the entire package unmeasurable —
    /// the user saw "unavailable" for a package whose JavaScript we could measure perfectly.
    /// Stubbing it lets the JS graph measure and leaves the bytes to be disclosed.
    Unmeasured,
}

/// What a non-JavaScript file ships as, or `None` when the engine should leave it to Rolldown.
///
/// The same classification is used at both discovery boundaries: JavaScript graph imports in the
/// Rolldown plugin and local resources referenced by a bundled stylesheet. Keeping one vocabulary
/// prevents a font from being intercepted in one path and silently ignored in the other.
///
/// The `Unmeasured` list is deliberately an allowlist of extensions a bundler's file loader really
/// emits, not a catch-all. An unknown extension still falls through to Rolldown, which is the
/// conservative behaviour: intercepting something we cannot name would stub a module that might
/// have been real JavaScript.
///
/// `.node` earns its place on that allowlist by the strongest form of the same argument: it is not
/// merely unlikely to be JavaScript, it cannot be. Node resolves the extension through
/// `process.dlopen`, so a `.node` file is a compiled native addon by definition of its name — there
/// is no spelling of it that OXC was ever going to parse. It is classified here, and not at some
/// per-package exception, because that is what makes it universal: `keytar`, `@node-rs/crc32` and
/// every addon nobody has hit yet are one rule, not three.
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
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "ico" | "bmp" | "mp4"
        | "webm" | "mp3" | "wav" | "ogg" | "node" => Some(AssetClass::Unmeasured),
        _ => None,
    }
}
