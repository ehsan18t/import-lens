use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);
#[allow(dead_code)]
static FIXTURE_PACKAGES_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn temp_workspace(prefix: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let id = NEXT_TEMP_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();
    let path = std::env::temp_dir().join(format!("{prefix}-{process_id}-{suffix}-{id}"));
    fs::create_dir_all(&path).expect("temp workspace should be created");
    path
}

#[allow(dead_code)]
pub fn fixture_workspace(name: &str) -> PathBuf {
    fixture_packages_dir().join(name)
}

#[allow(dead_code)]
fn fixture_packages_dir() -> &'static PathBuf {
    FIXTURE_PACKAGES_DIR.get_or_init(|| {
        let target = temp_workspace("import-lens-fixtures");
        let archive = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("packages.zip");
        let file = fs::File::open(&archive).expect("fixture archive should be readable");
        let mut zip = zip::ZipArchive::new(file).expect("fixture archive should be a valid zip");
        extract_fixture_archive(&mut zip, &target);
        target
    })
}

fn extract_fixture_archive(zip: &mut zip::ZipArchive<fs::File>, target: &Path) {
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .expect("fixture archive entry should be readable");
        let (relative_path, is_dir) = normalized_zip_entry_path(entry.name())
            .expect("fixture archive entry path should be safe");
        let path = target.join(relative_path);

        if is_dir {
            fs::create_dir_all(&path).expect("fixture archive directory should be created");
            continue;
        }

        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture archive parent directory should be created");
        let mut file = fs::File::create(&path).expect("fixture archive file should be created");
        io::copy(&mut entry, &mut file).expect("fixture archive file should be written");
    }
}

/// Fixture plumbing shared by the qualification suites
/// (candidate_packages.rs, candidate_performance.rs): both resolve real
/// packages out of the workspace prepared by
/// scripts/prepare-candidate-fixtures.mjs. Each integration-test crate
/// compiles this module independently, so crates that skip the suites see
/// its items as dead code.
#[allow(dead_code)]
pub mod engine_fixtures {
    use import_lens_daemon::engine::{BundleEntry, BundleSelection};
    use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
    use import_lens_daemon::pipeline::resolver::resolve_package_entry;
    use std::fs;
    use std::path::{Path, PathBuf};

    pub const SETUP_HINT: &str = "candidate package qualification needs installed fixtures: run \
         `node scripts/prepare-candidate-fixtures.mjs` and set \
         IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints";

    pub fn fixtures_workspace() -> PathBuf {
        let Some(dir) = std::env::var_os("IMPORT_LENS_FIXTURES_WORKSPACE") else {
            panic!("{SETUP_HINT}");
        };
        let dir = PathBuf::from(dir);
        assert!(
            dir.join("node_modules").is_dir(),
            "{SETUP_HINT} (node_modules missing under {})",
            dir.display()
        );
        dir
    }

    pub fn resolve_fixture_entry(
        workspace: &Path,
        package: &str,
        version: &str,
        export: &str,
    ) -> BundleEntry {
        // The synthetic document anchors node_modules resolution at the
        // fixtures workspace, exactly like a user file in a real project.
        let document = workspace.join("entry.js");
        if !document.is_file() {
            fs::write(&document, "").expect("fixture anchor document should be writable");
        }
        let resolved = resolve_package_entry(
            &document,
            &ImportRequest {
                specifier: package.to_owned(),
                package_name: package.to_owned(),
                version: version.to_owned(),
                named: vec![export.to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::default(),
            },
        )
        .unwrap_or_else(|error| panic!("{package} should resolve: {error}"));

        BundleEntry {
            entry_path: resolved.entry_path,
            package_root: resolved.package_root,
            selection: BundleSelection::Named(vec![export.to_owned()]),
            reported_side_effects: resolved.side_effects,
        }
    }
}

/// The same installed fixtures, entered through the **pipeline** instead of the engine.
///
/// `engine_fixtures` stops at `BundleArtifact`, which has no badges on it: `side_effects`,
/// `truly_treeshakeable` and `confidence` are decided in `pipeline::analyze`, so a harness that
/// wants to assert them has to produce an `ImportResult` (candidate_badges.rs).
#[allow(dead_code)]
pub mod pipeline_fixtures {
    use import_lens_daemon::ipc::protocol::{
        ImportKind, ImportRequest, ImportResult, ImportRuntime,
    };
    use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
    use std::fs;
    use std::path::Path;

    /// The version the workspace actually installed, read from the package's own manifest.
    ///
    /// Not a constant in the test: a pinned version is a fact about
    /// `scripts/accuracy-fixtures/package.json`, and repeating it in a test would only add a second
    /// place to forget.
    pub fn installed_version(workspace: &Path, package: &str) -> String {
        let mut manifest_path = workspace.join("node_modules");
        for segment in package.split('/') {
            manifest_path.push(segment);
        }
        manifest_path.push("package.json");
        let manifest = fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
            panic!(
                "{} should be installed ({}): {error}",
                package,
                manifest_path.display()
            )
        });
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest).expect("fixture manifest should be valid JSON");
        manifest
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{package} manifest should declare a string version"))
            .to_owned()
    }

    /// One named import of a real installed package, all the way through `analyze_import` — the
    /// only path on which the badges exist.
    pub fn analyze_named_import(
        workspace: &Path,
        package: &str,
        named: &[&str],
        runtime: ImportRuntime,
    ) -> ImportResult {
        // The synthetic document anchors node_modules resolution at the fixtures workspace,
        // exactly like a user file in a real project (mirrors `engine_fixtures`).
        let active_document_path = workspace.join("src").join("app.ts");
        let context = AnalysisContext {
            workspace_root: workspace.to_path_buf(),
            active_document_path,
        };
        let request = ImportRequest {
            specifier: package.to_owned(),
            package_name: package.to_owned(),
            version: installed_version(workspace, package),
            named: named.iter().map(|name| (*name).to_owned()).collect(),
            import_kind: ImportKind::Named,
            runtime,
        };

        analyze_import(&context, &request)
    }
}

// OXC validation helpers shared by the candidate qualification suites
// (candidate_matrix.rs, candidate_packages.rs). Copied out of
// daemon/tests/bundle.rs on purpose: that file is deleted at cutover and
// the qualification suites must keep compiling.

#[allow(dead_code)]
pub fn assert_parseable(source: &str) {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();

    assert!(
        !parsed.panicked && !parsed.diagnostics.has_errors(),
        "generated source should parse cleanly: {source}"
    );
}

#[allow(dead_code)]
pub fn assert_semantic_valid(source: &str) {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();

    assert!(
        !parsed.panicked && !parsed.diagnostics.has_errors(),
        "generated source should parse cleanly: {source}"
    );

    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    assert!(
        !semantic.diagnostics.has_errors(),
        "generated source should pass semantic checks: {source}\nerrors: {:?}",
        semantic.diagnostics.errors().collect::<Vec<_>>()
    );
}

/// Every `__il_`-prefixed identifier the chunk reads must be declared inside
/// it. An unresolved one means a requested surface was pruned while still
/// referenced — the defect class the bundler redesign exists to eliminate.
#[allow(dead_code)]
pub fn assert_no_dangling_il_bindings(source: &str) {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    let semantic = SemanticBuilder::new().build(&parsed.program);

    let mut dangling = semantic
        .semantic
        .scoping()
        .root_unresolved_references()
        .iter()
        .map(|(name, _)| name.to_string())
        .filter(|name| name.starts_with("__il_"))
        .collect::<Vec<_>>();
    dangling.sort();
    dangling.dedup();

    assert!(
        dangling.is_empty(),
        "bundle references undeclared bindings {dangling:?}:\n{source}"
    );
}

pub(crate) fn normalized_zip_entry_path(name: &str) -> Option<(PathBuf, bool)> {
    if name.contains('\0') {
        return None;
    }

    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }

    let is_dir = normalized.ends_with('/');
    let mut path = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(':') || Path::new(segment).is_absolute() {
            return None;
        }
        path.push(segment);
    }

    if path.as_os_str().is_empty() {
        return None;
    }

    Some((path, is_dir))
}

/// The five sizes of a result the test expects to have been MEASURED.
///
/// Panics with the result's own failure stage when there is none, which is the whole point: a test
/// that reaches for a size it did not get says so instead of comparing against a sentinel zero
/// (ADR-0006).
#[allow(dead_code)]
pub fn measured_sizes(
    result: &import_lens_daemon::ipc::protocol::ImportResult,
) -> import_lens_daemon::ipc::protocol::MeasuredSizes {
    result.sizes().unwrap_or_else(|| {
        panic!(
            "expected `{}` to be measured, but it is unmeasured (stage: {:?}, error: {:?})",
            result.specifier,
            result.unmeasured_stage(),
            result.error
        )
    })
}
