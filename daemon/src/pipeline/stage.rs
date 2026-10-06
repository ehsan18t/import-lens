//! The stages a result can carry, and the one question a durable store asks of one: is this
//! outcome a property of the package's bytes?
//!
//! [`crate::engine::stage`] owns the stages a build fails at. This module owns the ones the
//! pipeline fails at around the build (a manifest it cannot read, an entry it cannot stat, a
//! minifier that gave up), and the classification over both vocabularies, because a cache cares
//! only whether re-running would learn the same thing.
//!
//! ## The gate is an allowlist
//!
//! A stage may enter a durable store only if it is named here as a property of the bytes.
//! Anything else (a transient stage, an IO condition such as `entry_metadata`, and every stage
//! nobody has classified yet) is refused, and the store logs the refusal so a misclassification
//! surfaces as a warning rather than as a wrong number. Do not invert it into a denylist: an IO
//! failure missing from a denylist is cached and outlives the condition that caused it.
//!
//! ADR-0006, invariant 3.

use crate::engine::{diagnostic_stage, stage as engine_stage};

/// The package name is unsafe (traversal, separators). A property of the specifier.
pub const PACKAGE_VALIDATION: &str = "package_validation";
/// No `node_modules/<package>/package.json` at all, and the specifier is not first-party source
/// either. A missing dependency: its bytes belong in the file's total and are not in it.
pub const PACKAGE_RESOLUTION: &str = "package_resolution";
/// The specifier is a tsconfig path alias (`@app/components`, `~lib/foo`, a bare
/// `components/Button` under a `baseUrl`) that resolves to first-party source
/// (`crate::pipeline::resolver::FirstPartySourceProbe`).
///
/// Not a failure: Import Lens measures third-party imports (ADR-0004), so first-party code adds
/// nothing to a total, like a relative import, and the total stays complete. Only ever an
/// aggregate diagnostic; no `ImportResult` carries it.
pub const PATH_ALIAS: &str = "path_alias";
/// The manifest exists but cannot be parsed, or has no string `version`.
pub const PACKAGE_MANIFEST: &str = "package_manifest";
/// The manifest is fine but no entry point can be resolved from it.
pub const ENTRY_RESOLUTION: &str = "entry_resolution";
/// `fs::metadata` on the resolved entry failed.
///
/// Not durable and not deterministic: an IO condition (a lock, a permission blip, a drive that
/// went away) the next attempt may not hit. Transient although it is not an engine stage.
pub const ENTRY_METADATA: &str = "entry_metadata";
/// The entry file is larger than the module source limit. A property of its bytes.
pub const OVERSIZED_ENTRY: &str = "oversized_entry";
/// The minifier could not process the linked chunk. A property of the linked bytes.
pub const MINIFY: &str = "minify";
/// A compressor failed.
///
/// Not durable: `flate2` / `brotli` / `zstd` fail on valid input only through allocation failure
/// or IO, conditions of the machine rather than of the package.
pub const COMPRESSION: &str = "compression";
/// The request itself was rejected before any analysis ran.
///
/// Not durable: it describes the message, not the package's bytes.
pub const PROTOCOL: &str = "protocol";
/// The aggregate could not sum an import. Only ever an aggregate diagnostic, never an
/// `ImportResult`'s stage; the aggregate has its own gate
/// ([`crate::pipeline::file_size::FileSizeComputation::is_cacheable`]).
pub const FILE_SIZE_FALLBACK: &str = "file_size_fallback";
/// A declarations-only package: a measurement of zero runtime bytes, not a failure. Carried on a
/// Measured result, so it must be durable or every `@types`-shaped package is re-analyzed forever.
pub const TYPES_ONLY: &str = "types_only";
/// A native-binary-only package: no importable JS entry, only a `bin` plus a platform-specific
/// native binary declared as `optionalDependencies` (the `@scope/cli-win32-x64` pattern). A
/// measurement of zero runtime JS bytes, like `types_only`, so it must be durable or every such
/// tool (Biome, the TypeScript 7 native rewrite, esbuild's CLI) is re-analyzed forever.
pub const NATIVE_BINARY_ONLY: &str = "native_binary_only";
/// A package whose JS entry resolves (a thin shim, or a real API) but which is backed by a
/// platform-specific native binary shipped as `optionalDependencies`. An informational flag on a
/// successful measurement (the JS size is real; the tool's work lives in the native binary), so,
/// like `external` / `uncounted_assets`, refusing to cache it would refuse a healthy package.
pub const NATIVE_BINARY: &str = "native_binary";

/// Every stage this module declares, so the property test below quantifies over the whole
/// vocabulary.
pub const ALL: &[&str] = &[
    PACKAGE_VALIDATION,
    PACKAGE_RESOLUTION,
    PATH_ALIAS,
    PACKAGE_MANIFEST,
    ENTRY_RESOLUTION,
    ENTRY_METADATA,
    OVERSIZED_ENTRY,
    MINIFY,
    COMPRESSION,
    PROTOCOL,
    FILE_SIZE_FALLBACK,
    TYPES_ONLY,
    NATIVE_BINARY_ONLY,
    NATIVE_BINARY,
];

/// Every analysis stage that describes this request's machine/filesystem state rather than a fact
/// about the package bytes: a build cancelled at the deadline, unwound, cut off from its runtime,
/// or unable to observe an asset input, and transient pipeline work around the engine. This is the
/// only such list; the extension and CLI mirror it under a drift check
/// (`scripts/test/engine-stage-coordination.test.mjs`). It is not the cache gate by itself: that is
/// [`may_enter_a_durable_store`], which refuses any stage it has not classified.
pub const TRANSIENT_ANALYSIS_STAGES: &[&str] = &[
    engine_stage::PANIC,
    engine_stage::TIMEOUT,
    engine_stage::ENGINE_GONE,
    engine_stage::ASSET_IO,
    ENTRY_METADATA,
    COMPRESSION,
];

pub fn is_transient(stage: &str) -> bool {
    TRANSIENT_ANALYSIS_STAGES.contains(&stage)
}

/// The stages an outcome may carry into a store that outlives the request: those that are a
/// property of the package's bytes (the cache, keyed by those bytes' fingerprints, expires them
/// exactly when the answer would change) and the informational stages that ride a successful
/// measurement.
pub const DURABLE_RESULT_STAGES: &[&str] = &[
    // Engine failures that are a property of the code being built. The request-local engine
    // stages (`panic`, `timeout`, `engine_gone`, `asset_io`) are deliberately absent.
    engine_stage::RESOLVE,
    engine_stage::PARSE,
    engine_stage::LINK,
    engine_stage::GENERATE,
    engine_stage::OUTPUT_SHAPE,
    engine_stage::MODULE_GRAPH_LIMIT,
    engine_stage::MISSING_EXPORT,
    engine_stage::AMBIGUOUS_EXPORT,
    // Stages that ride a SUCCESSFUL measurement and disclose its limits. Refusing one would
    // refuse to cache a healthy package. A transient read error carries the separate `asset_io`
    // stage, so it cannot borrow this durable classification merely by also landing in
    // `uncounted_assets`.
    diagnostic_stage::EXTERNAL,
    diagnostic_stage::UNCOUNTED_ASSETS,
    diagnostic_stage::IMPRECISE_ASSETS,
    // Pipeline failures that are properties of package bytes.
    PACKAGE_VALIDATION,
    PACKAGE_RESOLUTION,
    PACKAGE_MANIFEST,
    ENTRY_RESOLUTION,
    OVERSIZED_ENTRY,
    MINIFY,
    // Real measurements of zero and the informational native-binary flag.
    TYPES_ONLY,
    NATIVE_BINARY_ONLY,
    NATIVE_BINARY,
];

/// Whether an outcome carrying `stage` may be written to a store that outlives the request.
///
/// False for any stage not in [`DURABLE_RESULT_STAGES`], including one never classified: a new
/// stage costs a rebuild until someone classifies it, never a durable wrong answer.
pub fn may_enter_a_durable_store(stage: &str) -> bool {
    DURABLE_RESULT_STAGES.contains(&stage)
}

/// Deterministic results that remain useful in caches and history but are not precise enough for a
/// pass/fail budget verdict. The extension and standalone CLI mirror this list under a drift test.
pub const NON_BUDGETABLE_RESULT_STAGES: &[&str] = &[diagnostic_stage::IMPRECISE_ASSETS];

pub fn prevents_budget_verdict(stage: &str) -> bool {
    NON_BUDGETABLE_RESULT_STAGES.contains(&stage)
}

/// Whether a diagnostic of `stage` on a **measured** result, or on a successful build, says shipped
/// bytes are absent from the number, which makes it a floor:
///
/// * `uncounted_assets`: supported assets the import ships that the size omits;
/// * `resolve`: a specifier the build could not resolve and kept as an import boundary, so anything
///   it would have pulled in is absent;
/// * `missing_export`: a binding stubbed between two dependencies, so whatever the real binding
///   would retain is absent.
///
/// `external` is not one: a builtin boundary is one the import never ships.
pub fn marks_a_floor(stage: &str) -> bool {
    [
        diagnostic_stage::UNCOUNTED_ASSETS,
        engine_stage::RESOLVE,
        engine_stage::MISSING_EXPORT,
    ]
    .contains(&stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Property over the entire stage vocabulary (the engine's and the pipeline's): a stage added
    /// to either list and left out of `may_enter_a_durable_store` lands in `refused` here, so its
    /// classification is always a visible decision.
    #[test]
    fn every_declared_stage_is_classified_and_no_transient_stage_is_durable() {
        let mut durable = Vec::new();
        let mut refused = Vec::new();
        for stage in engine_stage::ALL
            .iter()
            .chain(diagnostic_stage::ALL.iter())
            .chain(ALL.iter())
            .copied()
        {
            if may_enter_a_durable_store(stage) {
                durable.push(stage);
            } else {
                refused.push(stage);
            }
        }

        assert_eq!(
            refused,
            vec![
                engine_stage::PANIC,
                engine_stage::TIMEOUT,
                engine_stage::ENGINE_GONE,
                engine_stage::ASSET_IO,
                PATH_ALIAS,
                ENTRY_METADATA,
                COMPRESSION,
                PROTOCOL,
                FILE_SIZE_FALLBACK,
            ],
            "a stage that reaches a durable store must be classified on purpose. Refused today: \
             the transient analysis stages and the three stages that never ride an ImportResult \
             (`path_alias`, `protocol`, `file_size_fallback`)"
        );

        for stage in TRANSIENT_ANALYSIS_STAGES {
            assert!(
                !may_enter_a_durable_store(stage),
                "`{stage}` is transient; no durable store may take it (ADR-0006, invariant 3)"
            );
        }

        assert!(!durable.is_empty());
    }

    /// The default is refusal: forgetting to classify a new stage costs one rebuild, never a
    /// wrong answer that outlives the request.
    #[test]
    fn an_unclassified_stage_is_refused() {
        assert!(!may_enter_a_durable_store("a_stage_nobody_has_classified"));
        assert!(!may_enter_a_durable_store(""));
    }

    #[test]
    fn an_imprecise_asset_result_is_durable_but_not_budgetable() {
        assert!(may_enter_a_durable_store(
            diagnostic_stage::IMPRECISE_ASSETS
        ));
        assert!(prevents_budget_verdict(diagnostic_stage::IMPRECISE_ASSETS));
    }
}
