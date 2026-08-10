//! Shared native worker pool for coarse Hyphae runtime work.
//!
//! Scheduler waves and compiled map queries use the same lazily constructed
//! pool so enabling both cannot oversubscribe the process with independent
//! Rayon pools. The pool is absent on wasm and can be disabled on native with
//! `HYPHAE_WORKER_THREADS=0`. `HYPHAE_WAVE_THREADS` remains a compatibility
//! fallback for existing scheduler deployments.

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

/// Return the shared native pool, constructing it on first useful parallel
/// workload. `None` means parallel execution was explicitly disabled.
#[cfg(not(target_arch = "wasm32"))]
pub fn worker_pool() -> Option<&'static rayon::ThreadPool> {
    WORKER_POOL.as_ref()
}
