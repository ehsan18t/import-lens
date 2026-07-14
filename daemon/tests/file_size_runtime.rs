//! Combined file sizing must size each import under its OWN runtime.
//!
//! `compute_file_size` hands every entry to one `BundleRequest`, which carries a
//! single runtime. Root entries are pre-resolved per request, so their own paths
//! are right either way — but Rolldown resolves the whole TRANSITIVE graph under
//! that one runtime, and Server and Client resolve dependencies under materially
//! different conditions. Applying the first resolved import's runtime to every
//! entry therefore resolves the other runtime's dependencies against the wrong
//! conditions, and the mis-conditioned build still SUCCEEDS — so no fallback fires
//! and no diagnostic is raised. The user just gets a wrong number.
//!
//! A single Astro file reaches this: frontmatter imports are Server, processed
//! `<script>` imports are Client (`document/script_regions.rs`).
//!
//! Design doc §6.3 (I15, and I20 for the compression rule); [ADR-0005].

use import_lens_daemon::ipc::protocol::{
    FileSizeDocumentRequest, FileSizeDocumentResponse, ImportKind, ImportRequest, ImportResult,
    ImportRuntime, MeasuredSizes, ModuleContribution, PROTOCOL_VERSION,
};
use import_lens_daemon::pipeline::analyze::AnalysisContext;
use import_lens_daemon::pipeline::file_size::{
    FileSizeComputation, SizedImport, annotate_shared_bytes, compute_file_size,
};
use import_lens_daemon::service::ImportLensService;
use std::{fs, path::Path, path::PathBuf};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-file-size-runtime")
}

fn context(workspace: &Path) -> AnalysisContext {
    AnalysisContext {
        workspace_root: workspace.to_path_buf(),
        active_document_path: workspace.join("src").join("page.astro"),
    }
}

fn request(package: &str, export: &str, runtime: ImportRuntime) -> SizedImport {
    SizedImport::installed(
        ImportRequest {
            specifier: package.to_owned(),
            package_name: package.to_owned(),
            version: "1.0.0".to_owned(),
            named: vec![export.to_owned()],
            import_kind: ImportKind::Named,
            runtime,
        },
        // Nothing measured yet: these sizings exercise the combined build, which is what the
        // file's totals come from — the per-import measurements only feed the fallback.
        None,
    )
}

fn size(context: &AnalysisContext, requests: &[SizedImport]) -> FileSizeComputation {
    let computed = compute_file_size(context, requests);
    assert_eq!(
        computed.error, None,
        "file sizing should succeed: {:?}",
        computed.diagnostics
    );
    computed
}

/// `host` has one runtime-independent entry that re-exports `cond`, whose own
/// `exports` map is runtime-conditional: a single const under `browser`, a large
/// module under `node`. The conditional resolution therefore happens where
/// Rolldown owns it — transitively — which is the only place the defect lives.
///
/// `plain` shares no modules with either, so it can be sized under a different
/// runtime without any shared-module interaction confusing the comparison.
fn write_packages(workspace: &Path) {
    let host = workspace.join("node_modules").join("host");
    fs::create_dir_all(&host).expect("host package root");
    fs::write(
        host.join("package.json"),
        r#"{"name":"host","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}"#,
    )
    .expect("host manifest");
    fs::write(host.join("index.js"), "export { value } from \"cond\";\n").expect("host entry");

    let cond = workspace.join("node_modules").join("cond");
    fs::create_dir_all(&cond).expect("cond package root");
    fs::write(
        cond.join("package.json"),
        r#"{
  "name": "cond",
  "version": "1.0.0",
  "type": "module",
  "sideEffects": false,
  "main": "./server.js",
  "browser": "./browser.js",
  "exports": {
    ".": {
      "browser": "./browser.js",
      "node": "./server.js",
      "default": "./server.js"
    }
  }
}"#,
    )
    .expect("cond manifest");
    fs::write(cond.join("browser.js"), "export const value = 1;\n").expect("cond browser entry");
    fs::write(
        cond.join("server.js"),
        "export { value } from \"./heavy.js\";\n",
    )
    .expect("cond server entry");

    let mut heavy = String::from("export const value = [\n");
    for index in 0..600 {
        heavy.push_str(&format!("  \"heavy padding entry number {index:04}\",\n"));
    }
    heavy.push_str("];\n");
    fs::write(cond.join("heavy.js"), heavy).expect("cond heavy module");

    let plain = workspace.join("node_modules").join("plain");
    fs::create_dir_all(&plain).expect("plain package root");
    fs::write(
        plain.join("package.json"),
        r#"{"name":"plain","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}"#,
    )
    .expect("plain manifest");
    fs::write(plain.join("index.js"), "export const thing = 2;\n").expect("plain entry");
}

#[test]
fn combined_file_size_does_not_depend_on_import_order() {
    let workspace = temp_workspace();
    write_packages(&workspace);
    let context = context(&workspace);

    let host_client = size(&context, &[request("host", "value", ImportRuntime::Client)]);
    let host_server = size(&context, &[request("host", "value", ImportRuntime::Server)]);

    // If the two runtimes do not resolve `cond` differently, this test proves
    // nothing. Fail loudly rather than pass vacuously.
    assert!(
        host_server.raw_bytes > host_client.raw_bytes * 2,
        "fixture is broken: `host` must resolve differently per runtime \
         (server={} client={})",
        host_server.raw_bytes,
        host_client.raw_bytes,
    );

    // The same two imports, both orders. The entry count and the virtual-entry
    // facade are identical between them, so the ONLY difference is which import
    // resolves first — and therefore which runtime the single build would apply to
    // every entry. Order-invariance isolates the defect exactly.
    let client_first = size(
        &context,
        &[
            request("host", "value", ImportRuntime::Client),
            request("plain", "thing", ImportRuntime::Server),
        ],
    );
    let server_first = size(
        &context,
        &[
            request("plain", "thing", ImportRuntime::Server),
            request("host", "value", ImportRuntime::Client),
        ],
    );

    assert_eq!(
        client_first.raw_bytes, server_first.raw_bytes,
        "combined file size must not depend on import order: `host` is a Client import \
         and must be sized under Client conditions no matter which import resolved \
         first. client-first={}, server-first={} (design doc §6.3, I15).",
        client_first.raw_bytes, server_first.raw_bytes,
    );

    // ...and the order-invariant answer must be the CLIENT-conditioned one. Guards
    // against a "fix" that makes both orders agree by sizing everything as Server.
    assert!(
        server_first.raw_bytes < host_server.raw_bytes,
        "`host` is a Client import and must not be sized under Server conditions: \
         aggregate={} is at or above the Server-only size {}",
        server_first.raw_bytes,
        host_server.raw_bytes,
    );
}

/// A package imported under two different runtimes in one file is a real shape
/// (an Astro island imported in frontmatter and in a `<script>`). Each runtime
/// genuinely ships its own copy, so it is counted once per runtime.
#[test]
fn a_package_imported_under_two_runtimes_is_counted_once_per_runtime() {
    let workspace = temp_workspace();
    write_packages(&workspace);
    let context = context(&workspace);

    let client_only = size(
        &context,
        &[request("plain", "thing", ImportRuntime::Client)],
    );
    let both = size(
        &context,
        &[
            request("plain", "thing", ImportRuntime::Client),
            request("plain", "thing", ImportRuntime::Server),
        ],
    );

    assert!(
        both.raw_bytes > client_only.raw_bytes,
        "the same package imported under two runtimes ships two copies and must be \
         counted twice: both={} client_only={}",
        both.raw_bytes,
        client_only.raw_bytes,
    );
}

// ---------------------------------------------------------------------------------------------
// A runtime is an ARTIFACT boundary (ADR-0005). Two runtime groups are two things that ship, each
// carrying its own copy of anything both need — so their costs are added, and nothing is ever
// deduplicated across the boundary.
// ---------------------------------------------------------------------------------------------

/// The shared-heavy two-runtime document the fix exists for: `shared-core` is imported from both
/// the Astro frontmatter (Server) and a client `<script>` (Client), and each runtime additionally
/// pulls one package of its own.
fn write_mixed_runtime_packages(workspace: &Path) {
    let package = |name: &str, source: &str| {
        let root = workspace.join("node_modules").join(name);
        fs::create_dir_all(&root).expect("package root");
        fs::write(
            root.join("package.json"),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}}"#
            ),
        )
        .expect("manifest");
        fs::write(root.join("index.js"), source).expect("entry");
    };

    // Deliberately redundant, the way a real library is: repeated identifier and string shapes
    // that a compressor eats cheaply the SECOND time it sees them — which is exactly the
    // redundancy the concatenation was compressing away between two payloads that never meet.
    let mut shared = String::from("export const shared = {\n");
    for index in 0..400 {
        shared.push_str(&format!(
            "  entry{index:04}: {{ id: \"shared-core-entry-{index:04}\", label: \"Shared core entry number {index:04}\", enabled: true }},\n"
        ));
    }
    shared.push_str("};\n");
    package("shared-core", &shared);

    package(
        "server-extra",
        "export const serverExtra = \"SERVER_ONLY_PAYLOAD\";\n",
    );
    package(
        "client-extra",
        "export const clientExtra = \"CLIENT_ONLY_PAYLOAD\";\n",
    );
}

/// **The lower bound that was presented as a size.** Combined sizing builds one bundle per runtime
/// — correctly — and then JOINED their minified outputs and compressed the concatenation **once**.
/// Redundancy between the Server and the Client payload was therefore compressed away exactly once,
/// while the two bundles that really ship each pay for it.
///
/// The design accepted this, arguing that summing separately-compressed groups "would be a
/// different lie (compression is not additive)". Non-additivity is real, but it applies to parts
/// that would in reality be compressed **together**. Two runtime groups never are: they are two
/// artifacts that genuinely ship, each genuinely compressed on its own. Summing them models reality
/// exactly; it is the concatenation that distorts it (ADR-0005).
///
/// So the file's compressed total must be **exactly** the sum of what each runtime group compresses
/// to on its own. Equality is the assertion, not an inequality: the concatenation is strictly below
/// the sum for any document whose two runtimes share anything, and a "fix" that merely made the
/// number bigger would still not be the number that ships.
#[test]
fn mixed_runtime_compression_sums_the_groups_not_their_concatenation() {
    let workspace = temp_workspace();
    write_mixed_runtime_packages(&workspace);
    let context = context(&workspace);

    let server = [
        request("shared-core", "shared", ImportRuntime::Server),
        request("server-extra", "serverExtra", ImportRuntime::Server),
    ];
    let client = [
        request("shared-core", "shared", ImportRuntime::Client),
        request("client-extra", "clientExtra", ImportRuntime::Client),
    ];

    let server_only = size(&context, &server);
    let client_only = size(&context, &client);
    let mixed = size(
        &context,
        &server
            .iter()
            .chain(client.iter())
            .cloned()
            .collect::<Vec<_>>(),
    );

    // If the two payloads share nothing, the concatenation and the sum agree and this test proves
    // nothing. Fail loudly rather than pass vacuously.
    assert!(
        server_only.brotli_bytes > 0 && client_only.brotli_bytes > 0,
        "fixture is broken: each runtime group must compress to real bytes (server={} client={})",
        server_only.brotli_bytes,
        client_only.brotli_bytes,
    );

    let sum = server_only.brotli_bytes + client_only.brotli_bytes;
    assert_eq!(
        mixed.brotli_bytes,
        sum,
        "a mixed-runtime file ships TWO artifacts, so its compressed cost is the SUM of the two \
         separately-compressed runtime bundles ({} + {} = {}). Compressing their concatenation \
         once reports {} — a lower bound, under-reporting by {:.1}%, presented as a size and \
         (from Task 9) gating the per-file budget. ADR-0005.",
        server_only.brotli_bytes,
        client_only.brotli_bytes,
        sum,
        mixed.brotli_bytes,
        100.0 - (mixed.brotli_bytes as f64 / sum as f64) * 100.0,
    );

    // Same rule for the other two compressors, and for the minified total: the join also inserted a
    // separator byte per extra group, so `minified_bytes` described a string that ships nowhere.
    assert_eq!(
        mixed.gzip_bytes,
        server_only.gzip_bytes + client_only.gzip_bytes
    );
    assert_eq!(
        mixed.zstd_bytes,
        server_only.zstd_bytes + client_only.zstd_bytes
    );
    assert_eq!(
        mixed.minified_bytes,
        server_only.minified_bytes + client_only.minified_bytes
    );

    assert!(
        mixed.is_cacheable(),
        "nothing failed here: two clean per-runtime builds are a real File Cost: {:?}",
        mixed.diagnostics
    );
}

/// A file-size request for one document of the workspace, forced fresh so nothing is served from a
/// previous request's cache.
fn document_size(
    service: &ImportLensService,
    workspace: &Path,
    document: &str,
    request_id: u64,
    source: &str,
) -> FileSizeDocumentResponse {
    let response = service.handle_file_size_document(FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join(document)
            .to_string_lossy()
            .to_string(),
        source: source.to_owned(),
        force_fresh: true,
        analysis_generation: None,
    });

    assert_eq!(
        response.error, None,
        "{document} should size cleanly: {response:?}"
    );
    assert!(
        !response.degraded && !response.incomplete,
        "{document}: every package here is real and installed, so its combined build succeeds and \
         every contributor is measured: {response:?}"
    );

    response
}

/// The Astro frontmatter is Server; a processed `<script>` is Client (`document/script_regions.rs`).
const MIXED_RUNTIME_DOCUMENT: &str = "---\n\
     import { shared } from 'shared-core';\n\
     import { serverExtra } from 'server-extra';\n\
     ---\n\
     <p>{serverExtra}</p>\n\
     <script>\n\
     import { shared } from 'shared-core';\n\
     import { clientExtra } from 'client-extra';\n\
     console.log(shared, clientExtra);\n\
     </script>\n";

/// The Server half of that document on its own: frontmatter only.
const SERVER_ONLY_DOCUMENT: &str = "---\n\
     import { shared } from 'shared-core';\n\
     import { serverExtra } from 'server-extra';\n\
     ---\n\
     <p>{serverExtra}</p>\n";

/// The Client half on its own: a processed `<script>` and no frontmatter.
const CLIENT_ONLY_DOCUMENT: &str = "<script>\n\
     import { shared } from 'shared-core';\n\
     import { clientExtra } from 'client-extra';\n\
     console.log(shared, clientExtra);\n\
     </script>\n";

/// **The single line the whole per-runtime number hangs on, and nothing pinned it.**
///
/// A document's runtime is decided once, in `document::script_regions`, and lands on
/// `DetectedImport.runtime`. The BUILD partition does not read it there: `file_size.rs` groups on
/// `ImportRequest.runtime`, and the only thing that puts it there is one line of
/// `service::import_request_for_detected` — `runtime: detected.runtime`. Replace it with a constant
/// `ImportRuntime::Component` — the shape of any careless refactor of that struct literal — and the
/// **entire daemon suite stays green** (175 lib, 55 service, 4 here) while an Astro file's Server
/// and Client imports collapse back into ONE build group and the measured 49% under-report ADR-0005
/// exists to abolish silently returns.
///
/// Every other test in this file, and every one in `file_size.rs`, hands `compute_file_size` a
/// `SizedImport` list it built BY HAND — with the runtime it wanted already on it. So not one of
/// them can see the derivation at all. This one starts from an `.astro` **source string** and goes
/// through `handle_file_size_document`, which is where the `ImportRequest`s are derived from the
/// `DetectedImport`s, and asserts the file is sized as the TWO artifacts it ships: its totals are
/// the sum of the two single-runtime documents, each compressed on its own.
#[test]
fn a_mixed_runtime_astro_document_is_built_as_two_artifacts() {
    let workspace = temp_workspace();
    write_mixed_runtime_packages(&workspace);
    let service = ImportLensService::new(None, false);

    let mixed = document_size(
        &service,
        &workspace,
        "page.astro",
        901,
        MIXED_RUNTIME_DOCUMENT,
    );
    let server_only = document_size(
        &service,
        &workspace,
        "server.astro",
        902,
        SERVER_ONLY_DOCUMENT,
    );
    let client_only = document_size(
        &service,
        &workspace,
        "client.astro",
        903,
        CLIENT_ONLY_DOCUMENT,
    );

    let runtimes = mixed
        .states
        .iter()
        .filter_map(|state| state.request.as_ref().map(|request| request.runtime))
        .collect::<Vec<_>>();
    assert_eq!(
        runtimes.len(),
        4,
        "test setup: all four imports of the mixed document resolve to installed packages and get \
         a request: {:?}",
        mixed.states
    );

    // If the two halves do not really share `shared-core`, the sum and the merged build agree and
    // this test proves nothing. Fail loudly rather than pass vacuously: the number a SINGLE build
    // group produces — which is exactly what a lost runtime collapses to — must be strictly below
    // the sum, because the merged bundle links `shared-core` once for payloads that each ship it.
    let collapsed = compute_file_size(
        &context(&workspace),
        &[
            request("shared-core", "shared", ImportRuntime::Component),
            request("server-extra", "serverExtra", ImportRuntime::Component),
            request("shared-core", "shared", ImportRuntime::Component),
            request("client-extra", "clientExtra", ImportRuntime::Component),
        ],
    );
    let sum = server_only.brotli_bytes + client_only.brotli_bytes;
    assert!(
        collapsed.error.is_none() && collapsed.brotli_bytes > 0 && collapsed.brotli_bytes < sum,
        "fixture is broken: the two runtimes must genuinely share enough that ONE merged build \
         under-reports against the sum ({} vs {}): {:?}",
        collapsed.brotli_bytes,
        sum,
        collapsed.diagnostics
    );

    assert_eq!(
        mixed.brotli_bytes,
        sum,
        "a mixed-runtime Astro file ships TWO artifacts, so its compressed cost is the SUM of the \
         two separately-compressed runtime bundles ({} Server + {} Client = {}). Reporting {} \
         means the document's runtimes never reached the build's grouping and both halves were \
         linked into one bundle — `shared-core` counted once for two payloads that each ship it, \
         an under-report of {:.1}%, presented as a size and gating the per-file budget. \
         The merged build reports {}. ADR-0005.",
        server_only.brotli_bytes,
        client_only.brotli_bytes,
        sum,
        mixed.brotli_bytes,
        100.0 - (mixed.brotli_bytes as f64 / sum as f64) * 100.0,
        collapsed.brotli_bytes,
    );
    assert_eq!(
        mixed.raw_bytes,
        server_only.raw_bytes + client_only.raw_bytes
    );
    assert_eq!(
        mixed.minified_bytes,
        server_only.minified_bytes + client_only.minified_bytes
    );
    assert_eq!(
        mixed.gzip_bytes,
        server_only.gzip_bytes + client_only.gzip_bytes
    );
    assert_eq!(
        mixed.zstd_bytes,
        server_only.zstd_bytes + client_only.zstd_bytes
    );

    // ...and the derivation itself, named, so a failure above says WHICH line to look at: the
    // request the build partition groups by carries the runtime the document detected, and
    // `import_request_for_detected` is the only thing that puts it there.
    assert_eq!(
        runtimes
            .iter()
            .filter(|runtime| **runtime == ImportRuntime::Server)
            .count(),
        2,
        "the two FRONTMATTER imports must reach the build as Server requests: {runtimes:?}"
    );
    assert_eq!(
        runtimes
            .iter()
            .filter(|runtime| **runtime == ImportRuntime::Client)
            .count(),
        2,
        "and the two `<script>` imports as Client requests: {runtimes:?}"
    );
}

fn shared_result(specifier: &str, module_path: &str, bytes: u64) -> ImportResult {
    let mut result = ImportResult::measured(
        specifier,
        MeasuredSizes {
            raw_bytes: bytes,
            minified_bytes: bytes,
            gzip_bytes: bytes,
            brotli_bytes: bytes,
            zstd_bytes: bytes,
        },
    );
    result.module_breakdown = Some(vec![ModuleContribution {
        path: module_path.to_owned(),
        bytes,
    }]);
    result
}

/// **The saving that never happens.** Sharing was counted across every result of a document with no
/// runtime partition, so a module reached from Astro frontmatter (Server) *and* from a client
/// `<script>` (Client) was reported as a shared dependency — and `insights.ts` renders that to the
/// user as a deduplication saving. The per-runtime build model explicitly does not perform it: each
/// runtime ships its own copy (ADR-0005).
///
/// Sharing within a runtime is still real, and must survive — otherwise this fix could be "made to
/// pass" by never reporting anything as shared.
#[test]
fn a_module_used_in_two_runtimes_is_not_shared() {
    let module = "/workspace/node_modules/shared-core/index.js";
    let mut server_one = shared_result("shared-core", module, 300);
    let mut server_two = shared_result("server-extra", module, 300);
    let mut client = shared_result("shared-core", module, 300);

    annotate_shared_bytes(vec![
        (ImportRuntime::Server, &mut server_one),
        (ImportRuntime::Server, &mut server_two),
        (ImportRuntime::Client, &mut client),
    ]);

    assert_eq!(
        server_one.shared_bytes,
        Some(300),
        "two SERVER imports really do share the module: one Server chunk carries it once"
    );
    assert_eq!(server_two.shared_bytes, Some(300));
    assert_eq!(
        client.shared_bytes,
        Some(0),
        "the Client artifact ships its OWN copy of the module — the Server imports save it \
         nothing. Counting it as shared sells the user a deduplication the build model does not \
         perform (ADR-0005), on exactly the file shape the runtime split exists to handle."
    );
}
