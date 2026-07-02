use std::{
    fs,
    path::{Path, PathBuf},
};

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "js", "jsx", "ts", "tsx", "mts", "cts", "svelte", "astro", "vue",
];
const SKIPPED_DIRECTORIES: &[&str] = &["node_modules", "dist", "build", "out", "coverage"];
const MAX_SCAN_DEPTH: usize = 64;

pub fn scan_workspace_sources(workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    scan_directory(workspace_root, 0, &mut files);
    files.sort();
    files
}

fn scan_directory(directory: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }

    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow symlinks, so Windows
        // junctions/symlinked directories are skipped instead of recursed
        // into (they can cycle back to an ancestor). This matches the old
        // TypeScript scanner: VS Code's findFiles does not follow symlinked
        // directories either.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        if file_type.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }
            scan_directory(&path, depth + 1, files);
            continue;
        }

        if is_supported_source(&path) {
            files.push(path);
        }
    }
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

fn is_supported_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("il-scan-{}-{suffix}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("temp workspace should be created");
        path
    }

    #[test]
    fn scan_skips_nested_node_modules_and_includes_deep_sources() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join("src").join("nested").join("node_modules"))
            .expect("nested node_modules");
        fs::create_dir_all(workspace.join("src").join("nested").join("deep")).expect("deep dir");
        fs::write(
            workspace
                .join("src")
                .join("nested")
                .join("node_modules")
                .join("x.ts"),
            "export const x = 1;",
        )
        .expect("nested node_modules source");
        fs::write(
            workspace
                .join("src")
                .join("nested")
                .join("deep")
                .join("app.ts"),
            "export const app = 1;",
        )
        .expect("deep source");

        let files = scan_workspace_sources(&workspace);

        fs::remove_dir_all(&workspace).expect("workspace cleanup");
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with(Path::new("src/nested/deep/app.ts")));
    }

    #[test]
    fn scan_includes_uppercase_extensions() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join("src")).expect("src dir");
        fs::write(
            workspace.join("src").join("App.TSX"),
            "export const App = 1;",
        )
        .expect("uppercase source");

        let files = scan_workspace_sources(&workspace);

        fs::remove_dir_all(&workspace).expect("workspace cleanup");
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with(Path::new("src/App.TSX")));
    }

    #[test]
    fn scan_respects_depth_cap() {
        let workspace = temp_workspace();
        let mut deep = workspace.clone();
        for _ in 0..(MAX_SCAN_DEPTH + 5) {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).expect("deep dirs");
        fs::write(deep.join("beyond.ts"), "export const beyond = 1;").expect("beyond-cap source");
        fs::create_dir_all(workspace.join("src")).expect("src dir");
        fs::write(
            workspace.join("src").join("index.ts"),
            "export const i = 1;",
        )
        .expect("shallow source");

        let files = scan_workspace_sources(&workspace);

        fs::remove_dir_all(&workspace).expect("workspace cleanup");
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with(Path::new("src/index.ts")));
    }

    #[cfg(windows)]
    #[test]
    fn scan_terminates_on_directory_junction_cycles() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join("src")).expect("src dir");
        fs::write(
            workspace.join("src").join("index.ts"),
            "export const i = 1;",
        )
        .expect("source file");

        let link = workspace.join("src").join("loop");
        let status = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &workspace.to_string_lossy(),
            ])
            .output();
        let junction_created = status
            .map(|output| output.status.success() && link.is_dir())
            .unwrap_or(false);
        if !junction_created {
            eprintln!(
                "skipping junction cycle assertion: mklink /J unavailable in this environment"
            );
            fs::remove_dir_all(&workspace).expect("workspace cleanup");
            return;
        }

        let files = scan_workspace_sources(&workspace);

        // Remove the junction first so cleanup never follows it back to root.
        fs::remove_dir(&link).expect("junction removal");
        fs::remove_dir_all(&workspace).expect("workspace cleanup");
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with(Path::new("src/index.ts")));
        assert!(
            files
                .iter()
                .all(|file| !file.components().any(|part| part.as_os_str() == "loop"))
        );
    }
}
