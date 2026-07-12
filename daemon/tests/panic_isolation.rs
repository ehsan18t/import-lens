//! The release profile must unwind.
//!
//! The daemon isolates panics with `catch_unwind`: a panicking workspace file is
//! skipped from the report rather than failing the scan, a panicking analysis returns
//! an error response, and the report and registry workers survive. Under
//! `panic = "abort"` none of that works — the panic runtime aborts the process on the
//! spot, `catch_unwind` never runs, and one bad file takes the user's daemon down.
//!
//! The release profile carried `panic = "abort"` through the entire bundler redesign,
//! which meant every one of those guards was dead code in the shipped binary while
//! every test that covers them passed. They passed because **Cargo ignores
//! `panic = "abort"` for the test profile** — the isolation tests are compiled with
//! unwinding no matter what the release profile says, so they can never catch this.
//!
//! That is what makes this guard necessary rather than decorative: it is the only
//! thing in the suite that can fail if someone sets `panic = "abort"` again to shave
//! binary size.

use std::fs;
use std::path::Path;

/// The `[profile.release]` section of the workspace manifest, as raw lines.
fn release_profile() -> Vec<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the daemon crate sits inside the workspace")
        .join("Cargo.toml");
    let contents = fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("workspace manifest should be readable: {error}"));

    contents
        .lines()
        .skip_while(|line| line.trim() != "[profile.release]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .map(|line| line.trim().to_owned())
        .collect()
}

#[test]
fn the_release_profile_does_not_abort_on_panic() {
    let profile = release_profile();
    assert!(
        !profile.is_empty(),
        "[profile.release] should exist in the workspace manifest"
    );

    let panic_setting = profile
        .iter()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix("panic"))
        .map(|value| value.trim_start_matches(['=', ' ']).trim().to_owned());

    assert_ne!(
        panic_setting.as_deref(),
        Some("\"abort\""),
        "release must unwind: the daemon's eight catch_unwind isolation sites are dead \
         code under panic = \"abort\", and no other test can catch this because Cargo \
         ignores the setting for the test profile"
    );
}

/// The guard above is only worth having while the isolation it protects still exists.
/// If every `catch_unwind` were removed, the profile setting would stop mattering and
/// this file should go with it.
#[test]
fn the_daemon_still_relies_on_catch_unwind() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sites = 0;

    let mut pending = vec![source_root];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).expect("daemon source tree should be readable");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = fs::read_to_string(&path).expect("source file should be readable");
                sites += source.matches("catch_unwind").count();
            }
        }
    }

    assert!(
        sites > 0,
        "no catch_unwind left in the daemon — if panic isolation is genuinely gone, \
         delete this file; otherwise something was removed by mistake"
    );
}
