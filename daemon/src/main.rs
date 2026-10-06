use import_lens_daemon::ipc::server::run_server;
use rayon::ThreadPoolBuilder;
use std::{env, error::Error, path::PathBuf};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Default)]
struct Args {
    pipe: Option<String>,
    workspace: Option<PathBuf>,
    storage: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    configure_allocator();
    configure_rayon_pool();
    let args = parse_args(env::args().skip(1))?;
    let pipe = args.pipe.ok_or("missing required --pipe argument")?;
    let workspace = args
        .workspace
        .ok_or("missing required --workspace argument")?;

    run_server(&pipe, workspace, args.storage).await
}

/// `mi_option_purge_delay` in mimalloc's `mi_option_e`; the Rust binding does not name it.
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// Return freed pages to the OS immediately. With the default delay a purge only runs when
/// the freeing thread next allocates, and the daemon's pool threads go idle right after a
/// build, so the pages of a finished build stay resident.
fn configure_allocator() {
    // SAFETY: sets a process-global allocator option; mimalloc reads it atomically.
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
}

fn configure_rayon_pool() {
    let threads = std::thread::available_parallelism()
        .map(|value| value.get().saturating_sub(2).max(1))
        .unwrap_or(1);

    let _ = ThreadPoolBuilder::new().num_threads(threads).build_global();
}

fn parse_args<I>(args: I) -> Result<Args, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = Args::default();
    let mut iterator = args.into_iter();

    while let Some(arg) = iterator.next() {
        match arg.as_str() {
            "--pipe" => parsed.pipe = iterator.next(),
            "--workspace" => parsed.workspace = iterator.next().map(PathBuf::from),
            "--storage" => parsed.storage = iterator.next().map(PathBuf::from),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(parsed)
}
