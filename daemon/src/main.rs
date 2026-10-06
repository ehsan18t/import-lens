use import_lens_daemon::ipc::server::run_server;
use rayon::ThreadPoolBuilder;
use std::{env, error::Error, path::PathBuf};

#[derive(Debug, Default)]
struct Args {
    pipe: Option<String>,
    storage: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    configure_rayon_pool();
    let args = parse_args(env::args().skip(1))?;
    let pipe = args.pipe.ok_or("missing required --pipe argument")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_server(&pipe, args.storage));
    // The connection has already flushed the cache. A blocking handler still draining engine
    // builds has no cancellation point, and dropping the runtime would wait for it without limit.
    runtime.shutdown_background();

    result
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
            // Accepted and ignored: the extension and the CLI still pass it, but the workspace
            // root of every request comes from the client's `hello`.
            "--workspace" => {
                iterator.next();
            }
            "--storage" => parsed.storage = iterator.next().map(PathBuf::from),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(parsed)
}
