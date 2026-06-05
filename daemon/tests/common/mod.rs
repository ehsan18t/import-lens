use std::{
    fs,
    path::PathBuf,
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
        zip.extract(&target)
            .expect("fixture archive should extract successfully");
        target
    })
}
