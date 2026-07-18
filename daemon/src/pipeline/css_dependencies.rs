use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use lightningcss::dependencies::{Dependency, ImportDependency, UrlDependency};

use crate::engine::{AssetKind, CollectedAsset, classify_asset};

/// Resolve the supported local files referenced by `url()` in a bundled stylesheet.
///
/// Lightning CSS reports the source file for every reference, including rules originating in an
/// `@import` child. That source—not the synthetic union entry—is the base a browser/bundler uses.
/// Unsupported resource kinds remain CSS text and are outside the current CSS/wasm/font scope.
pub(super) struct CssDependencyAssets {
    pub assets: Vec<CollectedAsset>,
    pub unresolved: Vec<String>,
}

pub(super) fn collect_referenced_assets(
    dependencies: impl IntoIterator<Item = Dependency>,
) -> CssDependencyAssets {
    let mut assets = BTreeMap::new();
    let mut unresolved = BTreeSet::new();

    for dependency in dependencies {
        match dependency {
            Dependency::Url(dependency) => match collect_supported_asset(dependency) {
                Some(Ok(asset)) => {
                    assets.entry(asset.path.clone()).or_insert(asset);
                }
                Some(Err(message)) => {
                    unresolved.insert(message);
                }
                None => {}
            },
            Dependency::Import(dependency) => {
                if let Some(message) = unresolved_import(dependency) {
                    unresolved.insert(message);
                }
            }
        }
    }

    CssDependencyAssets {
        assets: assets.into_values().collect(),
        unresolved: unresolved.into_iter().collect(),
    }
}

/// Local `@import`s have already been inlined by the bundler. Any dependency that survives this
/// print pass is therefore an external stylesheet whose fetched bytes—and any assets it imports—are
/// outside the measurement. A data stylesheet is embedded in the counted CSS text itself.
fn unresolved_import(dependency: ImportDependency) -> Option<String> {
    if dependency
        .url
        .trim()
        .to_ascii_lowercase()
        .starts_with("data:")
    {
        return None;
    }

    Some(format!(
        "external CSS import `{}` in {} cannot be measured",
        dependency.url,
        Path::new(&dependency.loc.file_path).display()
    ))
}

fn collect_supported_asset(dependency: UrlDependency) -> Option<Result<CollectedAsset, String>> {
    let resource_path = resource_path(&dependency.url)?;
    let kind = classify_asset(Path::new(&resource_path))?;
    if !matches!(kind, AssetKind::Wasm | AssetKind::Font) {
        return None;
    }

    let source_file = Path::new(&dependency.loc.file_path);
    let resource = Path::new(&resource_path);
    if has_url_scheme(dependency.url.trim()) || resource.has_root() || !source_file.is_absolute() {
        return Some(Err(format!(
            "CSS resource `{}` in {} is not a package-relative file and cannot be measured",
            dependency.url,
            source_file.display()
        )));
    }

    let path = source_file.parent()?.join(resource);
    let path = fs::canonicalize(&path).unwrap_or(path);
    let raw_bytes = fs::metadata(&path).map_or(0, |metadata| metadata.len());
    Some(Ok(CollectedAsset {
        path,
        kind,
        raw_bytes,
    }))
}

/// Extract the filesystem-looking portion of a CSS resource URL. Query strings and fragments name
/// the same emitted file. Data URLs are already bytes inside the stylesheet and fragment-only URLs
/// reference the current document, so neither creates a separate artifact.
fn resource_path(specifier: &str) -> Option<PathBuf> {
    let specifier = specifier.trim();
    if specifier.is_empty()
        || specifier.starts_with('#')
        || specifier.to_ascii_lowercase().starts_with("data:")
    {
        return None;
    }

    let path_end = specifier.find(['?', '#']).unwrap_or(specifier.len());
    let path = decode_percent_encoded(&specifier[..path_end])?;
    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
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
