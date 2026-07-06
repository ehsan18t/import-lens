use import_lens_daemon::cache::key::{cache_key_for_resolved_import, decode_cache_identity};
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::resolver::resolve_package_entry;
use std::{fs, thread, time::Duration};

fn write_pkg(root: &std::path::Path, entry_bytes: &str) {
    fs::create_dir_all(root).expect("pkg root");
    fs::write(
        root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js"}"#,
    )
    .expect("manifest");
    fs::write(root.join("index.js"), entry_bytes).expect("entry");
}

fn request() -> ImportRequest {
    ImportRequest {
        specifier: "v4-lib".to_owned(),
        package_name: "v4-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    }
}

#[test]
fn identical_content_reinstall_reuses_the_same_key() {
    let ws = std::env::temp_dir().join(format!("il-v4-reuse-{}", std::process::id()));
    let pkg = ws.join("node_modules").join("v4-lib");
    let document = ws.join("src").join("index.ts");
    let req = request();

    write_pkg(&pkg, "export const value = 1;");
    let resolved1 = resolve_package_entry(&document, &req).expect("resolve 1");
    let key1 = cache_key_for_resolved_import(&req, &resolved1);
    assert!(key1.starts_with("v4:"), "keys are v4-prefixed");

    // Reinstall with IDENTICAL bytes but a new mtime (npm ci / git checkout).
    thread::sleep(Duration::from_millis(20));
    write_pkg(&pkg, "export const value = 1;");
    let resolved2 = resolve_package_entry(&document, &req).expect("resolve 2");
    let key2 = cache_key_for_resolved_import(&req, &resolved2);

    assert_eq!(
        key1, key2,
        "identical-content reinstall must reuse the key (v4 drops in-key fingerprints)"
    );
    fs::remove_dir_all(ws).ok();
}

#[test]
fn cache_key_identity_carries_no_fingerprints() {
    let ws = std::env::temp_dir().join(format!("il-v4-nofp-{}", std::process::id()));
    let pkg = ws.join("node_modules").join("v4-lib");
    let document = ws.join("src").join("index.ts");
    let req = request();
    write_pkg(&pkg, "export const value = 1;");
    let resolved = resolve_package_entry(&document, &req).expect("resolve");
    let key = cache_key_for_resolved_import(&req, &resolved);

    // The decoded identity no longer exposes fingerprint fields (compile-time), and
    // the key round-trips.
    let identity = decode_cache_identity(&key).expect("decode v4 identity");
    assert_eq!(identity.package_name, "v4-lib");
    fs::remove_dir_all(ws).ok();
}
