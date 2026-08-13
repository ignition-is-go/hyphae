//! Shared native worker pool for coarse Hyphae runtime work.
//!
//! This module exists only with the `scheduler` feature. Scheduler waves and
//! eligible compiled join regions use one lazily constructed, dedicated Rayon
//! pool rather than the process-global pool or independent pools. Wasm never
//! constructs it, and map queries without `scheduler` compile a sequential
//! path. Native configuration checks `HYPHAE_WORKER_THREADS` first, then the
//! compatibility fallback `HYPHAE_WAVE_THREADS`; zero disables the pool. The
//! default is available parallelism capped at four workers.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::LazyLock;

#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_THREADS_CAP: usize = 4;

#[cfg(not(target_arch = "wasm32"))]
fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(not(target_arch = "wasm32"))]
static WORKER_THREADS: LazyLock<usize> = LazyLock::new(|| {
    env_usize("HYPHAE_WORKER_THREADS")
        .or_else(|| env_usize("HYPHAE_WAVE_THREADS"))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, |count| count.get().min(DEFAULT_THREADS_CAP))
        })
});

#[cfg(not(target_arch = "wasm32"))]
static WORKER_POOL: LazyLock<Option<rayon::ThreadPool>> = LazyLock::new(|| {
    let threads = *WORKER_THREADS;
    if threads == 0 {
        return None;
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("hyphae-worker-{index}"))
        .build()
        .ok()
});

/// Return the shared native pool, constructing it on the first eligible coarse
/// workload. `None` means parallel execution was explicitly disabled or pool
/// construction failed. Callers perform their cost and balance gates before
/// requesting the pool.
#[cfg(not(target_arch = "wasm32"))]
pub fn worker_pool() -> Option<&'static rayon::ThreadPool> {
    WORKER_POOL.as_ref()
}

/// Return the configured worker count without constructing the lazy pool.
#[cfg(not(target_arch = "wasm32"))]
pub fn configured_worker_threads() -> usize {
    *WORKER_THREADS
}

#[cfg(target_arch = "wasm32")]
pub const fn configured_worker_threads() -> usize {
    1
}
