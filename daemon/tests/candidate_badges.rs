//! Real-package **badge** baseline (design §10.6, amendment I23).
//!
//! The accuracy oracle (`scripts/accuracy-compare.mjs`) compares **bytes**. Nothing compared
//! **claims**. `side_effects`, `truly_treeshakeable` and `confidence` live on `ImportResult`, and
//! `candidate_packages.rs` stops at the engine boundary and never builds one — so the three flags
//! the user actually reads had no real-package ground truth anywhere in the repo. This file is that
//! ground truth.
//!
//! **Every expectation below is derived from what the package DECLARES**, not copied from a
//! recorded run: a `sideEffects` field (or its absence), a CJS or ESM entry, and whether the
//! requested export is a slice of the package or its whole surface. Each row says which. A run that
//! disagrees with the declaration is the run being wrong, not the row.
//!
//! Fixture installation is an explicit setup step; these tests perform no network access:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --release --locked \
//!     --test candidate_badges -- --ignored --nocapture
//! ```

use import_lens_daemon::engine::AssetKind;
use import_lens_daemon::ipc::protocol::{ConfidenceLevel, ImportResult, ImportRuntime};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

mod common;

struct BadgeExpectation {
    package: &'static str,
    named: &'static [&'static str],
    side_effects: bool,
    truly_treeshakeable: bool,
    confidence: ConfidenceLevel,
    /// The declaration the three expectations above were derived FROM. It is the row's evidence:
    /// if the code changes what a declaration means, this sentence is what tells the next reader
    /// whether the row moved because the package changed or because we did.
    derivation: &'static str,
}

/// The pinned real-package set, one row per package in `scripts/accuracy-fixtures/package.json`.
/// `every_pinned_fixture_has_a_badge_row` holds the two in step.
const EXPECTATIONS: &[BadgeExpectation] = &[
    BadgeExpectation {
        package: "date-fns",
        named: &["format"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: false` and is a wide barrel of independent functions, \
                     so one named import is far below 95% of the package",
    },
    BadgeExpectation {
        package: "lodash-es",
        named: &["debounce"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: false`; `debounce` is one function of a wide barrel",
    },
    BadgeExpectation {
        package: "css-tree",
        named: &["parse"],
        side_effects: false,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: false`, so it is not side-effectful and confidence \
                     stays High — but `lib/index.js` publishes its API by DESTRUCTURING one \
                     fully-configured object (`export const { parse, generate, walk, lexer } = \
                     syntax`, built from the lexer + parser + walker configs together), so `parse` \
                     alone drags in the whole syntax. This is precisely the case §10.6 exists to \
                     name: a package that declares itself side-effect-free whose graph does not \
                     support granular export isolation",
    },
    BadgeExpectation {
        package: "uuid",
        named: &["v4"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: false`; `v4` is one generator among v1/v3/v5/v6/v7 \
                     and the validation helpers",
    },
    BadgeExpectation {
        package: "zod",
        named: &["z"],
        side_effects: false,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: false` — so the badge is Not-side-effectful and \
                     confidence stays High — but `z` IS the library's entire schema surface, so \
                     the named import cannot be under 95% of the full package. The tree-shaking \
                     claim is about this import, not about the package's hygiene",
    },
    BadgeExpectation {
        package: "react",
        named: &["useState"],
        side_effects: true,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::Medium,
        derivation: "declares no `sideEffects` at all (a missing field is read conservatively, \
                     FR-021) and ships a CommonJS entry, so nothing about it can be certified \
                     tree-shaken away",
    },
    BadgeExpectation {
        package: "lodash",
        named: &["debounce"],
        side_effects: true,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::Medium,
        derivation: "the CommonJS twin of lodash-es: no `sideEffects` field, one monolithic CJS \
                     module. Same export, opposite badges — which is the point of keeping both",
    },
    BadgeExpectation {
        package: "react-loading-skeleton",
        named: &["SkeletonTheme"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: ConfidenceLevel::High,
        derivation: "declares `sideEffects: [\"**/*.css\"]` — and the entry being measured is \
                     `dist/index.js` (the `import` condition of its `exports`), which matches no \
                     pattern in it. Side-Effectful is a property of THE IMPORT: the one file in \
                     this package the rule does describe is `dist/skeleton.css`, and the published \
                     JavaScript never imports it. So the import is not side-effectful, the \
                     full-package comparison runs, and `SkeletonTheme` really is a slice of the \
                     surface (esbuild puts it at 83% of the whole, well under the 95% bar) — \
                     nothing is unmeasured, so confidence is High. This row read \
                     `true`/`false`/Medium until Task 4, and every one of those three came from \
                     `pipeline::analyze` ORing `is_array()` into the answer: the array form was \
                     reported side-effectful WHATEVER it matched, which gated the comparison build \
                     off and made `truly_treeshakeable: false` true by construction. It is the \
                     only real-package detector of that rule, and it moved when the rule did",
    },
    BadgeExpectation {
        package: "@uiw/react-md-editor",
        named: &["headingExecute"],
        side_effects: true,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::Medium,
        derivation: "declares no `sideEffects`, so it reports conservatively — and its ESM entry \
                     does `import \"./index.css\"`, which is why it is in the set at all (see \
                     `a_css_shipping_real_package_counts_its_stylesheet_into_its_size`). It stays \
                     Medium because it declares side effects, NOT because of its stylesheet: those \
                     bytes are counted now (B2), so the asset disclosure no longer holds anything \
                     below High",
    },
    BadgeExpectation {
        package: "refractor",
        named: &["refractor"],
        side_effects: true,
        truly_treeshakeable: false,
        confidence: ConfidenceLevel::Medium,
        derivation: "declares `sideEffects: [\"lib/all.js\",\"lib/common.js\"]` and its `exports` \
                     entry IS `./lib/common.js` — so the entry being measured is one the package \
                     names effectful, and the badge is `true`. The badge was ALWAYS true; what was \
                     wrong was the size behind it. The two patterns are the only ones in the whole \
                     fixture set that carry a `/`, which the matcher anchors at the package root \
                     instead of prefixing with `**/` — and the daemon was handing Rolldown a \
                     `\\\\?\\` verbatim entry id against a non-canonical package.json path, so the \
                     relativization degraded to an absolute path, both patterns missed, and \
                     Rolldown tree-shook ~35 gated `refractor.register(lang)` calls out of a \
                     package it had just been told was effectful: 30,229 B reported, 113,152 B \
                     real. This row cannot detect that (a badge is not a byte) — \
                     `scripts/accuracy-compare.mjs` measures it against esbuild, and \
                     `every_side_effects_form_answers_with_what_rolldown_retained` pins the \
                     declaration form. It is here because every pinned fixture must have a row",
    },
];

fn fixture_manifest_dependencies() -> BTreeSet<String> {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scripts")
        .join("accuracy-fixtures")
        .join("package.json");
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("the fixture manifest should be readable"),
    )
    .expect("the fixture manifest should be valid JSON");

    manifest
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .expect("the fixture manifest should declare dependencies")
        .keys()
        .cloned()
        .collect()
}

fn describe(result: &ImportResult) -> String {
    format!(
        "side_effects={} truly_treeshakeable={} confidence={:?} sizes={:?} stage={:?} \
         diagnostics={:?}",
        result.side_effects,
        result.truly_treeshakeable,
        result.confidence,
        result.sizes(),
        result.unmeasured_stage(),
        result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.stage.as_str())
            .collect::<Vec<_>>(),
    )
}

/// The baseline. Every pinned package, through the pipeline, badges compared with the row above.
///
/// Mismatches are collected rather than short-circuited: when a rule moves (Task 4 moves the array
/// `sideEffects` rule, and it moves several rows at once) the useful output is *every* package it
/// moved, not the first one alphabetically.
#[test]
#[ignore = "requires installed fixtures (scripts/prepare-candidate-fixtures.mjs); qualification-only"]
fn real_package_badges_hold() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut failures = Vec::new();

    for expected in EXPECTATIONS {
        let result = common::pipeline_fixtures::analyze_named_import(
            &workspace,
            expected.package,
            expected.named,
            ImportRuntime::Component,
        );

        eprintln!(
            "{}/{}: {}",
            expected.package,
            expected.named.join(","),
            describe(&result)
        );

        // A badge on an Unmeasured result is not a badge, it is the conservative default every
        // failure carries (ADR-0006). Asserting it would pass for the wrong reason forever.
        if result.sizes().is_none() {
            failures.push(format!(
                "{}: UNMEASURED (stage {:?}, error {:?}) — no build, so its badges mean nothing",
                expected.package,
                result.unmeasured_stage(),
                result.error,
            ));
            continue;
        }

        if result.side_effects != expected.side_effects
            || result.truly_treeshakeable != expected.truly_treeshakeable
            || result.confidence != expected.confidence
        {
            failures.push(format!(
                "{}: expected side_effects={} truly_treeshakeable={} confidence={:?} \
                 (derived from: {})\n     got {}\n     reasons: {:?}",
                expected.package,
                expected.side_effects,
                expected.truly_treeshakeable,
                expected.confidence,
                expected.derivation,
                describe(&result),
                result.confidence_reasons,
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "real-package badges moved ({} of {} rows):\n  - {}",
        failures.len(),
        EXPECTATIONS.len(),
        failures.join("\n  - "),
    );
}

/// The §10.3 real-package set was **entirely pure JavaScript**, which is exactly why the CSS defect
/// survived qualification: Rolldown refuses a stylesheet at the LINK stage (`UNSUPPORTED_FEATURE:
/// Bundling CSS is no longer supported`), so every CSS-importing package was unmeasurable, and the
/// deleted size-fabricator hid it behind a plausible number.
///
/// `@uiw/react-md-editor` is the set's first package whose published ESM entry really does
/// `import "./index.css"`. Its stylesheet's bytes are now COUNTED into the Import Cost (B2), so it
/// must produce a size that includes them and report what the CSS contributed — never an Unmeasured
/// row, and no longer a disclosure of bytes the number left out.
///
/// (Checked before pinning it: react-toastify, react-datepicker, swiper and react-loading-skeleton
/// all *ship* a stylesheet but none of them **imports** one from published JavaScript, so none of
/// them would exercise this path at all.)
#[test]
#[ignore = "requires installed fixtures (scripts/prepare-candidate-fixtures.mjs); qualification-only"]
fn a_css_shipping_real_package_counts_its_stylesheet_into_its_size() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let result = common::pipeline_fixtures::analyze_named_import(
        &workspace,
        "@uiw/react-md-editor",
        &["headingExecute"],
        ImportRuntime::Component,
    );

    let sizes = common::measured_sizes(&result);
    assert!(sizes.brotli_bytes > 0, "{}", describe(&result));

    let css = result
        .asset_breakdown
        .iter()
        .find(|contribution| contribution.kind == AssetKind::Css)
        .unwrap_or_else(|| {
            panic!(
                "a real package whose entry imports a stylesheet must count it and say what it \
                 contributed: {}",
                describe(&result)
            )
        });
    assert!(
        css.brotli_bytes > 0 && css.minified_bytes > 0,
        "the stylesheet must contribute real bytes to the size: {css:?}",
    );
    // The disclosure is the FALLBACK now. Seeing it here would mean Lightning CSS could not process
    // a real published stylesheet, which is the one thing this fixture exists to catch.
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"),
        "a real stylesheet must process, not fall back to disclosure: {}",
        describe(&result)
    );
}

/// Anti-vacuity Guard, mirroring `candidate_packages::dangling_binding_gate_is_not_vacuous`: prove
/// the baseline can FIRE. A table whose rows all expect the same value cannot fail a build that
/// hardwires that value — it would stay green while the badge it exists to protect was a constant.
/// So the table must partition the set on every flag it claims to check. This row needs no
/// fixtures, so the guard holds even in a run where the real packages are absent.
#[test]
fn the_badge_baseline_can_discriminate() {
    let distinct = |values: BTreeSet<bool>| values.len() == 2;

    assert!(
        distinct(
            EXPECTATIONS
                .iter()
                .map(|expected| expected.side_effects)
                .collect()
        ),
        "no row expects side_effects to differ; a pipeline that hardwired it would stay green",
    );
    assert!(
        distinct(
            EXPECTATIONS
                .iter()
                .map(|expected| expected.truly_treeshakeable)
                .collect()
        ),
        "no row expects truly_treeshakeable to differ; a pipeline that hardwired it would stay \
         green — which is the exact failure this baseline exists to catch",
    );
    let confidences: Vec<ConfidenceLevel> = EXPECTATIONS
        .iter()
        .map(|expected| expected.confidence)
        .collect();
    let distinct_confidences = confidences
        .iter()
        .enumerate()
        .filter(|(index, level)| !confidences[..*index].contains(level))
        .count();
    assert!(
        distinct_confidences >= 2,
        "every row expects the same confidence; the level would be unguarded",
    );

    // The pipeline cannot certify an effectful package as tree-shaken away
    // (`pipeline::analyze` gates the full-package comparison on `!side_effects`), so a row
    // claiming both is asserting something the code cannot produce and would only ever fail.
    for expected in EXPECTATIONS {
        assert!(
            !(expected.side_effects && expected.truly_treeshakeable),
            "{}: a side-effectful package can never be truly tree-shakeable",
            expected.package,
        );
    }
}

/// A new fixture with no row is a gap — and a silent one, because the badge test would simply not
/// look at it. The fixture manifest is the independently-maintained source of the set; this holds
/// the table against it in both directions.
#[test]
fn every_pinned_fixture_has_a_badge_row() {
    let pinned = fixture_manifest_dependencies();
    let covered: BTreeSet<String> = EXPECTATIONS
        .iter()
        .map(|expected| expected.package.to_owned())
        .collect();

    let unbaselined: Vec<&String> = pinned.difference(&covered).collect();
    let stale: Vec<&String> = covered.difference(&pinned).collect();

    assert!(
        unbaselined.is_empty(),
        "pinned fixtures with no badge row (add one to EXPECTATIONS, derived from what the \
         package declares): {unbaselined:?}",
    );
    assert!(
        stale.is_empty(),
        "badge rows for packages that are no longer pinned: {stale:?}",
    );
}
