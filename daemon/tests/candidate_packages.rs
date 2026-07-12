//! Real-package qualification for the Rolldown candidate engine (spec
//! §10.3/§10.4) over the pinned accuracy fixtures. Fixture installation is
//! an explicit setup step; these tests perform no network access:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --locked \
//!     --test candidate_packages -- --ignored --nocapture
//! ```

use import_lens_daemon::engine::{
    BundleArtifact, BundlePurpose, BundleRequest, ImportRuntime, RolldownEngine,
};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

mod common;

struct PackageCase {
    package: &'static str,
    version: &'static str,
    export: &'static str,
}

async fn bundle_case(case: &PackageCase) -> BundleArtifact {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let entry = common::engine_fixtures::resolve_fixture_entry(
        &workspace,
        case.package,
        case.version,
        case.export,
    );
    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![entry],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{}/{} should bundle: {failure:?}",
                case.package, case.export
            )
        });

    // §10.4: parses, passes OXC semantic validation, zero dangling
    // `__il_`-prefixed bindings (the css-tree/date-fns defect class).
    common::assert_parseable(&artifact.code);
    common::assert_semantic_valid(&artifact.code);
    common::assert_no_dangling_il_bindings(&artifact.code);

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "{}/{} exported names: {:?}",
        case.package,
        case.export,
        artifact.exported_names
    );

    // §10.4: contributions contain only rendered real modules, and every
    // rendered module is a loaded (fingerprinted) input.
    let loaded: HashSet<&PathBuf> = artifact.loaded_paths.iter().collect();
    assert!(
        !artifact.contributions.is_empty(),
        "{}/{} should render at least one module",
        case.package,
        case.export
    );
    for contribution in &artifact.contributions {
        assert!(contribution.rendered_bytes > 0);
        let canonical =
            fs::canonicalize(&contribution.path).unwrap_or_else(|_| contribution.path.clone());
        assert!(
            loaded.contains(&canonical),
            "{}/{}: rendered module {} missing from loaded_paths",
            case.package,
            case.export,
            contribution.path.display()
        );
    }

    // Determinism gate (§10.6): an identical request is byte-identical.
    let entry = common::engine_fixtures::resolve_fixture_entry(
        &workspace,
        case.package,
        case.version,
        case.export,
    );
    let second = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![entry],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .expect("repeat bundle should succeed");
    assert_eq!(
        artifact.code, second.code,
        "{} code moved between runs",
        case.package
    );
    assert_eq!(artifact.loaded_paths, second.loaded_paths);
    assert_eq!(artifact.exported_names, second.exported_names);
    let contribution_pairs = |bundle: &BundleArtifact| {
        bundle
            .contributions
            .iter()
            .map(|contribution| (contribution.path.clone(), contribution.rendered_bytes))
            .collect::<Vec<_>>()
    };
    assert_eq!(contribution_pairs(&artifact), contribution_pairs(&second));

    eprintln!(
        "{}/{}: raw {} bytes, {} rendered modules, {} loaded paths",
        case.package,
        case.export,
        artifact.code.len(),
        artifact.contributions.len(),
        artifact.loaded_paths.len()
    );
    artifact
}

macro_rules! package_case {
    ($name:ident, $package:literal, $version:literal, $export:literal) => {
        #[tokio::test]
        #[ignore = "requires installed fixtures (scripts/prepare-candidate-fixtures.mjs); qualification-only"]
        async fn $name() {
            bundle_case(&PackageCase {
                package: $package,
                version: $version,
                export: $export,
            })
            .await;
        }
    };
}

// The §2.2 defect fixture: the four dangling css-tree bindings must reach
// zero, which `assert_no_dangling_il_bindings` inside `bundle_case` proves.
package_case!(css_tree_parse, "css-tree", "3.2.1", "parse");
package_case!(lodash_es_debounce, "lodash-es", "4.18.1", "debounce");
// Real-package CJS coverage: link-time interop, not enumeration.
package_case!(lodash_debounce_cjs, "lodash", "4.17.21", "debounce");
package_case!(zod_z, "zod", "4.4.3", "z");
package_case!(react_use_state, "react", "19.2.7", "useState");
package_case!(uuid_v4, "uuid", "14.0.1", "v4");

// date-fns carries the extra §10.4 gate: `loaded_paths` includes modules
// tree-shaking later removed (freshness must survive edits to them).
#[tokio::test]
#[ignore = "requires installed fixtures (scripts/prepare-candidate-fixtures.mjs); qualification-only"]
async fn date_fns_format_loads_tree_shaken_modules() {
    let artifact = bundle_case(&PackageCase {
        package: "date-fns",
        version: "4.1.0",
        export: "format",
    })
    .await;

    let rendered: HashSet<PathBuf> = artifact
        .contributions
        .iter()
        .map(|contribution| {
            fs::canonicalize(&contribution.path).unwrap_or_else(|_| contribution.path.clone())
        })
        .collect();
    let tree_shaken = artifact
        .loaded_paths
        .iter()
        .filter(|path| !rendered.contains(*path))
        .count();
    assert!(
        tree_shaken > 0,
        "date-fns/format should load modules that tree-shaking then removes \
         (loaded {}, rendered {})",
        artifact.loaded_paths.len(),
        rendered.len()
    );
}

/// The css-tree row exists for exactly one assertion — zero dangling `__il_`
/// bindings, the §2.2 defect the redesign was built to remove — and every other
/// row leans on it too. If that assertion could not fire, the whole suite would
/// stay green no matter what the engine emitted. This row needs no fixtures, so
/// the gate stays honest even in a run where the real packages are absent.
#[test]
#[should_panic(expected = "bundle references undeclared bindings")]
fn dangling_binding_gate_is_not_vacuous() {
    common::assert_no_dangling_il_bindings("export const parse = __il_entry_0_export_0;\n");
}

/// Prewarm decides whether to queue a Default job for each dependency. It used to
/// answer that with a full engine build of the whole package graph, per dependency,
/// serially, before any real prewarm work could start; it now parses the entry file
/// alone. The two must agree, or prewarm silently stops warming the import the user
/// is about to type — so this pins the fast path against the slow one it replaced,
/// on real packages, where the interesting cases live (react's default arrives
/// through CJS interop; css-tree and date-fns have no default at all).
#[tokio::test]
#[ignore = "requires installed fixtures (scripts/prepare-candidate-fixtures.mjs); qualification-only"]
async fn the_parse_based_default_probe_agrees_with_engine_enumeration() {
    use import_lens_daemon::engine::EngineBudget;
    use import_lens_daemon::engine::boundary::enumerate_exports_sync;
    use import_lens_daemon::prefetch::entry_exposes_default_export;

    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut disagreements = Vec::new();

    for (package, version) in [
        ("css-tree", "3.2.1"),
        ("zod", "4.4.3"),
        ("date-fns", "4.1.0"),
        ("lodash-es", "4.18.1"),
        ("lodash", "4.17.21"),
        ("react", "19.2.7"),
        ("uuid", "14.0.1"),
    ] {
        let entry =
            common::engine_fixtures::resolve_fixture_entry(&workspace, package, version, "any");
        let engine = enumerate_exports_sync(
            entry.entry_path.clone(),
            ImportRuntime::default(),
            EngineBudget::interactive(),
        )
        .map(|enumeration| enumeration.names.iter().any(|name| name == "default"))
        .unwrap_or(true);
        let parsed = entry_exposes_default_export(&entry.entry_path);
        if engine != parsed {
            disagreements.push(format!("{package}: engine={engine} parse={parsed}"));
        }
    }

    assert!(
        disagreements.is_empty(),
        "the parse probe must agree with engine enumeration: {disagreements:?}"
    );
}
