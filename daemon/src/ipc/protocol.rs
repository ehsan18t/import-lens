use crate::document::{PackageJsonDependencyEntry, PackageJsonDependencySection};
use serde::{Deserialize, Serialize};

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

// `Ord` gives combined file sizing a stable runtime grouping (`file_size.rs`); it orders
// variants, not the wire form.
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

/// Data-layer freshness of a served size result, carried over IPC. No UI consumes it.
///
/// A flat struct with a unit-only `kind` enum, never an enum with struct variants: the disk
/// cache's positional `rmp_serde` encoding cannot round-trip struct/newtype variants.
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

    /// True for the default `Fresh` state. Drives `skip_serializing_if`, so the disk (which only
    /// stores `Fresh`; freshness is a serve-time property) never writes the field.
    pub fn is_fresh(&self) -> bool {
        self.kind == FreshnessKind::Fresh
    }
}

/// The five sizes of a build that **succeeded**.
///
/// The only way to put a size on an [`ImportResult`] (ADR-0006, invariant 1: a size exists if and
/// only if a build succeeded): [`ImportResult::measured`] takes it and takes no failure stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeasuredSizes {
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
}

impl MeasuredSizes {
    /// A genuine zero, reserved for a package that ships no runtime bytes (e.g.
    /// `pipeline::types_only`). Measured, not Unmeasured: there was nothing to build.
    pub const ZERO: Self = Self {
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
    };
}

/// What one kind of non-JavaScript asset contributes to an import's size.
///
/// Every artifact of that kind, each compressed on its own and summed (ADR-0005). These bytes are
/// already inside the result's five sizes: a composition, not an addendum. Flat rather than nesting
/// a [`MeasuredSizes`], because the disk cache encoding is positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetContribution {
    pub kind: crate::engine::AssetKind,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
}

/// One import's analysis, in one of the two states a response can carry (ADR-0006). The third,
/// Loading, is an [`ImportAnalysisItem`] with `status: Loading` and no result.
///
/// * **Measured**: the five sizes are `Some`, `unmeasured_stage` and `error` are `None`.
/// * **Unmeasured**: the five sizes are `None`, `unmeasured_stage` names the stage that could
///   not answer, and `error` carries its message.
///
/// The size fields are private and the only constructors are [`Self::measured`] and
/// [`Self::unmeasured`], so a fabricated size is unrepresentable. Consumers read sizes through
/// [`Self::sizes`] and ask "is there a size?", never "is there an error?".
///
/// A size together with a request-local diagnostic stage is representable and real (a
/// full-package comparison timing out beside genuine sizes, a partial asset size). Durability is
/// therefore enforced at the stores, by [`Self::is_durable`].
///
/// Serde: the sizes, `module_breakdown` and `shared_bytes` are plain `Option`s with no
/// `skip_serializing_if`. The L2 encoding is positional (`rmp_serde::to_vec`), so skipping a
/// mid-struct field shifts every later field; a plain `Option` writes a `nil` placeholder.
/// `cache::disk` guards this. Only `freshness`, the last serialized field, may skip.
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
    /// Tells a flaky box (`timeout`) from a broken package (`parse`): the CI gate and the cache
    /// treat them differently. Plain `Option` (see the serde note).
    #[serde(default)]
    unmeasured_stage: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Plain `Option`, no `skip_serializing_if` (see the serde note).
    #[serde(default)]
    pub module_breakdown: Option<Vec<ModuleContribution>>,
    /// Plain `Option`, no `skip_serializing_if` (see the serde note).
    #[serde(default)]
    pub shared_bytes: Option<u64>,
    /// What each kind of non-JavaScript asset contributed to the five sizes above (bytes already
    /// inside them). Usually empty. Plain `Vec`, no `skip_serializing_if` (see the serde note).
    #[serde(default)]
    pub asset_breakdown: Vec<AssetContribution>,
    /// Freshness of this served value. Skipped when `Fresh`, so the disk (which only stores
    /// `Fresh`) never writes it; non-`Fresh` values travel only over the named IPC encoding.
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

    /// **Unmeasured**: the build could not answer. No size, ever: not a zero, not an estimate. The
    /// stage says whether that is a property of the package's bytes (deterministic: `parse`,
    /// `link`, `output_shape`, …) or of this request's state (`timeout`, `panic`, `asset_io`, …).
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
            // Nothing was linked, so nothing can be certified side-effect free.
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

    /// The sizes, if a build produced them.
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
    /// Not the durability gate (ADR-0006, invariant 3): [`Self::is_durable`] is.
    pub fn is_transient(&self) -> bool {
        self.unmeasured_stage
            .as_deref()
            .is_some_and(crate::pipeline::stage::is_transient)
            || self
                .diagnostics
                .iter()
                .any(|diagnostic| crate::pipeline::stage::is_transient(&diagnostic.stage))
    }

    /// The gate every durable store applies (ADR-0006, invariant 3). A store that outlives the
    /// request (L1, L2, the extension's histories) checks this itself at the insert, rather than
    /// trusting its callers.
    ///
    /// An allowlist over stages, not a denylist (see `pipeline::stage::may_enter_a_durable_store`).
    /// Checks both the result's own `unmeasured_stage` and every diagnostic, which catches a
    /// successful measurement whose full-package comparison parked or whose asset bytes were
    /// partial (`asset_io`/`compression`). The common case, Measured with no failure diagnostics,
    /// must stay fast.
    pub fn is_durable(&self) -> bool {
        let stage_is_durable =
            |stage: &str| crate::pipeline::stage::may_enter_a_durable_store(stage);

        self.unmeasured_stage
            .as_deref()
            .is_none_or(stage_is_durable)
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

    /// Whether a successful build disclosed bytes that are absent from its five sizes
    /// ([`crate::pipeline::stage::marks_a_floor`]). The result may still be reusable when the
    /// omission is deterministic, but the number is a floor and cannot stand in for a complete
    /// File Cost.
    pub fn is_floor(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| crate::pipeline::stage::marks_a_floor(&diagnostic.stage))
    }

    /// A declarations-only package: it resolves to no runtime entry because it ships no runtime
    /// code, and is answered Measured, a genuine zero at High confidence
    /// ([`crate::pipeline::types_only`]).
    ///
    /// [`crate::pipeline::resolver`] returns `Err` both for this and for "could not be resolved";
    /// the aggregate must tell them apart, because a types-only zero is a fact that leaves the
    /// file's total complete, not a gap that makes it a floor. The sizes must be present.
    pub fn is_types_only(&self) -> bool {
        self.sizes().is_some()
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::TYPES_ONLY)
    }

    /// A **native-binary-only** package: it ships a platform-specific native binary and no
    /// importable JS entry, so it is answered Measured at zero ([`crate::pipeline::native_binary`]).
    /// Like [`Self::is_types_only`], the zero is a fact rather than a gap, so the file's total
    /// stays complete. The sizes must be present.
    pub fn is_native_binary_only(&self) -> bool {
        self.sizes().is_some()
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::NATIVE_BINARY_ONLY)
    }

    /// A native-binary-backed package whose JS entry resolved and was measured: an informational
    /// flag on a real (possibly non-zero) measurement of the JS shim. The sizes must be present
    /// ([`crate::pipeline::native_binary::annotate_native_binary`] enforces this on the way in).
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
    /// When true, bypass stale-while-revalidate: recompute synchronously and never serve a
    /// stale/unverified size (CI / CLI budget checks). Defaults false.
    #[serde(default)]
    pub force_fresh: bool,
    /// The triggering document analysis's request id. Echoed on the SWR `refreshed_results` push
    /// so the client can drop a superseded push; also marks the read as interactive.
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
    /// What the five totals above are made of, per non-JavaScript kind (bytes already inside them),
    /// so the headline can explain stylesheet, wasm and font bytes.
    #[serde(default)]
    pub asset_breakdown: Vec<AssetContribution>,
    /// These totals are a floor, not the file's size: an import in a fallback sum was not
    /// measured, or a build disclosed `uncounted_assets` (see
    /// [`crate::pipeline::file_size::FileSizeComputation::incomplete`]).
    ///
    /// On the wire for the client's own durable stores: `error` is `None` (the sum succeeded), and
    /// the `file_size_fallback` diagnostics also ride deterministic, cacheable per-import failures,
    /// so neither can signal this. SRS FR-024a/FR-026c.
    #[serde(default)]
    pub incomplete: bool,
    /// The file's own combined build failed, so these totals are not the file's, whatever the
    /// state of its imports ([`crate::pipeline::file_size::FileSizeComputation::degraded`]).
    ///
    /// The half of ADR-0006's invariant 4 that `incomplete` cannot see: with every contributor
    /// Measured, the wire carries an un-deduplicated per-import sum, a Combined Import Cost
    /// (ADR-0004) that over-counts. It must be shown, and never stored, compared, or judged.
    #[serde(default)]
    pub degraded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

/// A stable per-import identity for the SWR refresh push. The specifier alone is not unique (two
/// imports of one package can differ by kind or named exports), so each pushed result is paired
/// with this.
///
/// `runtime` is part of the identity: an Astro document can import the same package identically
/// from frontmatter (Server) and a client `<script>` (Client), two rows with different sizes
/// (ADR-0005). A payload without it decodes to `Component`, the runtime of every non-Astro
/// document.
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
    /// Per-result import identity, index-aligned with `results`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<RefreshedImportIdentity>,
    /// The analysis generation this push was computed for. The client drops it if a newer
    /// analysis has superseded it.
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
    /// How this hint was resolved: "cache" or "network".
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
    /// Opaque per-manifest key (the client's document key) that scopes bulk supersession to one
    /// source. Absent, the request falls into a shared bucket.
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

/// The budgets the workspace report can judge: the per-import one only.
///
/// A per-file budget is judged against a File Cost (one bundle over all a file's imports,
/// ADR-0004), and the report has no such build behind a row. Do not add one judged against summed
/// per-import sizes: that double-counts shared modules (SRS FR-036i, FR-036q).
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
/// The four size fields are `Option` for the same reason [`ImportResult`]'s are: an unmeasured
/// import has no size, and defaulting would print a fabricated "0 B".
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

/// Every import of one specifier across the workspace, and what they cost together: three files
/// importing `react` is three Reacts (see
/// [`WorkspaceReportSummary::combined_import_cost_brotli_bytes`]). Never call it a total.
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
/// A module's size and its importing sites' combined cost are two different numbers ([ADR-0004]):
/// a 100 kB `react-dom/index.js` reached by three imports costs those sites 300 kB.
///
/// - [`Self::module_bytes`]: what the module is, the largest single rendered contribution seen
///   across the builds that reached it (builds may tree-shake it differently; the largest is a
///   real byte count, not an average).
/// - [`Self::combined_import_cost_bytes`]: that module counted once per importing site. An upper
///   bound, never a size.
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
/// It counts a dependency at every site it is imported from: `react` in fifty files is fifty
/// Reacts, and `import React, { useState } from "react"` is two imports, counted twice. Do not
/// subtract the overlap: that would assert a project-level bundle quantity this product does not
/// model, and compressed sizes are not additive. An upper bound that ranks imports, never a size
/// or a "total". The treemap's percentages are shares of this figure.
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
    /// The analysis root the client uses for files governed by this manifest, so the prewarm fills
    /// the shard interactive analysis reads. Absent, the daemon derives it
    /// (`prefetch::prewarm_root`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
}

/// The watcher's "something the daemon memoized is no longer true" message.
///
/// Carries both kinds of file that feed the resolvers: a `node_modules/<pkg>/package.json` (an
/// install or uninstall) and a `tsconfig.json` / `jsconfig.json` (the workspace's alias table, the
/// sole discriminator between a path alias and a package that is not installed).
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
    /// The import's UTF-16 cursor offset in `active_document_path`, when the caller has one. The
    /// daemon classifies it into a runtime (`document::runtime_at_offset`) so enumeration resolves
    /// under the same conditions as the size. Absent, the runtime is `Component`.
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
pub struct CacheShardInfo {
    pub shard_id: String,
    pub project_root: String,
    pub normalized_root: String,
    pub cache_path: String,
    pub size_bytes: u64,
    pub last_used_millis: Option<u64>,
    pub loaded: bool,
    /// Number of cache entries this shard holds, read O(1) from the per-shard summary (never a
    /// `CACHE_TABLE` scan).
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
    /// Sum of every shard's logical (envelope) bytes from the per-shard summaries: the
    /// budget-tracked total, distinct from `total_size_bytes` (the physical on-disk footprint).
    #[serde(default)]
    pub total_bytes: u64,
    /// The global disk-byte budget the BudgetCoordinator enforces
    /// (`cache_max_size_mb` expressed in bytes; 0 disables the budget).
    #[serde(default)]
    pub budget_bytes: u64,
    /// Serialized size of the shared npm-registry metadata snapshot (a length, not a scan).
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
    /// Reclaim orphaned caches: remove shards whose project root was moved or deleted, and scrub
    /// stale entries from surviving shards. An abandoned project is never reopened, so its shard
    /// is reclaimed only here or by the per-open maintenance sweep. Drive-safe: an offline drive
    /// keeps its shard (`ProjectCacheRegistry::purge_orphans` via `classify_project_root`).
    Orphans,
    /// Clear only the shared npm-hint registry metadata store, leaving every bundle shard (and its
    /// derived L1/graph caches) untouched.
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
    /// Stale entries scrubbed from caches that were kept, so an orphan purge that removed no shard
    /// does not report "nothing to reclaim".
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

/// Every frame a client sends, told apart by its `type` field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(HelloMessage),
    AnalyzeDocument(AnalyzeDocumentRequest),
    AnalyzePackageJson(AnalyzePackageJsonRequest),
    AnalyzeSpecifiers(AnalyzeSpecifiersRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    NodeModulesChanged(NodeModulesChangedMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSizeDocument(FileSizeDocumentRequest),
    CompleteImportMembers(CompleteImportMembersRequest),
    CacheStatus(CacheStatusRequest),
    CacheList(CacheListRequest),
    CacheRemove(CacheRemoveRequest),
    RefreshRegistryHints(RefreshRegistryHintsRequest),
    WorkspaceReport(WorkspaceReportRequest),
    Shutdown(ShutdownMessage),
}

fn hello_message_type() -> String {
    "hello".to_owned()
}

fn default_cache_max_size_mb() -> u64 {
    512
}

fn default_registry_cache_max_size_mb() -> u64 {
    // Matches `REGISTRY_CACHE_MAX_SIZE_BYTES` and the extension's `registryCacheMaxSizeMB` default.
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
