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
