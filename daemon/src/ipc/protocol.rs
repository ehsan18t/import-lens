use crate::document::{PackageJsonDependencyEntry, PackageJsonDependencySection};
use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 7;

pub fn is_supported_protocol_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Named,
    Default,
    Namespace,
    Dynamic,
}

// `Ord` lets combined file sizing group entries by runtime in a stable order
// (`file_size.rs`); the derived order is over the variants, not the wire form, so
// it does not affect serialization.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ImportRuntime {
    #[default]
    Component,
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    #[default]
    Low,
}

impl ImportRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSyntax {
    Static,
    Reexport,
    StarReexport,
    Dynamic,
}

impl ImportSyntax {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Reexport => "reexport",
            Self::StarReexport => "star_reexport",
            Self::Dynamic => "dynamic",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedImport {
    pub specifier: String,
    pub package_name: String,
    pub named: Vec<String>,
    pub import_kind: ImportKind,
    pub syntax: ImportSyntax,
    pub runtime: ImportRuntime,
    pub line: u32,
    pub quote_end: SourcePosition,
    pub specifier_range: SourceRange,
    pub statement_range: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRequest {
    pub specifier: String,
    #[serde(rename = "package")]
    pub package_name: String,
    pub version: String,
    pub named: Vec<String>,
    pub import_kind: ImportKind,
    #[serde(default)]
    pub runtime: ImportRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub imports: Vec<ImportRequest>,
    #[serde(default)]
    pub streaming: bool,
}

/// Which freshness state a served size result is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessKind {
    /// Verified current against the files on disk.
    #[default]
    Fresh,
    /// A dependency changed (still present); a background recompute may be in flight.
    Stale,
    /// A dependency could not be checked (transient stat/read error); the last-known
    /// value is shown.
    Unverified,
}

/// Data-layer freshness of a served size result. Carried over IPC and stored in the
/// disk cache; no UI consumes it yet.
///
/// Modeled as a flat struct with a unit-only `kind` enum rather than an enum with
/// struct variants: the disk cache serializes `ImportResult` with `rmp_serde` in
/// compact (positional) mode, which cannot round-trip enum struct/newtype variants
/// (they encode as a map but decode expecting a sequence). A plain struct + unit enum
/// is msgpack-safe (same shape the crate already uses for `ConfidenceLevel`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResultFreshness {
    #[serde(default)]
    pub kind: FreshnessKind,
    /// Only meaningful when `kind == Stale`: a background recompute is in flight.
    #[serde(default)]
    pub revalidating: bool,
    /// Only meaningful when `kind == Unverified`: why verification could not complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ResultFreshness {
    pub fn fresh() -> Self {
        Self::default()
    }

    pub fn stale(revalidating: bool) -> Self {
        Self {
            kind: FreshnessKind::Stale,
            revalidating,
            reason: None,
        }
    }

    pub fn unverified(reason: impl Into<String>) -> Self {
        Self {
            kind: FreshnessKind::Unverified,
            revalidating: false,
            reason: Some(reason.into()),
        }
    }

    /// True for the default `Fresh` state. Used by `skip_serializing_if` so a `Fresh`
    /// result omits the field entirely — which keeps the positional-msgpack DISK
    /// encoding aligned (the disk only ever stores `Fresh`, since freshness is a
    /// serve-time property) and trims the common case over the named IPC encoding.
    pub fn is_fresh(&self) -> bool {
        self.kind == FreshnessKind::Fresh
    }
}

/// The five sizes of a build that **succeeded**.
///
/// The only way to put a size on an [`ImportResult`] (ADR-0006, invariant 1: *a size exists if
/// and only if a build succeeded*). A failing path cannot reach for one, because the constructor
/// that takes it — [`ImportResult::measured`] — is the one that does not take a failure stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeasuredSizes {
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
}

impl MeasuredSizes {
    /// A genuine zero. Reserved for a package that really does ship no runtime bytes — a
    /// declarations-only package (`pipeline::types_only`), which is Measured, not Unmeasured:
    /// the build did not fail, there was simply nothing to build.
    pub const ZERO: Self = Self {
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
    };
}

/// What one kind of non-JavaScript asset contributes to an import's size (B2).
///
/// Every artifact of that kind, each compressed on its own and summed (ADR-0005). These bytes are
/// **already inside** the result's five sizes — this is the composition of a number, not an
/// addendum to it, which is exactly what the old `uncounted_assets` disclosure was not.
///
/// Flat rather than nesting a [`MeasuredSizes`], because the disk cache encoding is positional and
/// a flat row of `u64`s is the shape both sides read most plainly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetContribution {
    pub kind: crate::engine::AssetKind,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
}

/// One import's analysis, in exactly one of the two states a *response* can carry (ADR-0006).
/// The third — Loading — is not an `ImportResult` at all: it is
/// [`ImportAnalysisItem`] with `status: Loading` and no result.
///
/// * **Measured** — the five sizes are `Some`, `unmeasured_stage` is `None`, `error` is `None`.
/// * **Unmeasured** — the five sizes are `None`, `unmeasured_stage` names the stage that could
///   not answer, and `error` carries its message.
///
/// The size fields are **private**, and the only two constructors are [`Self::measured`] and
/// [`Self::unmeasured`]. That is what makes the **fabricated** state unrepresentable: there is no
/// way to put a size on a result except by declaring that a build produced it, so a failing path
/// cannot reach for one. Read a size back through [`Self::sizes`] or [`Self::brotli_bytes`] and the
/// compiler asks the only question a consumer is allowed to ask: **is there a size?** — never "is
/// there an error?".
///
/// What is **not** unrepresentable — and was claimed to be — is *a size together with a
/// request-local stage*. That shape is REAL: a full-package comparison can time out beside genuine
/// primary sizes, or asset I/O/compression can leave a disclosed partial asset size. Deleting it
/// would delete the only evidence that the numeric result or its tree-shaking verdict must not
/// become durable, and no type can stop it anyway — `diagnostics` is an open list whose `stage` is a
/// `String`. The invariant is therefore enforced **at the stores**, by [`Self::is_durable`].
///
/// Serde note: the sizes are plain `Option<u64>` with **no** `skip_serializing_if` — and neither
/// have `module_breakdown` or `shared_bytes`, for the same reason. The dominant `Option` pattern in
/// this file breaks the disk cache for any field that sits mid-struct: the L2 encoding is positional
/// (`rmp_serde::to_vec`), so a skipped field shortens the msgpack array and every field after it
/// decodes off by one. An Unmeasured result carries `module_breakdown: None` beside a
/// `shared_bytes: Some(0)` that `annotate_shared_bytes` stamps on every result, measured or not —
/// exactly that shape. A plain `Option` writes a `nil` placeholder and keeps the array length.
/// `cache::disk` guards this. Only `freshness`, which is the LAST serialized field, may skip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportResult {
    pub specifier: String,
    raw_bytes: Option<u64>,
    minified_bytes: Option<u64>,
    gzip_bytes: Option<u64>,
    brotli_bytes: Option<u64>,
    zstd_bytes: Option<u64>,
    pub cache_hit: bool,
    pub side_effects: bool,
    pub truly_treeshakeable: bool,
    pub is_cjs: bool,
    #[serde(default)]
    pub confidence: ConfidenceLevel,
    #[serde(default)]
    pub confidence_reasons: Vec<String>,
    pub error: Option<String>,
    /// The stage that could not answer, when there is no size. `None` on a measurement.
    ///
    /// Present so a consumer can ask **why** there is no size, not merely whether — the CI gate
    /// must tell a flaky box (`timeout`) apart from a broken package (`parse`), and
    /// `should_cache_result` must cache the second and refuse the first. Plain `Option`, no
    /// `skip_serializing_if`, for the positional-msgpack reason above.
    #[serde(default)]
    unmeasured_stage: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Plain `Option`, NO `skip_serializing_if`: mid-struct, and the L2 encoding is positional.
    /// See the struct's serde note.
    #[serde(default)]
    pub module_breakdown: Option<Vec<ModuleContribution>>,
    /// Plain `Option`, NO `skip_serializing_if`: mid-struct, and the L2 encoding is positional.
    /// See the struct's serde note.
    #[serde(default)]
    pub shared_bytes: Option<u64>,
    /// What each kind of non-JavaScript asset contributed to the five sizes above (B2). Empty when
    /// the import ships none, which is the common case.
    ///
    /// These bytes are already IN the sizes; this says how they are composed, so a reader can see
    /// that a UI kit's number is part JavaScript and part stylesheet. Plain `Vec`, no
    /// `skip_serializing_if`, for the positional-msgpack reason above; `#[serde(default)]` so the
    /// field is simply empty for anything that predates it.
    #[serde(default)]
    pub asset_breakdown: Vec<AssetContribution>,
    /// Freshness of this served value. `#[serde(default)]` so old disk entries decode
    /// as `Fresh`; `skip_serializing_if = is_fresh` so the DISK (positional msgpack)
    /// never emits it (disk only stores `Fresh`), keeping the array aligned past the
    /// conditionally-skipped `module_breakdown`/`shared_bytes`. Non-`Fresh` values
    /// travel only over the named IPC encoding, which is position-independent.
    #[serde(default, skip_serializing_if = "ResultFreshness::is_fresh")]
    pub freshness: ResultFreshness,
    #[serde(default, skip)]
    pub internal_contributions: Vec<ModuleContribution>,
}

impl ImportResult {
    /// **Measured**: a build succeeded and produced these bytes.
    pub fn measured(specifier: impl Into<String>, sizes: MeasuredSizes) -> Self {
        Self {
            specifier: specifier.into(),
            raw_bytes: Some(sizes.raw_bytes),
            minified_bytes: Some(sizes.minified_bytes),
            gzip_bytes: Some(sizes.gzip_bytes),
            brotli_bytes: Some(sizes.brotli_bytes),
            zstd_bytes: Some(sizes.zstd_bytes),
            cache_hit: false,
            side_effects: false,
            truly_treeshakeable: false,
            is_cjs: false,
            confidence: ConfidenceLevel::Low,
            confidence_reasons: Vec::new(),
            error: None,
            unmeasured_stage: None,
            diagnostics: Vec::new(),
            module_breakdown: None,
            shared_bytes: None,
            asset_breakdown: Vec::new(),
            freshness: ResultFreshness::fresh(),
            internal_contributions: Vec::new(),
        }
    }

    /// **Unmeasured**: the build could not answer. No size, ever — not a zero, not an estimate of
    /// the directory on disk, not the entry file measured alone. The stage says whether that is a
    /// property of the package's bytes (deterministic: `parse`, `link`, `output_shape`, …) or of
    /// this request's machine/filesystem state (`timeout`, `panic`, `engine_gone`, `asset_io`, …).
    pub fn unmeasured(
        specifier: impl Into<String>,
        stage: &str,
        message: impl Into<String>,
        details: Vec<String>,
    ) -> Self {
        let message = message.into();
        Self {
            specifier: specifier.into(),
            raw_bytes: None,
            minified_bytes: None,
            gzip_bytes: None,
            brotli_bytes: None,
            zstd_bytes: None,
            cache_hit: false,
            // Nothing was linked, so nothing can be certified free of side effects or
            // tree-shaken away. The conservative reading is the only honest one.
            side_effects: true,
            truly_treeshakeable: false,
            is_cjs: false,
            confidence: ConfidenceLevel::Low,
            confidence_reasons: vec![
                "Analysis failed before a bundle size could be measured.".to_owned(),
            ],
            error: Some(message.clone()),
            unmeasured_stage: Some(stage.to_owned()),
            diagnostics: vec![ImportDiagnostic {
                stage: stage.to_owned(),
                message,
                details,
            }],
            module_breakdown: None,
            shared_bytes: None,
            asset_breakdown: Vec::new(),
            freshness: ResultFreshness::fresh(),
            internal_contributions: Vec::new(),
        }
    }

    /// The sizes, if a build produced them. `None` is the whole point: it is the question every
    /// consumer must ask, and the compiler will not let it be skipped.
    pub fn sizes(&self) -> Option<MeasuredSizes> {
        Some(MeasuredSizes {
            raw_bytes: self.raw_bytes?,
            minified_bytes: self.minified_bytes?,
            gzip_bytes: self.gzip_bytes?,
            brotli_bytes: self.brotli_bytes?,
            zstd_bytes: self.zstd_bytes?,
        })
    }

    pub fn raw_bytes(&self) -> Option<u64> {
        self.raw_bytes
    }

    pub fn minified_bytes(&self) -> Option<u64> {
        self.minified_bytes
    }

    pub fn gzip_bytes(&self) -> Option<u64> {
        self.gzip_bytes
    }

    pub fn brotli_bytes(&self) -> Option<u64> {
        self.brotli_bytes
    }

    pub fn zstd_bytes(&self) -> Option<u64> {
        self.zstd_bytes
    }

    /// The stage that could not answer, on an Unmeasured result.
    pub fn unmeasured_stage(&self) -> Option<&str> {
        self.unmeasured_stage.as_deref()
    }

    /// This result describes **this run of the daemon** rather than the package: a build was lost,
    /// a secondary comparison failed, exact asset bytes were unavailable, or a compressor failed.
    ///
    /// Both are reasons no durable store may take it (ADR-0006, invariant 3) — but they are not the
    /// only ones, so this is not the gate. [`Self::is_durable`] is.
    pub fn is_transient(&self) -> bool {
        self.unmeasured_stage
            .as_deref()
            .is_some_and(crate::pipeline::stage::is_transient)
            || self
                .diagnostics
                .iter()
                .any(|diagnostic| crate::pipeline::stage::is_transient(&diagnostic.stage))
    }

    /// **The gate every durable store applies** (ADR-0006, invariant 3). A store that outlives the
    /// request — the L1 memory cache, the L2 disk cache, the extension's histories — may take this
    /// result only if this is true, and each of those stores asks *itself*, at the insert, rather
    /// than trusting its callers to have asked.
    ///
    /// It is an ALLOWLIST over stages, not a denylist of the currently-known transient ones
    /// (`pipeline::stage::may_enter_a_durable_store` explains why: `entry_metadata` is a bare
    /// `fs::metadata` failure — transient in fact, and absent from every list of the engine's
    /// transient stages). Both places a stage can hide are checked:
    ///
    /// * the result's own `unmeasured_stage` — the build that could not answer;
    /// * every diagnostic — which catches both a **successful** primary measurement whose
    ///   full-package comparison merely parked and a partial asset measurement carrying
    ///   `asset_io`/`compression`.
    ///
    /// A Measured result with no failure diagnostics is durable, which is the overwhelmingly common
    /// case and the one that must stay fast.
    pub fn is_durable(&self) -> bool {
        let stage_is_durable =
            |stage: &str| crate::pipeline::stage::may_enter_a_durable_store(stage);

        self.unmeasured_stage
            .as_deref()
            .is_none_or(&stage_is_durable)
            && self
                .diagnostics
                .iter()
                .all(|diagnostic| stage_is_durable(&diagnostic.stage))
    }

    /// Whether this result is precise enough for a budget verdict.
    ///
    /// Budgetability is deliberately stricter than durability. A deterministic upper bound can be
    /// cached and shown again, but comparing it with a threshold can produce a false failure.
    pub fn is_budgetable(&self) -> bool {
        self.sizes().is_some()
            && self.is_durable()
            && self.diagnostics.iter().all(|diagnostic| {
                !crate::pipeline::stage::prevents_budget_verdict(&diagnostic.stage)
            })
    }

    /// Whether a successful build disclosed supported asset bytes that are absent from its five
    /// sizes. The result may still be reusable when the omission is deterministic, but the number
    /// is a floor and cannot stand in for a complete File Cost.
    pub fn has_uncounted_assets(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == crate::engine::diagnostic_stage::UNCOUNTED_ASSETS)
    }

    /// A **declarations-only** package: a package that resolves to no runtime entry *because it
    /// ships no runtime code*, and is answered Measured — a genuine zero, at High confidence
    /// ([`crate::pipeline::types_only`]).
    ///
    /// The distinction this exists to draw is "resolved to nothing because there is nothing" versus
    /// "could not be resolved". They look identical to [`crate::pipeline::resolver`], which returns
    /// `Err` for both, and the aggregate must tell them apart: a types-only import contributes zero
    /// bytes as a **fact**, so it leaves the file's total complete. Treating it as a gap instead
    /// made every file importing an `@types/…` package a permanent floor — never cached, never
    /// persisted, exit 3 from `importlens check` — which is a large fraction of real TypeScript.
    ///
    /// The sizes must be present: the zero is an *answer*, and an answer has a size.
    pub fn is_types_only(&self) -> bool {
        self.sizes().is_some()
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::TYPES_ONLY)
    }

    /// A **native-binary-only** package: it ships a platform-specific native binary and no
    /// importable JS entry, so it is answered Measured at zero ([`crate::pipeline::native_binary`]).
    /// Like [`Self::is_types_only`], the zero is a fact rather than a gap, so the aggregate leaves
    /// the file's total complete rather than turning it into a permanent floor. The sizes must be
    /// present: the zero is an *answer*, and an answer has a size.
    pub fn is_native_binary_only(&self) -> bool {
        self.sizes().is_some()
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::NATIVE_BINARY_ONLY)
    }

    /// A **native-binary-backed** package whose JS entry resolved and was measured. An informational
    /// flag on a real measurement (the measured size is the JS shim; the tool's work is in the
    /// native binary), so — unlike [`Self::is_native_binary_only`] — the size may be non-zero. The
    /// sizes must be present: the flag only ever rides a successful measurement
    /// ([`crate::pipeline::native_binary::annotate_native_binary`] enforces the same on the way in).
    pub fn is_native_binary(&self) -> bool {
        self.sizes().is_some()
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::NATIVE_BINARY)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDiagnostic {
    pub stage: String,
    pub message: String,
    pub details: Vec<String>,
}

impl ImportDiagnostic {
    pub fn for_stage(stage: &str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.to_owned(),
            message: message.into(),
            details: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleContribution {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAnalysisStatus {
    Loading,
    Ready,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAnalysisItem {
    pub detected: DetectedImport,
    pub status: ImportAnalysisStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<ImportRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImportResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeDocumentRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_document_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeDocumentResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportAnalysisItem>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeSpecifiersRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_specifiers_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub specifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeSpecifiersResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportAnalysisItem>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeDocumentRequest {
    #[serde(rename = "type")]
    #[serde(default = "file_size_document_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
    /// When true, bypass stale-while-revalidate: recompute synchronously and never
    /// serve a stale/unverified size (CI / CLI budget checks require the true current
    /// size). Defaults false for interactive clients, which get SWR.
    #[serde(default)]
    pub force_fresh: bool,
    /// The analysis generation (the triggering document analysis's request id) this
    /// size read belongs to. Echoed back on the resulting SWR `refreshed_results`
    /// push so the client can drop a push a newer analysis has since superseded.
    /// Optional / additive for back-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeDocumentResponse {
    pub version: u32,
    pub request_id: u64,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub imports: Vec<ImportResult>,
    pub states: Vec<ImportAnalysisItem>,
    /// What the five totals above are made of, per non-JavaScript kind — bytes already INSIDE them.
    ///
    /// The per-import result has carried this since B2; the file total did not, so the status bar
    /// and "Show Current File Size" began including stylesheet, wasm and font bytes with no surface
    /// able to say so. A headline that changes meaning without a way to explain itself is the shape
    /// this model exists to avoid, so the composition travels with the number.
    ///
    /// `#[serde(default)]` because the IPC wire is msgpack NAMED: the field is simply absent for a
    /// daemon that predates it.
    #[serde(default)]
    pub asset_breakdown: Vec<AssetContribution>,
    /// These totals are a **floor**, not the file's size: an import that belongs in a fallback sum
    /// was not measured, or a successful import/combined build disclosed supported asset bytes
    /// that are absent from its five sizes (`uncounted_assets`; see
    /// [`crate::pipeline::file_size::FileSizeComputation::incomplete`]).
    ///
    /// It is on the wire because the client has durable stores of its own (the bundle-impact
    /// history), and neither of the other two fields can tell it this: `error` is `None` — the sum
    /// succeeded, it just summed less than the file — and the diagnostics that DO name the missing
    /// import are stage-tagged `file_size_fallback`, which a deterministic per-import failure (a
    /// real, cacheable fact, and no reason to distrust the total) carries too. SRS FR-024a/FR-026c.
    #[serde(default)]
    pub incomplete: bool,
    /// The file's **own combined build** failed, so these totals are not the file's — whatever the
    /// state of its imports ([`crate::pipeline::file_size::FileSizeComputation::degraded`]).
    ///
    /// The second half of ADR-0006's invariant 4, and the one `incomplete` structurally cannot see:
    /// a combined build is strictly larger than any single import's build, so it is the likeliest
    /// thing in the system to hit `BUILD_TIMEOUT` — and when it does, every contributor may still be
    /// perfectly Measured, leaving `incomplete: false`, `error: None`, and an un-deduplicated
    /// per-import SUM on the wire. That sum is a Combined Import Cost, a different quantity from a
    /// File Cost (ADR-0004), and an OVER-count rather than a floor. It must be shown, and it must
    /// never be stored, compared, or judged.
    #[serde(default)]
    pub degraded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

/// A stable per-import identity for the SWR refresh push. The specifier alone is
/// NOT unique — two imports of the same package differ by import kind / named
/// exports but share a specifier — so each pushed result is paired with this to
/// disambiguate variants on the client.
///
/// `runtime` is part of the identity because **it is part of the import**. An Astro document can
/// import the same package, with the same kind and the same named exports, from its frontmatter
/// (Server) and from a client `<script>` (Client), and those are two rows with two different sizes
/// — the two runtimes resolve dependencies under materially different conditions ([ADR-0005]).
/// Without it the two variants collide on one key and the client collapses them into a single row,
/// in the one document shape the runtime split exists for.
///
/// Additive and `#[serde(default)]`, so it needs no protocol-version bump: an older client ignores
/// the extra field, and a payload without it decodes to the default (`Component`) — which is the
/// runtime of every non-Astro document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshedImportIdentity {
    pub specifier: String,
    pub import_kind: ImportKind,
    #[serde(default)]
    pub named: Vec<String>,
    #[serde(default)]
    pub runtime: ImportRuntime,
}

/// Unsolicited server→client push carrying freshly-recomputed sizes for a document
/// after a background SWR revalidation. Unlike a request/response, it is not keyed by
/// `request_id`; the client dispatches it by its `message_type` and locates the store
/// rows by `workspace_root` + `document_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshedResultsResponse {
    #[serde(rename = "type", default = "refreshed_results_message_type")]
    pub message_type: String,
    pub version: u32,
    pub workspace_root: String,
    pub document_path: String,
    pub results: Vec<ImportResult>,
    /// Per-result import identity, index-aligned with `results`, so the client can
    /// disambiguate same-specifier variants. `skip_serializing_if` empty keeps the
    /// push compact and lets an older client ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<RefreshedImportIdentity>,
    /// The analysis generation this refresh was computed for (echoed from the
    /// triggering `FileSizeDocumentRequest`). The client drops the push if a newer
    /// analysis has since superseded it. Optional / additive for back-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_latest: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageJsonDependencyAnalysisItem {
    pub entry: PackageJsonDependencyEntry,
    pub name: String,
    pub section: String,
    pub status: ImportAnalysisStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_hint: Option<RegistryHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImportResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHintMode {
    Off,
    Cached,
    RefreshStale,
    ForceRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintTarget {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintResult {
    pub target: RegistryHintTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<RegistryHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// "cache" or "network" — how this hint was resolved. Optional for
    /// backward compatibility with older daemons/extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsRequest {
    #[serde(rename = "type")]
    #[serde(default = "refresh_registry_hints_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub targets: Vec<RegistryHintTarget>,
    pub mode: RegistryHintMode,
    /// Opaque per-manifest key (the client's document key) that scopes bulk
    /// supersession to one source: a refresh of a different manifest must not
    /// cancel this one's in-flight block. Optional for wire back-compat with
    /// clients that predate the field (they fall back to a shared bucket).
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsResponse {
    pub version: u32,
    pub request_id: u64,
    pub results: Vec<RegistryHintResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

/// The budgets the workspace report can judge, which is the **per-import** one and only that.
///
/// A per-file budget is judged against a **File Cost** — one bundle over all a file's imports, so a
/// module two of them reach is counted once (ADR-0004) — and the report has no such build behind a
/// row. It used to sum each file's per-import brotli and warn off THAT: an upper bound that
/// double-counts every shared module, and a verdict the editor and `importlens check` (which both
/// measure the File Cost) contradicted on the same file under the same budget. The field is gone so
/// that nothing can be judged against a number the report does not have (SRS FR-036i, FR-036q).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_import_brotli_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportRequest {
    #[serde(rename = "type")]
    #[serde(default = "workspace_report_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    #[serde(default)]
    pub budgets: WorkspaceReportBudgets,
}

/// One row of the workspace report.
///
/// The four size fields are `Option` for the same reason [`ImportResult`]'s are: an import the
/// engine could not measure has no size, and `.unwrap_or_default()` here would print **"0 B"** —
/// the sentinel zero this model exists to abolish, in the one surface a user exports and shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportRow {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub line: u32,
    pub runtime: String,
    pub minified_bytes: Option<u64>,
    pub gzip_bytes: Option<u64>,
    pub brotli_bytes: Option<u64>,
    pub zstd_bytes: Option<u64>,
    pub shared_bytes: u64,
    pub confidence: String,
    pub confidence_reasons: String,
    pub top_modules: String,
    pub warning: String,
    pub module_contributions: Vec<ModuleContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportTreemapItem {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub brotli_bytes: u64,
    pub percentage: u64,
    pub confidence: String,
}

/// Every import of one specifier across the workspace, and what they cost **together** — three files
/// importing `react` is three Reacts (see [`WorkspaceReportSummary::combined_import_cost_brotli_bytes`]).
/// The field was `total_brotli_bytes`, and a "total" of fifty Reacts is a number no project ships.
/// Under the honest label the panel is finally saying the thing it exists to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateImportGroup {
    pub specifier: String,
    pub count: u64,
    pub combined_import_cost_brotli_bytes: u64,
    pub source_files: Vec<String>,
}

/// One module, and the imports that reach it.
///
/// **A module has a size, and its importing sites have a combined cost, and they are two different
/// numbers.** This group carried one field, `total_bytes` — the module's bytes added up once per
/// importing row — and the report rendered it under the header "Total Bytes". So
/// `react-dom/index.js`, which **is 100 kB** and is reached by three imports (`react-dom`,
/// `react-dom/client`, `react-dom/server`), was reported as **300 kB**. That is a *Combined Import
/// Cost* wearing the one word [ADR-0004] exists to abolish, one table below the headline that was
/// relabelled for exactly this reason.
///
/// - [`Self::module_bytes`] — what the module **is**: the largest single rendered contribution seen
///   across the builds that reached it. (Two builds may tree-shake it differently, so it need not be
///   one number; the largest is the module at its fullest, and it is a byte count that really came
///   out of a build rather than an average of two that did not.)
/// - [`Self::combined_import_cost_bytes`] — what the **sites** pay: that module counted once per
///   importing site. An **upper bound**, because each import is priced as though the application
///   were otherwise empty, and never a size.
///
/// [ADR-0004]: ../../../docs/adr/0004-import-lens-measures-imports-not-bundles.md
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateModuleGroup {
    pub module_path: String,
    pub basename: String,
    /// The number of imports that reach this module.
    pub count: u64,
    /// The module's own rendered size.
    pub module_bytes: u64,
    /// The module counted once per importing site: a Combined Import Cost, an upper bound.
    pub combined_import_cost_bytes: u64,
    pub specifiers: Vec<String>,
    pub vendored: bool,
}

/// The report's headline figure is a **Combined Import Cost**: the sum of independent Import Costs,
/// each priced as though the application were otherwise empty ([ADR-0004]).
///
/// It counts a dependency at **every site it is imported from** — `react` in fifty files is fifty
/// Reacts, and a single `import React, { useState } from "react"` is **two imports** and is counted
/// **twice**. That is not an error to be corrected: subtracting the overlap would assert a
/// project-level bundle quantity this product deliberately does not model, and compressed sizes are
/// not additive anyway, so the sum is an **upper bound**. It **ranks** imports and **apportions
/// blame**; it is never a size.
///
/// It was called `total_brotli_bytes` and rendered as "Total Brotli", which every reader takes to
/// mean *what my project ships*. The arithmetic was right; the word was the defect. The treemap's
/// percentages are shares of this figure, not of a bundle.
///
/// [ADR-0004]: ../../../docs/adr/0004-import-lens-measures-imports-not-bundles.md
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportSummary {
    pub import_count: u64,
    pub combined_import_cost_brotli_bytes: u64,
    pub low_confidence_count: u64,
    pub medium_confidence_count: u64,
    pub conservative_count: u64,
    pub budget_violation_count: u64,
    pub duplicate_imports: Vec<DuplicateImportGroup>,
    pub shared_modules: Vec<DuplicateModuleGroup>,
    pub treemap: Vec<WorkspaceReportTreemapItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportResponse {
    pub version: u32,
    pub request_id: u64,
    pub rows: Vec<WorkspaceReportRow>,
    pub summary: WorkspaceReportSummary,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzePackageJsonRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_package_json_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub include_registry_hints: bool,
    #[serde(default)]
    pub force_registry_refresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_hint_mode: Option<RegistryHintMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzePackageJsonResponse {
    pub version: u32,
    pub request_id: u64,
    pub sections: Vec<PackageJsonDependencySection>,
    pub states: Vec<PackageJsonDependencyAnalysisItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteImportMembersRequest {
    #[serde(rename = "type")]
    #[serde(default = "complete_import_members_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
    pub cursor_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteImportMembersResponse {
    pub version: u32,
    pub request_id: u64,
    pub specifier: Option<String>,
    pub exports: Vec<String>,
    pub imported_names: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    #[serde(default = "hello_message_type")]
    pub message_type: String,
    pub version: u32,
    pub workspace_root: String,
    pub storage_path: String,
    pub enable_disk_cache: bool,
    #[serde(default = "default_cache_max_size_mb")]
    pub cache_max_size_mb: u64,
    // Registry-metadata store byte budget (`importLens.registryCacheMaxSizeMB`).
    // Serde-defaulted so an older client that omits it keeps the daemon default.
    #[serde(default = "default_registry_cache_max_size_mb")]
    pub registry_cache_max_size_mb: u64,
    pub log_level: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInvalidateMessage {
    #[serde(rename = "type")]
    #[serde(default = "cache_invalidate_message_type")]
    pub message_type: String,
    #[serde(rename = "package")]
    pub package_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInvalidateAllMessage {
    #[serde(rename = "type")]
    #[serde(default = "cache_invalidate_all_message_type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrewarmPackageJsonMessage {
    #[serde(rename = "type")]
    #[serde(default = "prewarm_package_json_message_type")]
    pub message_type: String,
    pub package_json_path: String,
    pub active_document_path: String,
}

/// The watcher's "something the daemon memoized is no longer true" message.
///
/// It carries two kinds of path because two kinds of file feed the resolvers, and both were only
/// ever half-watched: a `node_modules/<pkg>/package.json` (an install / uninstall) and a
/// `tsconfig.json` / `jsconfig.json` (the workspace's **alias table**, the sole discriminator
/// between a path alias and a package that is not installed). The second is new; without it the
/// alias table the daemon parsed at startup was the one it used until it died, and the repair the
/// SRS prescribes for an unrecognized alias — add the `paths` entry — had no effect at all.
///
/// `tsconfig_paths` is `#[serde(default)]`, so an older client that sends only
/// `package_json_paths` still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeModulesChangedMessage {
    #[serde(rename = "type")]
    #[serde(default = "node_modules_changed_message_type")]
    pub message_type: String,
    pub package_json_paths: Vec<String>,
    #[serde(default)]
    pub tsconfig_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumerateExportsRequest {
    #[serde(rename = "type")]
    #[serde(default = "enumerate_exports_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub specifier: String,
    #[serde(rename = "package")]
    pub package_name: String,
    pub package_version: String,
    /// The import's UTF-16 cursor offset in `active_document_path`, when the caller has
    /// one. The daemon classifies it into a runtime (`document::runtime_at_offset`) so the
    /// enumeration resolves under the same conditions the size will — an import in Astro
    /// frontmatter (Server) must be enumerated under node conditions, not browser. Absent
    /// (a plain file, or an older client), the classifier default is `Component`.
    /// `#[serde(default)]` keeps an older client that omits it decoding.
    #[serde(default)]
    pub cursor_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumerateExportsResponse {
    pub version: u32,
    pub request_id: u64,
    pub specifier: String,
    pub exports: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeRequest {
    #[serde(rename = "type")]
    #[serde(default = "file_size_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub imports: Vec<ImportRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeResponse {
    pub version: u32,
    pub request_id: u64,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub imports: Vec<ImportResult>,
    /// These totals are a **floor**, not the file's size — the same flag, and the same meaning, as
    /// [`FileSizeDocumentResponse::incomplete`].
    ///
    /// It was missing here, which made this the one surface where a floor and a measurement are
    /// indistinguishable: the legacy `file_size` request answers with the same
    /// [`crate::pipeline::file_size::FileSizeComputation`] and simply dropped the one field that
    /// says the number is short. Additive and `#[serde(default)]`, so an older client that ignores
    /// it is no worse off than it was.
    #[serde(default)]
    pub incomplete: bool,
    /// The file's own combined build failed — the same flag, and the same meaning, as
    /// [`FileSizeDocumentResponse::degraded`]. Missing here for the same reason `incomplete` was.
    #[serde(default)]
    pub degraded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheShardInfo {
    pub shard_id: String,
    pub project_root: String,
    pub normalized_root: String,
    pub cache_path: String,
    pub size_bytes: u64,
    pub last_used_millis: Option<u64>,
    pub loaded: bool,
    /// Number of cache entries this shard holds, read O(1) from the C1 per-shard
    /// SUMMARY (never a CACHE_TABLE scan). `#[serde(default)]` so an older peer
    /// that predates the field still decodes it as 0. The comparable "recency"
    /// signal §8/X-24 asks for is already carried by `last_used_millis` above,
    /// so no separate `last_used` field is added.
    #[serde(default)]
    pub entry_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheOperationResult {
    pub shard_id: String,
    pub project_root: String,
    pub cache_path: String,
    pub removed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatusRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_status_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatusResponse {
    pub version: u32,
    pub request_id: u64,
    pub total_size_bytes: u64,
    pub project_count: usize,
    pub max_size_mb: u64,
    pub current_project: Option<CacheShardInfo>,
    /// Σ of every shard's logical (envelope) bytes from the C1 rollups — the
    /// budget-tracked total, distinct from `total_size_bytes` (the physical
    /// on-disk directory footprint, which includes redb overhead and metadata).
    /// `#[serde(default)]` so version skew degrades gracefully.
    #[serde(default)]
    pub total_bytes: u64,
    /// The global disk-byte budget the BudgetCoordinator enforces
    /// (`cache_max_size_mb` expressed in bytes; 0 disables the budget).
    #[serde(default)]
    pub budget_bytes: u64,
    /// Serialized size of the shared npm-registry metadata snapshot — a single
    /// length measurement of the persisted envelope, not a scan.
    #[serde(default)]
    pub registry_size_bytes: u64,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheListRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_list_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheListResponse {
    pub version: u32,
    pub request_id: u64,
    pub shards: Vec<CacheShardInfo>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRemoveScope {
    CurrentProject,
    Selected,
    All,
    /// Reclaim orphaned caches (RB-17): remove shards whose project root was
    /// moved/deleted, and scrub stale/uninstalled entries from surviving shards.
    /// Complements the automatic reclaim — entry-level staleness self-heals on
    /// access (name invalidation + the freshness `Gone` eviction), but a whole
    /// abandoned project is never reopened, so its shard is reclaimed only here
    /// (manual button) or by the throttled maintenance-tick sweep. Drive-safe:
    /// an offline/unplugged drive keeps its shard (`ProjectCacheRegistry::purge_orphans`
    /// via `classify_project_root`, X-3).
    Orphans,
    /// Clear ONLY the shared npm-hint registry metadata store, leaving every
    /// bundle shard (and its derived L1/graph caches) untouched. Serializes as
    /// `"registry"`; older peers that predate this variant log-and-skip it.
    Registry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRemoveRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_remove_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub scope: CacheRemoveScope,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub shard_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRemoveResponse {
    pub version: u32,
    pub request_id: u64,
    pub removed: Vec<CacheOperationResult>,
    pub failed: Vec<CacheOperationResult>,
    /// Stale entries scrubbed from caches that were KEPT, and stale registry metadata pruned.
    ///
    /// The orphan purge does two things and only ever reported one of them. A run that removed no
    /// shard reported "nothing to reclaim" while having dropped entries from surviving shards and
    /// expired registry metadata — a zero shown for work that happened. Both counts were already
    /// computed and thrown away (`purge_orphan_entries` returns a `usize`;
    /// `purge_expired_metadata` went to a debug log), so this surfaces what the daemon already knew.
    #[serde(default)]
    pub scrubbed_entries: usize,
    #[serde(default)]
    pub registry_entries_removed: usize,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownMessage {
    #[serde(rename = "type")]
    #[serde(default = "shutdown_message_type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClientMessage {
    Hello(HelloMessage),
    AnalyzeDocument(AnalyzeDocumentRequest),
    AnalyzePackageJson(AnalyzePackageJsonRequest),
    AnalyzeSpecifiers(AnalyzeSpecifiersRequest),
    Batch(BatchRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    NodeModulesChanged(NodeModulesChangedMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
    FileSizeDocument(FileSizeDocumentRequest),
    CompleteImportMembers(CompleteImportMembersRequest),
    CacheStatus(CacheStatusRequest),
    CacheList(CacheListRequest),
    CacheRemove(CacheRemoveRequest),
    RefreshRegistryHints(RefreshRegistryHintsRequest),
    WorkspaceReport(WorkspaceReportRequest),
    Shutdown(ShutdownMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum ClientMessageWire {
    Typed(TypedClientMessage),
    Batch(BatchRequestWire),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TypedClientMessage {
    Hello(HelloMessage),
    AnalyzeDocument(AnalyzeDocumentRequest),
    AnalyzePackageJson(AnalyzePackageJsonRequest),
    AnalyzeSpecifiers(AnalyzeSpecifiersRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    NodeModulesChanged(NodeModulesChangedMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
    FileSizeDocument(FileSizeDocumentRequest),
    CompleteImportMembers(CompleteImportMembersRequest),
    CacheStatus(CacheStatusRequest),
    CacheList(CacheListRequest),
    CacheRemove(CacheRemoveRequest),
    RefreshRegistryHints(RefreshRegistryHintsRequest),
    WorkspaceReport(WorkspaceReportRequest),
    Shutdown(ShutdownMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchRequestWire {
    version: u32,
    request_id: u64,
    workspace_root: String,
    active_document_path: String,
    imports: Vec<ImportRequest>,
    #[serde(default)]
    streaming: bool,
}

impl<'de> Deserialize<'de> for ClientMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ClientMessageWire::deserialize(deserializer).map(Into::into)
    }
}

impl From<ClientMessageWire> for ClientMessage {
    fn from(message: ClientMessageWire) -> Self {
        match message {
            ClientMessageWire::Typed(message) => message.into(),
            ClientMessageWire::Batch(request) => Self::Batch(request.into()),
        }
    }
}

impl From<TypedClientMessage> for ClientMessage {
    fn from(message: TypedClientMessage) -> Self {
        match message {
            TypedClientMessage::Hello(message) => Self::Hello(message),
            TypedClientMessage::AnalyzeDocument(message) => Self::AnalyzeDocument(message),
            TypedClientMessage::AnalyzePackageJson(message) => Self::AnalyzePackageJson(message),
            TypedClientMessage::AnalyzeSpecifiers(message) => Self::AnalyzeSpecifiers(message),
            TypedClientMessage::CacheInvalidate(message) => Self::CacheInvalidate(message),
            TypedClientMessage::CacheInvalidateAll(message) => Self::CacheInvalidateAll(message),
            TypedClientMessage::PrewarmPackageJson(message) => Self::PrewarmPackageJson(message),
            TypedClientMessage::NodeModulesChanged(message) => Self::NodeModulesChanged(message),
            TypedClientMessage::EnumerateExports(message) => Self::EnumerateExports(message),
            TypedClientMessage::FileSize(message) => Self::FileSize(message),
            TypedClientMessage::FileSizeDocument(message) => Self::FileSizeDocument(message),
            TypedClientMessage::CompleteImportMembers(message) => {
                Self::CompleteImportMembers(message)
            }
            TypedClientMessage::CacheStatus(message) => Self::CacheStatus(message),
            TypedClientMessage::CacheList(message) => Self::CacheList(message),
            TypedClientMessage::CacheRemove(message) => Self::CacheRemove(message),
            TypedClientMessage::RefreshRegistryHints(message) => {
                Self::RefreshRegistryHints(message)
            }
            TypedClientMessage::WorkspaceReport(message) => Self::WorkspaceReport(message),
            TypedClientMessage::Shutdown(message) => Self::Shutdown(message),
        }
    }
}

impl From<BatchRequestWire> for BatchRequest {
    fn from(request: BatchRequestWire) -> Self {
        Self {
            version: request.version,
            request_id: request.request_id,
            workspace_root: request.workspace_root,
            active_document_path: request.active_document_path,
            imports: request.imports,
            streaming: request.streaming,
        }
    }
}

fn hello_message_type() -> String {
    "hello".to_owned()
}

fn default_cache_max_size_mb() -> u64 {
    512
}

fn default_registry_cache_max_size_mb() -> u64 {
    // 32 MiB, matching `REGISTRY_CACHE_MAX_SIZE_BYTES` and the extension's
    // `registryCacheMaxSizeMB` default, so an omitted field is a no-op.
    32
}

fn analyze_document_message_type() -> String {
    "analyze_document".to_owned()
}

fn analyze_package_json_message_type() -> String {
    "analyze_package_json".to_owned()
}

fn analyze_specifiers_message_type() -> String {
    "analyze_specifiers".to_owned()
}

fn cache_invalidate_message_type() -> String {
    "cache_invalidate".to_owned()
}

fn cache_invalidate_all_message_type() -> String {
    "cache_invalidate_all".to_owned()
}

fn prewarm_package_json_message_type() -> String {
    "prewarm_package_json".to_owned()
}

fn node_modules_changed_message_type() -> String {
    "node_modules_changed".to_owned()
}

fn enumerate_exports_message_type() -> String {
    "enumerate_exports".to_owned()
}

fn file_size_message_type() -> String {
    "file_size".to_owned()
}

fn file_size_document_message_type() -> String {
    "file_size_document".to_owned()
}

fn refreshed_results_message_type() -> String {
    "refreshed_results".to_owned()
}

fn complete_import_members_message_type() -> String {
    "complete_import_members".to_owned()
}

fn cache_status_message_type() -> String {
    "cache_status".to_owned()
}

fn cache_list_message_type() -> String {
    "cache_list".to_owned()
}

fn cache_remove_message_type() -> String {
    "cache_remove".to_owned()
}

fn shutdown_message_type() -> String {
    "shutdown".to_owned()
}

fn refresh_registry_hints_message_type() -> String {
    "refresh_registry_hints".to_owned()
}

fn workspace_report_message_type() -> String {
    "workspace_report".to_owned()
}
