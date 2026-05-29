use import_lens_daemon::ipc::server::run_server;
use rayon::ThreadPoolBuilder;
use std::{env, error::Error, path::PathBuf};

#[derive(Debug, Default)]
struct Args {
    pipe: Option<String>,
    workspace: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    configure_rayon_pool();
    let args = parse_args(env::args().skip(1))?;
    let pipe = args.pipe.ok_or("missing required --pipe argument")?;
    let workspace = args
        .workspace
        .ok_or("missing required --workspace argument")?;

    run_server(&pipe, workspace).await
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
            "--storage" => {
                let _ = iterator.next();
            }
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(parsed)
}
