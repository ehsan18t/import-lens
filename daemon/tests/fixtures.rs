use std::path::PathBuf;

mod common;

#[test]
fn fixture_zip_entry_paths_normalize_windows_separators() {
    let (path, is_dir) =
        common::normalized_zip_entry_path(r"uuid@13.0.0\node_modules\uuid\package.json")
            .expect("zip entry should normalize");

    assert_eq!(
        path,
        PathBuf::from("uuid@13.0.0")
            .join("node_modules")
            .join("uuid")
            .join("package.json"),
    );
    assert!(!is_dir);
}
