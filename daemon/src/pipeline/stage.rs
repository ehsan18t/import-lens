//! The stages a result can carry, and the ONE question a durable store asks of one:
//! **is this outcome a property of the package's bytes?**
//!
//! [`crate::engine::stage`] owns the stages a *build* fails at. This module owns the ones the
//! pipeline fails at around the build — a manifest it cannot read, an entry it cannot stat, a
//! minifier that gave up — and it owns the classification over **both** vocabularies, because a
//! cache does not care which module produced the failure. It cares whether re-running would learn
//! the same thing.
//!
//! ## The gate is an ALLOWLIST, and that is the point
//!
//! It used to be a denylist: cache everything except `panic` / `timeout` / `engine_gone`. That is
//! safe only if every *other* stage the daemon can produce is a property of the bytes — and
//! `entry_metadata` is not. It is `fs::metadata` failing: a file locked by an installer, a
//! permission blip, a network drive that blinked. Under a denylist, that momentary IO error was
//! written to the L1 and L2 caches and expired only when the package's `package.json` changed —
//! the exact disease ADR-0006 exists to end (a transient condition producing a durable wrong
//! answer), reintroduced by the very commit that widened the gate to cache deterministic failures.
//!
//! So the question is inverted. A stage may enter a durable store only if it is **named here** as
//! a property of the bytes. Anything else — a transient stage, an IO condition, and every stage
//! nobody has classified yet, including one added tomorrow — is refused, and the store logs the
//! refusal so a misclassification surfaces as a warning rather than as a wrong number.
//!
//! ADR-0006, invariant 3.

use crate::engine::{diagnostic_stage, stage as engine_stage};

/// The package name is unsafe (traversal, separators). A property of the specifier.
pub const PACKAGE_VALIDATION: &str = "package_validation";
/// No `node_modules/<package>/package.json` at all, and the specifier is not first-party source
/// either. A missing dependency: its bytes belong in the file's total and are not in it.
pub const PACKAGE_RESOLUTION: &str = "package_resolution";
/// The specifier is not a package at all — a tsconfig **path alias** (`@app/components`, `~lib/foo`,
/// a bare `components/Button` under a `baseUrl`) that RESOLVES to first-party source
/// (`crate::pipeline::resolver::FirstPartySourceProbe`).
///
/// **Not a failure.** Import Lens measures third-party imports (ADR-0004), so first-party code
/// contributes nothing to a total it reports, exactly like a relative import — the total stays
/// complete. Only ever an *aggregate* diagnostic; no `ImportResult` carries it.
pub const PATH_ALIAS: &str = "path_alias";
/// The manifest exists but cannot be parsed, or has no string `version`.
pub const PACKAGE_MANIFEST: &str = "package_manifest";
/// The manifest is fine but no entry point can be resolved from it.
pub const ENTRY_RESOLUTION: &str = "entry_resolution";
/// `fs::metadata` on the resolved entry failed.
///
/// **Not durable, and not deterministic.** This is an IO condition — a lock, a permission blip, a
/// drive that went away — and the next attempt may well succeed. It is the one stage the old
/// denylist got wrong, because it is transient in fact while being absent from
/// [`engine_stage::is_transient`], whose three names describe the *engine*.
pub const ENTRY_METADATA: &str = "entry_metadata";
/// The entry file is larger than the module source limit. A property of its bytes.
pub const OVERSIZED_ENTRY: &str = "oversized_entry";
/// The minifier could not process the linked chunk. A property of the linked bytes.
pub const MINIFY: &str = "minify";
/// A compressor failed.
///
/// **Not durable.** In practice the only ways `flate2` / `brotli` / `zstd` fail on a valid string
/// are allocation failure and IO — conditions of the machine, not of the package.
pub const COMPRESSION: &str = "compression";
/// The request itself was rejected before any analysis ran.
///
/// **Not durable.** It describes the message, not the package; nothing about the package's bytes
/// was ever examined.
pub const PROTOCOL: &str = "protocol";
/// The aggregate could not sum an import. Only ever an *aggregate* diagnostic — never an
/// `ImportResult`'s stage — and the aggregate has its own gate
/// ([`crate::pipeline::file_size::FileSizeComputation::is_cacheable`]).
pub const FILE_SIZE_FALLBACK: &str = "file_size_fallback";
/// A declarations-only package: a **measurement** of zero runtime bytes, not a failure. Carried on
/// a Measured result, so it must be durable or every `@types`-shaped package would be re-analyzed
/// forever.
pub const TYPES_ONLY: &str = "types_only";

/// Every stage this module declares, so the property test below can quantify over the whole
/// vocabulary rather than over the subset someone remembered to list.
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
];

/// Whether an outcome carrying `stage` may be written to a store that outlives the request.
///
/// True only for a stage that is a **property of the package's bytes** — so the cache, which is
/// keyed by those bytes' fingerprints, expires it exactly when the answer would change — or for a
/// purely informational stage that rides along on a successful measurement (`external`,
/// `uncounted_assets`, `types_only`).
///
/// False for everything else, **including a stage this function has never heard of**. That default
/// is the whole design: a new stage is refused until someone classifies it, so the failure mode of
/// forgetting is a rebuild, not a durable wrong answer.
pub fn may_enter_a_durable_store(stage: &str) -> bool {
    matches!(
        stage,
        // Engine failures that are a property of the code being built. `is_transient`'s three —
        // `panic`, `timeout`, `engine_gone` — are deliberately absent.
        engine_stage::RESOLVE
            | engine_stage::PARSE
            | engine_stage::LINK
            | engine_stage::GENERATE
            | engine_stage::OUTPUT_SHAPE
            | engine_stage::MODULE_GRAPH_LIMIT
            | engine_stage::MISSING_EXPORT
            | engine_stage::AMBIGUOUS_EXPORT
            // Informational stages an engine build emits on the SUCCESS path. Refusing one of
            // these would refuse to cache a healthy package.
            | diagnostic_stage::EXTERNAL
            | diagnostic_stage::UNCOUNTED_ASSETS
            // Pipeline failures that are a property of the package's bytes.
            | PACKAGE_VALIDATION
            | PACKAGE_RESOLUTION
            | PACKAGE_MANIFEST
            | ENTRY_RESOLUTION
            | OVERSIZED_ENTRY
            | MINIFY
            // A real measurement of zero.
            | TYPES_ONLY
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Property** over the ENTIRE stage vocabulary — the engine's and the pipeline's — asserting
    /// each stage is classified deliberately. A stage added to either list and left out of
    /// `may_enter_a_durable_store` lands in `refused` here, and the reviewer must move it or accept
    /// that it is refused. Neither is silent.
    #[test]
    fn every_declared_stage_is_classified_and_no_transient_stage_is_durable() {
        let mut durable = Vec::new();
        let mut refused = Vec::new();
        for stage in engine_stage::ALL.iter().chain(ALL.iter()).copied() {
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
                PATH_ALIAS,
                ENTRY_METADATA,
                COMPRESSION,
                PROTOCOL,
                FILE_SIZE_FALLBACK,
            ],
            "a stage that reaches a durable store must be classified on purpose. Refused today: \
             the three transient engine stages, plus the machine conditions (`entry_metadata`, \
             `compression`) and the three stages that never ride an ImportResult (`path_alias`, \
             `protocol`, `file_size_fallback`) — refusing an aggregate-only stage costs nothing, \
             because no result carries one into a cache"
        );

        for stage in engine_stage::ALL
            .iter()
            .copied()
            .filter(|stage| engine_stage::is_transient(stage))
        {
            assert!(
                !may_enter_a_durable_store(stage),
                "`{stage}` is transient; no durable store may take it (ADR-0006, invariant 3)"
            );
        }

        assert!(!durable.is_empty());
    }

    /// The default is REFUSAL. This is what makes the allowlist safe: the cost of forgetting to
    /// classify a new stage is one rebuild, never a wrong answer that outlives the request.
    #[test]
    fn an_unclassified_stage_is_refused() {
        assert!(!may_enter_a_durable_store("a_stage_nobody_has_classified"));
        assert!(!may_enter_a_durable_store(""));
    }
}
