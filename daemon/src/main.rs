use import_lens_daemon::{ipc::server::run_server, reclaim};
use libmimalloc_sys::{mi_heap_malloc_aligned, mi_heap_realloc_aligned, mi_heap_zalloc_aligned};
use rayon::ThreadPoolBuilder;
use std::alloc::{GlobalAlloc, Layout};
use std::{env, error::Error, path::PathBuf};

/// mimalloc, with every allocation made inside `reclaim::long_lived` sent to the long-lived heap.
/// mimalloc frees a block into whichever heap owns it, so `dealloc` needs no routing.
struct DaemonAllocator;

#[global_allocator]
static GLOBAL: DaemonAllocator = DaemonAllocator;

// SAFETY: every path hands `layout`'s size and alignment to mimalloc, which honours both; blocks
// from either heap are freed by `mi_free`, which finds the owning heap itself.
unsafe impl GlobalAlloc for DaemonAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match reclaim::long_lived_heap_in_scope() {
            Some(heap) => unsafe {
                mi_heap_malloc_aligned(heap, layout.size(), layout.align()).cast()
            },
            None => unsafe { mimalloc::MiMalloc.alloc(layout) },
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match reclaim::long_lived_heap_in_scope() {
            Some(heap) => unsafe {
                mi_heap_zalloc_aligned(heap, layout.size(), layout.align()).cast()
            },
            None => unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) },
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        unsafe { mimalloc::MiMalloc.dealloc(block, layout) }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        match reclaim::long_lived_heap_in_scope() {
            Some(heap) => unsafe {
                mi_heap_realloc_aligned(heap, block.cast(), new_size, layout.align()).cast()
            },
            None => unsafe { mimalloc::MiMalloc.realloc(block, layout, new_size) },
        }
    }
}

#[derive(Debug, Default)]
struct Args {
    pipe: Option<String>,
    storage: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    configure_allocator();
    configure_rayon_pool();
    let args = parse_args(env::args().skip(1))?;
    let pipe = args.pipe.ok_or("missing required --pipe argument")?;

    // The IPC runtime only multiplexes one connection's frames; every handler runs on the
    // blocking pool and every build on the engine runtime, so a worker per core would only
    // add idle threads, each holding its own allocator heap.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_park(reclaim::collect_this_thread)
        .enable_all()
        .build()?;
    runtime.spawn(reclaim::sweep_when_settled());
    let result = runtime.block_on(run_server(&pipe, args.storage));
    // The connection has already flushed the cache. A blocking handler still draining engine
    // builds has no cancellation point, and dropping the runtime would wait for it without limit.
    runtime.shutdown_background();

    result
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
            // Accepted and ignored: the extension and the CLI pass it, but the workspace root of
            // every request comes from the client's `hello`.
            "--workspace" => {
                iterator.next();
            }
            "--storage" => parsed.storage = iterator.next().map(PathBuf::from),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(parsed)
}
