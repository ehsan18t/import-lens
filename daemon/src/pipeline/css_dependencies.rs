use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use lightningcss::dependencies::{Dependency, ImportDependency, UrlDependency};

use crate::engine::{AssetKind, CollectedAsset, UncountedAsset, classify_asset};

/// Resolve the local files referenced by `url()` in a bundled stylesheet.
///
/// Lightning CSS reports the source file for every reference, including rules originating in an
/// `@import` child. That source—not the synthetic union entry—is the base a browser/bundler uses.
///
/// Every reference lands in exactly one of these, and the split is the point: a resource this
/// package ships is either counted, or disclosed with its bytes, or named as an omission — never
/// dropped. Only a resource fetched from elsewhere at runtime leaves no trace, because it is not
/// this import's cost to begin with (ADR-0004).
pub(super) struct CssDependencyAssets {
    pub assets: Vec<CollectedAsset>,
    pub failures: Vec<CssDependencyFailure>,
    /// Resolvable local files outside the counted CSS/wasm/font taxonomy — an image, an SVG. Their
    /// bytes ship, so they are disclosed at their real size and left out of the total.
    pub uncounted: Vec<UncountedAsset>,
    /// Local resources that ship but could not be located, read, or inspected, so not even their
    /// size is known. These make the result a floor: bytes are missing and the magnitude is not.
    pub omissions: Vec<String>,
    /// Resources fetched over the network at runtime. Real weight the user pays, but not bytes this
    /// package ships, so the measured size stays EXACT and keeps its budget verdict. Disclosed on
    /// the `external` stage, which is durable and budgetable, rather than on a precision stage that
    /// would refuse to judge an exact number.
    pub external: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CssDependencyFailure {
    pub path: PathBuf,
    pub raw_bytes: u64,
    pub message: String,
    /// WHY the read failed. A target that simply is not there is a deterministic fact about the
    /// package and must take the absent-state sentinel, so that supplying the file invalidates the
    /// result; a permission error or a lock is a fact about this machine right now and stays
    /// request-local. Carrying the kind here is what lets the caller tell them apart — hardcoding
    /// "not missing" made every stale `url()` target permanently non-durable and told the user the
    /// result "reflects a changing or unavailable filesystem" about a stable package.
    pub kind: std::io::ErrorKind,
}

enum SupportedAsset {
    Collected(CollectedAsset),
    Unreadable(CssDependencyFailure),
    Uncounted(UncountedAsset),
    Omitted(String),
    /// Fetched over the network at runtime, so not bytes this package ships. The measured size
    /// stays exact and keeps its budget verdict; treating one of these as unmeasurable is what
    /// silently disabled budgeting for every package that `@import`s a web font.
    External(String),
}

pub(super) fn collect_referenced_assets(
    dependencies: impl IntoIterator<Item = Dependency>,
    read_asset: &impl Fn(&Path, AssetKind) -> std::io::Result<CollectedAsset>,
    should_continue: &impl Fn() -> bool,
) -> CssDependencyAssets {
    let mut assets = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let mut uncounted = BTreeMap::new();
    let mut omissions = BTreeSet::new();
    let mut external = BTreeSet::new();

    let mut dependencies = dependencies.into_iter();
    while should_continue() {
        let Some(dependency) = dependencies.next() else {
            break;
        };
        match dependency {
            Dependency::Url(dependency) => match collect_supported_asset(dependency, read_asset) {
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
                // Nothing at all: a `data:` payload already inside the counted CSS text, or a bare
                // fragment pointing at the current document.
                None => {}
            },
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

    // Stopping early is abandoned work, not an absence of references. Anything still queued when the
    // deadline or the budget cut the walk short ships without being examined, so it is named as an
    // omission: silently returning the references we happened to reach makes a short total look
    // complete, which is the one outcome this module exists to prevent.
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

/// A remote stylesheet is real weight the user's page pays, but it is fetched rather than shipped by
/// this package, so it does not change what the measured bytes are — only what they leave out.
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

fn collect_supported_asset(
    dependency: UrlDependency,
    read_asset: &impl Fn(&Path, AssetKind) -> std::io::Result<CollectedAsset>,
) -> Option<SupportedAsset> {
    let specifier = dependency.url.trim();
    let source_file = Path::new(&dependency.loc.file_path);

    // No separate artifact at all: a `data:` payload is already bytes inside the counted CSS text and
    // a bare fragment points at the current document. These are the ONLY two reasons a reference may
    // leave no trace, which is why they are decided here rather than falling out of a failed parse.
    if specifier.is_empty()
        || specifier.starts_with('#')
        || specifier.to_ascii_lowercase().starts_with("data:")
    {
        return None;
    }

    // Externality is decided BEFORE the kind, and the order is load-bearing. Classifying first sent
    // every unsupported extension out through one `None` arm, so a remote image and a shipped local
    // image were indistinguishable — and both vanished.
    if is_remote_reference(specifier) {
        return Some(SupportedAsset::External(format!(
            "CSS resource `{}` in {} is fetched at runtime and is not in this size",
            dependency.url,
            source_file.display()
        )));
    }

    // A reference we cannot turn into a path still names bytes that ship. Percent-escapes that do not
    // decode to UTF-8 (a CP-1252 export naming `Ubuntu-R%E9gular.woff2`) used to leave through the
    // same silent `None` as a `data:` payload, so a whole font face vanished from the total while the
    // result stayed Measured at High confidence — cached, budgeted, and never invalidated by
    // supplying the file. It is an omission: the bytes are missing and their size is unknown.
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
    let path = fs::canonicalize(&path).unwrap_or(path);
    let metadata = fs::metadata(&path);
    let raw_bytes = metadata.as_ref().map_or(0, |metadata| metadata.len());

    // A resolvable path that cannot be stat'd is `Unreadable`, NOT `Omitted`, and the distinction is
    // load-bearing for freshness: `Unreadable` carries the path into `failed_paths`, which is what
    // makes the result never-fresh so that ADDING the missing file invalidates it. `Omitted` has no
    // path to fingerprint and is reserved for references that never resolved to one.
    let unreadable = |message: String, kind: std::io::ErrorKind| {
        Some(SupportedAsset::Unreadable(CssDependencyFailure {
            message,
            path: path.clone(),
            raw_bytes,
            kind,
        }))
    };
    if let Err(error) = metadata.as_ref() {
        return unreadable(
            format!(
                "CSS resource {} could not be read, so its shipped bytes are not in this size",
                path.display()
            ),
            error.kind(),
        );
    }

    // Outside the counted taxonomy — an image, an SVG, anything the processors do not handle. The
    // bytes ship regardless, so they are disclosed at full size rather than dropped. This arm used
    // to be a bare `None`, which took them out of the headline in silence and left the result at
    // High confidence claiming to be the import's full cost.
    let counted_kind =
        classify_asset(&path).filter(|kind| matches!(kind, AssetKind::Wasm | AssetKind::Font));
    let Some(kind) = counted_kind else {
        return Some(SupportedAsset::Uncounted(UncountedAsset {
            path,
            bytes: raw_bytes,
        }));
    };

    match read_asset(&path, kind) {
        Ok(asset) => Some(SupportedAsset::Collected(asset)),
        Err(error) => {
            let error_kind = error.kind();
            unreadable(
                format!("failed to read CSS resource {}: {error}", path.display()),
                error_kind,
            )
        }
    }
}

/// Extract the filesystem-looking portion of a CSS resource URL. Query strings and fragments name
/// the same emitted file. Returns `None` when the reference cannot be turned into a file name at
/// all; the caller discloses that as an omission rather than dropping it. The `data:` and
/// fragment-only cases are decided by the caller, because those are the only two references that
/// legitimately name no artifact.
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
/// Protocol-relative (`//cdn/x.woff2`) counts, and leaving it out is what made a CDN font look like
/// an unlocatable LOCAL file: the number stayed correct but was labelled a floor and lost its budget
/// verdict. The `@import` half already knew about this form; the `url()` half did not.
fn is_remote_reference(value: &str) -> bool {
    value.starts_with("//") || has_url_scheme(value)
}

fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    characters
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
