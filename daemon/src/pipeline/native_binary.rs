//! Native-binary-backed packages: a growing class of dev tools ship a thin JS shim (or nothing at
//! all) plus a platform-specific native binary distributed as `optionalDependencies` — the
//! `@scope/cli-win32-x64` / `pkg-linux-x64` pattern (Biome, the TypeScript 7 native rewrite,
//! esbuild). The native binary is spawned at runtime by the package's `bin`; it never enters the
//! import graph, so counting its bytes would be wrong (you do not bundle a dev CLI into your app),
//! and measuring the JS shim alone reports a confident, misleadingly-tiny number.
//!
//! So the rule is **detect and label, do not count** (known issue B3):
//! - **No importable JS entry** → a `native_binary_only` badge (a Measured zero, the same shape as
//!   `types_only`), instead of a bare "unavailable" that reads like a build failure.
//! - **A JS entry that resolves** → its measured JS size stands, with a `native_binary` flag beside
//!   it, so a stub like TypeScript 7's version shim reads as "the JS shim; the tool is native"
//!   rather than the whole cost.
//!
//! Detection is by the platform-suffixed `optionalDependencies` convention, at the resolver
//! boundary — never by package name — so it covers every such package by construction.

use crate::ipc::protocol::{
    ConfidenceLevel, ImportDiagnostic, ImportRequest, ImportResult, MeasuredSizes,
};
use crate::pipeline::resolver::find_package_root;
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Operating-system tokens seen in the platform suffix of a native-binary optional dependency
/// (`process.platform` values plus the Rust/other spellings packages actually publish).
const OS_TOKENS: &[&str] = &[
    "win32",
    "windows",
    "darwin",
    "linux",
    "freebsd",
    "openbsd",
    "netbsd",
    "dragonfly",
    "sunos",
    "solaris",
    "aix",
    "android",
    "openharmony",
    "haiku",
    "cygwin",
    "wasi",
];

/// Architecture tokens seen in the platform suffix.
const ARCH_TOKENS: &[&str] = &[
    "x64",
    "x86_64",
    "amd64",
    "arm64",
    "aarch64",
    "ia32",
    "x86",
    "arm",
    "armv6",
    "armv7",
    "armv7l",
    "ppc",
    "ppc64",
    "ppc64le",
    "s390",
    "s390x",
    "riscv64",
    "loong64",
    "loongarch64",
    "mips",
    "mipsel",
    "mips64",
    "mips64el",
    "wasm32",
    "universal",
];

/// Optional libc / ABI token that may trail the arch (`linux-x64-musl`, `win32-x64-msvc`).
const LIBC_TOKENS: &[&str] = &[
    "musl",
    "gnu",
    "gnueabi",
    "gnueabihf",
    "eabi",
    "eabihf",
    "glibc",
    "msvc",
];

/// Whether a single dependency name looks like a platform-specific native package: after stripping
/// any `@scope/`, its trailing segments are `<os>-<arch>` (optionally followed by one libc/ABI
/// token). Anchoring to the END is what keeps this precise — a bare `arm` or `x64` in the middle of
/// a name does not match; it must be an OS token immediately followed by an arch token at the tail.
fn dependency_is_platform_native(dependency: &str) -> bool {
    let tail = dependency.rsplit('/').next().unwrap_or(dependency);
    let tokens: Vec<&str> = tail.split('-').collect();
    if tokens.len() < 2 {
        return false;
    }

    for index in 0..tokens.len() - 1 {
        let is_os = OS_TOKENS.contains(&tokens[index]);
        let is_arch = ARCH_TOKENS.contains(&tokens[index + 1]);
        if !is_os || !is_arch {
            continue;
        }
        let trailing = tokens.len() - (index + 2);
        if trailing == 0 || (trailing == 1 && LIBC_TOKENS.contains(&tokens[index + 2])) {
            return true;
        }
    }

    false
}

/// Whether a package manifest declares a platform-specific native binary as an optional dependency
/// — the signal that the package is native-binary-backed.
pub fn manifest_is_native_binary_backed(package_json: &Value) -> bool {
    let Some(optional) = package_json
        .get("optionalDependencies")
        .and_then(Value::as_object)
    else {
        return false;
    };
    optional
        .keys()
        .any(|dependency| dependency_is_platform_native(dependency))
}

fn read_manifest_json(package_root: &Path) -> Option<Value> {
    let text = fs::read_to_string(package_root.join("package.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a manifest declares NO importable JavaScript entry point (`main` / `module` / `browser`
/// / `exports`). This is what distinguishes a genuine native-binary-only package (Biome declares
/// only `bin`) from one that DECLARES a JS entry which merely failed to resolve — a broken or
/// partial install. Only the former is a real zero; the latter must stay "unavailable" rather than
/// be flattened to a confident zero it does not have.
fn manifest_declares_no_js_entry(package_json: &Value) -> bool {
    !["main", "module", "browser", "exports"]
        .iter()
        .any(|field| package_json.get(*field).is_some())
}

/// A native-binary-**only** package (no importable JS entry), answered MEASURED at zero — the same
/// shape as [`crate::pipeline::types_only`]. Only ever reached on the `entry_resolution` failure
/// branch, so by construction there is no JS to size; the zero is an *answer* (nothing ships into
/// the import graph), not the absence of one, and the `native_binary_only` diagnostic keeps it
/// unambiguous.
pub fn native_binary_only_package_result(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Option<ImportResult> {
    let package_root = find_package_root(active_document_path, &request.package_name).ok()?;
    let manifest = read_manifest_json(&package_root)?;
    // Both must hold: the package is native-binary-backed AND it declares no importable JS entry.
    // Requiring the second guards against a package that has real JavaScript whose entry merely
    // failed to resolve (a broken install) being flattened to a confident zero — that stays
    // "unavailable", which is the honest answer when we could not measure it.
    if !manifest_is_native_binary_backed(&manifest) || !manifest_declares_no_js_entry(&manifest) {
        return None;
    }

    let mut result = ImportResult::measured(request.specifier.clone(), MeasuredSizes::ZERO);
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons = vec![
        "Package ships a platform-specific native binary and no importable JavaScript entry."
            .to_owned(),
    ];
    result.diagnostics = vec![ImportDiagnostic {
        stage: crate::pipeline::stage::NATIVE_BINARY_ONLY.to_owned(),
        message: "package ships only a native binary; nothing is imported into the bundle"
            .to_owned(),
        details: vec![
            format!("specifier: {}", request.specifier),
            format!("package: {}", request.package_name),
            format!("package_root: {}", package_root.display()),
        ],
    }];
    result.module_breakdown = Some(Vec::new());

    Some(result)
}

/// Flag a **measured** result whose package is native-binary-backed, so a resolved-but-thin JS
/// entry (TypeScript 7's version stub, esbuild's wrapper) is read as "the JS shim; the tool is a
/// native binary" rather than the whole cost. Never touches an Unmeasured result — the flag rides a
/// real measurement, not a failure — and never a package that is not native-backed.
pub fn annotate_native_binary(result: &mut ImportResult, package_json: &Value) {
    if result.sizes().is_none() || !manifest_is_native_binary_backed(package_json) {
        return;
    }
    result.diagnostics.push(ImportDiagnostic {
        stage: crate::pipeline::stage::NATIVE_BINARY.to_owned(),
        message:
            "package is backed by a platform-specific native binary; the measured size is its \
                  JavaScript entry only"
                .to_owned(),
        details: vec![format!("specifier: {}", result.specifier)],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_with_optional(keys: &[&str]) -> Value {
        let mut optional = serde_json::Map::new();
        for key in keys {
            optional.insert((*key).to_owned(), json!("1.0.0"));
        }
        json!({ "version": "1.0.0", "optionalDependencies": Value::Object(optional) })
    }

    #[test]
    fn real_native_binary_manifests_are_detected() {
        // The exact optionalDependencies shapes shipped by these tools today.
        let cases: &[(&str, &[&str])] = &[
            (
                "@biomejs/biome",
                &["@biomejs/cli-win32-x64", "@biomejs/cli-linux-x64-musl"],
            ),
            (
                "typescript",
                &[
                    "@typescript/typescript-win32-x64",
                    "@typescript/typescript-aix-ppc64",
                ],
            ),
            (
                "esbuild",
                &[
                    "@esbuild/win32-x64",
                    "@esbuild/android-arm",
                    "@esbuild/linux-mips64el",
                ],
            ),
            ("sharp", &["@img/sharp-win32-x64", "@img/sharp-linux-x64"]),
            (
                "next",
                &["@next/swc-win32-x64-msvc", "@next/swc-linux-x64-gnu"],
            ),
            ("unscoped", &["pkg-linux-x64", "pkg-openharmony-arm64"]),
        ];
        for (name, keys) in cases {
            assert!(
                manifest_is_native_binary_backed(&manifest_with_optional(keys)),
                "{name} should be detected as native-binary-backed: {keys:?}",
            );
        }
    }

    #[test]
    fn ordinary_manifests_are_not_detected() {
        // No optionalDependencies at all.
        assert!(!manifest_is_native_binary_backed(
            &json!({ "version": "1.0.0" })
        ));
        // Optional deps that are not platform-suffixed native packages.
        assert!(!manifest_is_native_binary_backed(&manifest_with_optional(
            &["left-pad", "@babel/core", "fsevents",]
        )));
        // An arch or OS token that is NOT anchored as an `<os>-<arch>` tail.
        assert!(!manifest_is_native_binary_backed(&manifest_with_optional(
            &[
                "@scope/arm-helpers",
                "@scope/x64-utils",
                "@scope/linux-tools",
                "@scope/react-native",
            ]
        )));
    }

    #[test]
    fn a_declared_js_entry_disqualifies_the_native_binary_only_zero() {
        // Biome's shape: only `bin` (plus native optional deps), no JS entry field -> qualifies.
        assert!(manifest_declares_no_js_entry(
            &json!({ "version": "1.0.0", "bin": { "biome": "bin/biome" } })
        ));
        // A DECLARED entry, even one that fails to resolve (a broken install), disqualifies the
        // zero: the package stays "unavailable" rather than being flattened to a confident zero.
        for field in ["main", "module", "browser", "exports"] {
            let mut manifest = serde_json::Map::new();
            manifest.insert("version".to_owned(), json!("1.0.0"));
            manifest.insert(field.to_owned(), json!("missing.js"));
            assert!(
                !manifest_declares_no_js_entry(&Value::Object(manifest)),
                "declaring `{field}` must disqualify the native-binary-only zero",
            );
        }
    }

    #[test]
    fn the_flag_never_lands_on_an_unmeasured_result() {
        let mut unmeasured = ImportResult::unmeasured(
            "pkg",
            crate::pipeline::stage::ENTRY_RESOLUTION,
            "no",
            vec![],
        );
        annotate_native_binary(
            &mut unmeasured,
            &manifest_with_optional(&["@scope/cli-win32-x64"]),
        );
        assert!(
            !unmeasured
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::NATIVE_BINARY),
            "the native-binary flag must never ride an Unmeasured result: {unmeasured:?}",
        );
    }

    #[test]
    fn the_flag_lands_on_a_measured_native_backed_result_and_not_otherwise() {
        let native = manifest_with_optional(&["@typescript/typescript-win32-x64"]);
        let mut flagged = ImportResult::measured("typescript", MeasuredSizes::ZERO);
        annotate_native_binary(&mut flagged, &native);
        assert!(
            flagged
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::NATIVE_BINARY),
            "a measured native-backed package must be flagged: {flagged:?}",
        );

        let mut plain = ImportResult::measured("left-pad", MeasuredSizes::ZERO);
        annotate_native_binary(&mut plain, &json!({ "version": "1.0.0" }));
        assert!(
            plain.diagnostics.is_empty(),
            "a package with no native optional dependency must not be flagged: {plain:?}",
        );
    }
}
