//! Whole-file replacement that a crash cannot tear.

use std::{
    fs, io,
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `bytes` so that a reader sees either the old file or the new one in full,
/// never a truncated or partially written one.
///
/// The bytes go to a sibling temp file, are flushed to disk, and the temp file is renamed over the
/// target (a same-directory rename is an atomic replace on every supported platform). The temp name
/// is unique per process and per call: these files live in storage shared by every window's daemon,
/// and two writers sharing one temp path could interleave into it and rename the mix into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temp_path = path.with_file_name(temp_name);

    let written = write_durably(&temp_path, bytes).and_then(|()| fs::rename(&temp_path, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    written
}

fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    };

    /// A reader racing a stream of rewrites must only ever see a complete version. A truncating
    /// in-place write exposes the empty and half-written states this rules out.
    #[test]
    fn a_concurrent_reader_never_sees_a_torn_file() {
        let directory = std::env::temp_dir().join(format!(
            "import-lens-atomic-write-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("state.json");
        let version = |index: usize| vec![b'a' + (index % 26) as u8; 256 * 1024];
        write_atomic(&path, &version(0)).expect("seed write");

        let done = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = {
            let path = path.clone();
            let done = Arc::clone(&done);
            let reads = Arc::clone(&reads);
            std::thread::spawn(move || {
                let mut torn = 0usize;
                while !done.load(Ordering::Relaxed) {
                    // A sharing violation during the swap is not a torn read; skip it.
                    let Ok(bytes) = fs::read(&path) else {
                        continue;
                    };
                    reads.fetch_add(1, Ordering::Relaxed);
                    let complete =
                        bytes.len() == 256 * 1024 && bytes.iter().all(|byte| *byte == bytes[0]);
                    if !complete {
                        torn += 1;
                    }
                }
                torn
            })
        };

        let mut writes = 0usize;
        let started = std::time::Instant::now();
        let mut index = 0;
        while reads.load(Ordering::Relaxed) < 500
            && started.elapsed() < std::time::Duration::from_secs(2)
        {
            index += 1;
            if write_atomic(&path, &version(index)).is_ok() {
                writes += 1;
            }
        }
        done.store(true, Ordering::Relaxed);
        let torn = reader.join().expect("reader thread");

        assert!(writes > 0, "at least one rewrite must land");
        assert!(
            reads.load(Ordering::Relaxed) > 0,
            "the reader must observe the file"
        );
        assert_eq!(torn, 0, "a reader saw a partially written file");
        let leftovers = fs::read_dir(&directory)
            .expect("temp dir listing")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "state.json")
            .count();
        assert_eq!(leftovers, 0, "no temp file may outlive its write");
        fs::remove_dir_all(directory).expect("temp dir cleanup");
    }
}
