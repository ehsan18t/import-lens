use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use lightningcss::dependencies::{Dependency, ImportDependency, UrlDependency};

use crate::engine::{AssetClass, AssetKind, CollectedAsset, UncountedAsset, classify_asset_class};

/// Resolve the local files referenced by `url()` in a bundled stylesheet.
///
/// Lightning CSS reports the source file for every reference, including rules originating in an
/// `@import` child. That source, not the synthetic union entry, is the base a bundler resolves
/// from.
///
/// Every reference lands in exactly one field: a resource this package ships is counted, disclosed
/// with its bytes, or named as an omission, never dropped. A runtime-fetched resource is not this
/// import's cost (ADR-0004) and is disclosed as `external`.
pub(super) struct CssDependencyAssets {
    pub assets: Vec<CollectedAsset>,
    pub failures: Vec<CssDependencyFailure>,
    /// Resolvable local files outside the counted CSS/wasm/font taxonomy (an image, an SVG). Their
    /// bytes ship, so they are disclosed at their real size and left out of the total.
    pub uncounted: Vec<UncountedAsset>,
    /// Local resources that ship but could not be located, read, or inspected, so not even their
    /// size is known. These make the result a floor: bytes are missing and the magnitude is not.
    pub omissions: Vec<String>,
    /// Resources fetched over the network at runtime: not bytes this package ships, so the measured
    /// size stays EXACT and keeps its budget verdict. Disclosed on the durable, budgetable
    /// `external` stage, never on a precision stage that would refuse to judge an exact number.
    pub external: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CssDependencyFailure {
    pub path: PathBuf,
    pub raw_bytes: u64,
    pub message: String,
}

enum SupportedAsset {
    Collected(CollectedAsset),
    Unreadable(CssDependencyFailure),
    Uncounted(UncountedAsset),
    Omitted(String),
    /// Fetched over the network at runtime, so not bytes this package ships. The measured size
    /// stays exact and keeps its budget verdict.
    External(String),
}

/// `stat` must record what it observes in the build's read ledger: it is the only freshness input a
/// disclosed-by-size resource has.
pub(super) fn collect_referenced_assets(
    dependencies: impl IntoIterator<Item = Dependency>,
    stat: &impl Fn(&Path) -> std::io::Result<fs::Metadata>,
    read_asset: &impl Fn(&Path, AssetKind) -> std::io::Result<CollectedAsset>,
    should_continue: &impl Fn() -> bool,
) -> CssDependencyAssets {
    let mut assets = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let mut uncounted = BTreeMap::new();
    let mut omissions = BTreeSet::new();
    let mut external = BTreeSet::new();
    let mut located = HashSet::new();

    let mut dependencies = dependencies.into_iter();
    while should_continue() {
        let Some(dependency) = dependencies.next() else {
            break;
        };
        match dependency {
            Dependency::Url(dependency) => {
                match collect_supported_asset(dependency, &mut located, stat, read_asset) {
                    Some(SupportedAsset::Collected(asset)) => {
                        assets.entry(asset.path.clone()).or_insert(asset);
                    }
                    Some(SupportedAsset::Unreadable(failure)) => {
                        failures.insert(failure.path.clone(), failure);
                    }
                    Some(SupportedAsset::Uncounted(asset)) => {
                        uncounted.entry(asset.path.clone()).or_insert(asset);
                    }
                    Some(SupportedAsset::Omitted(message)) => {
                        omissions.insert(message);
                    }
                    Some(SupportedAsset::External(message)) => {
                        external.insert(message);
                    }
                    // Nothing at all: a `data:` payload already inside the counted CSS text, a bare
                    // fragment pointing at the current document, or a file already located.
                    None => {}
                }
            }
            // Local `@import`s were already inlined by the bundler, so anything surviving this print
            // is an external stylesheet whose bytes are fetched at runtime, or a `data:` one already
            // counted inside the CSS text.
            Dependency::Import(dependency) => {
                if let Some(message) = external_import(dependency) {
                    external.insert(message);
                }
            }
        }
    }

    // Stopping early is abandoned work, not an absence of references: anything still queued is
    // named as an omission, so a short total never looks complete.
    let abandoned = dependencies.count();
    if abandoned > 0 {
        omissions.insert(format!(
            "{abandoned} CSS resource reference(s) were not examined because asset processing \
             stopped early, so any bytes they ship are not in this size"
        ));
    }

    CssDependencyAssets {
        assets: assets.into_values().collect(),
        failures: failures.into_values().collect(),
        uncounted: uncounted.into_values().collect(),
        omissions: omissions.into_iter().collect(),
        external: external.into_iter().collect(),
    }
}

/// A remote stylesheet is fetched rather than shipped by this package, so it is disclosed, never
/// counted.
fn external_import(dependency: ImportDependency) -> Option<String> {
    if dependency
        .url
        .trim()
        .to_ascii_lowercase()
        .starts_with("data:")
    {
        return None;
    }

    Some(format!(
        "external CSS import `{}` in {} is fetched at runtime and is not in this size",
        dependency.url,
        Path::new(&dependency.loc.file_path).display()
    ))
}

/// `located` holds every file path this collection has already examined. An icon font names the
/// same few files from dozens of rules, and each repeat would cost a canonicalize (a handle open on
/// Windows) and a stat for a file already in its bucket, its fingerprint already in the ledger.
fn collect_supported_asset(
    dependency: UrlDependency,
    located: &mut HashSet<PathBuf>,
    stat: &impl Fn(&Path) -> std::io::Result<fs::Metadata>,
    read_asset: &impl Fn(&Path, AssetKind) -> std::io::Result<CollectedAsset>,
) -> Option<SupportedAsset> {
    let specifier = dependency.url.trim();
    let source_file = Path::new(&dependency.loc.file_path);

    // No separate artifact: a `data:` payload is already inside the counted CSS text and a bare
    // fragment points at the current document. These are the ONLY references that may leave no
    // trace, so they are decided here explicitly rather than falling out of a failed parse.
    if specifier.is_empty()
        || specifier.starts_with('#')
        || specifier.to_ascii_lowercase().starts_with("data:")
    {
        return None;
    }

    // Externality is decided BEFORE the kind: a remote image and a shipped local image classify the
    // same, and only this check tells them apart.
    if is_remote_reference(specifier) {
        return Some(SupportedAsset::External(format!(
            "CSS resource `{}` in {} is fetched at runtime and is not in this size",
            dependency.url,
            source_file.display()
        )));
    }

    // A reference we cannot turn into a path still names bytes that ship, so it is an OMISSION and
    // never a silent `None`. Percent-escapes that do not decode to UTF-8 reach here (a CP-1252
    // export naming `Ubuntu-R%E9gular.woff2`); dropping one would lose a font face from a total
    // still reported Measured at High confidence.
    let Some(resource_path) = resource_path(specifier) else {
        return Some(SupportedAsset::Omitted(format!(
            "CSS resource `{}` in {} could not be interpreted as a file name, so its shipped bytes \
             are not in this size",
            dependency.url,
            source_file.display()
        )));
    };

    let resource = Path::new(&resource_path);
    if resource.has_root() || !source_file.is_absolute() {
        return Some(SupportedAsset::Omitted(format!(
            "CSS resource `{}` in {} is not package-relative, so its shipped bytes could not be \
             located",
            dependency.url,
            source_file.display()
        )));
    }

    let path = source_file.parent()?.join(resource);
    if !located.insert(path.clone()) {
        return None;
    }
    let path = fs::canonicalize(&path).unwrap_or(path);
    let metadata = stat(&path);
    let raw_bytes = metadata.as_ref().map_or(0, |metadata| metadata.len());

    // A resolvable path that cannot be stat'd is `Unreadable`, NOT `Omitted`: it names a file, and
    // `stat` has already recorded its absence or failure in the ledger, so ADDING the missing file
    // invalidates the result. `Omitted` is reserved for references that never resolved to a path.
    let unreadable = |message: String| {
        Some(SupportedAsset::Unreadable(CssDependencyFailure {
            message,
            path: path.clone(),
            raw_bytes,
        }))
    };
    if metadata.is_err() {
        return unreadable(format!(
            "CSS resource {} could not be read, so its shipped bytes are not in this size",
            path.display()
        ));
    }

    // Outside the counted taxonomy (an image, an SVG): the bytes ship, so they are disclosed at
    // full size, and `stat` above expires that disclosure when the file changes. Only a wasm or
    // font is counted here; a `url()` naming a stylesheet is not, because Lightning CSS inlines
    // `@import` children into the one bundled sheet and counting it again would double it.
    let counted_kind = classify_asset_class(&path).and_then(|class| match class {
        AssetClass::Counted(kind @ (AssetKind::Wasm | AssetKind::Font)) => Some(kind),
        _ => None,
    });
    let Some(kind) = counted_kind else {
        return Some(SupportedAsset::Uncounted(UncountedAsset {
            path,
            bytes: raw_bytes,
        }));
    };

    match read_asset(&path, kind) {
        Ok(asset) => Some(SupportedAsset::Collected(asset)),
        Err(error) => unreadable(format!(
            "failed to read CSS resource {}: {error}",
            path.display()
        )),
    }
}

/// Extract the filesystem-looking portion of a CSS resource URL. Query strings and fragments name
/// the same emitted file. Returns `None` when the reference cannot be turned into a file name; the
/// caller discloses that as an omission. The caller handles the `data:` and fragment-only cases.
fn resource_path(specifier: &str) -> Option<PathBuf> {
    let path_end = specifier.find(['?', '#']).unwrap_or(specifier.len());
    let path = decode_percent_encoded(&specifier[..path_end])?;
    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
}

/// A reference the browser fetches from elsewhere rather than one this package ships.
///
/// Protocol-relative (`//cdn/x.woff2`) counts; otherwise a CDN font would read as an unlocatable
/// local file and mark the size a floor. The one predicate for both discovery boundaries: `url()`
/// here, and `@import` through the stylesheet bundler's resolve.
pub(super) fn is_remote_reference(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("//") || has_url_scheme(value)
}

/// A single-letter "scheme" is a Windows drive (`C:\pkg\a.css`), which is how the synthetic union
/// entry spells every sheet it imports; reading it as a URL would externalize the whole set.
fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    scheme.len() > 1
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

/// CSS URLs percent-encode filenames independently of the host filesystem. Decode valid escapes
/// without treating an invalid literal `%` as a reason to drop the whole reference.
fn decode_percent_encoded(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some((high, low)) = bytes.get(index + 1).zip(bytes.get(index + 2))
            && let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low))
        {
            decoded.push((high << 4) | low);
            index += 3;
            continue;
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightningcss::dependencies::{Location, SourceRange};
    use std::cell::Cell;

    fn url_in(sheet: &Path, url: &str) -> Dependency {
        let at = Location { line: 1, column: 1 };
        Dependency::Url(UrlDependency {
            url: url.to_owned(),
            placeholder: String::new(),
            loc: SourceRange {
                file_path: sheet.to_string_lossy().into_owned(),
                start: at,
                end: at,
            },
        })
    }

    #[test]
    fn a_file_named_by_many_rules_is_located_once() {
        let dir = std::env::temp_dir().join(format!(
            "il-css-located-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).expect("fixture dir");
        let files = ["a.png", "b.png", "c.png"];
        for file in files {
            fs::write(dir.join(file), [7_u8; 16]).expect("fixture file");
        }
        let sheet = dir.join("index.css");
        let stats = Cell::new(0_usize);

        let collected = collect_referenced_assets(
            (0..50).map(|rule| url_in(&sheet, &format!("./{}", files[rule % files.len()]))),
            &|path| {
                stats.set(stats.get() + 1);
                fs::metadata(path)
            },
            &|path, _| panic!("an image is disclosed, never read: {}", path.display()),
            &|| true,
        );
        fs::remove_dir_all(&dir).ok();

        assert_eq!(stats.get(), files.len(), "one stat per file, not per rule");
        assert_eq!(collected.uncounted.len(), files.len());
        assert!(collected.uncounted.iter().all(|asset| asset.bytes == 16));
        assert!(collected.omissions.is_empty(), "{:?}", collected.omissions);
    }
}
