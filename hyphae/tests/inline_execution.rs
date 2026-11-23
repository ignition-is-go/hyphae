#![cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]

use std::{
    panic::{catch_unwind, resume_unwind},
    process::Command,
    thread,
};

use hyphae::{Cell, Mutable, Signal, Watchable, batch, executor::with_inline_execution};

#[test]
fn scoped_waves_run_on_the_caller_and_restore_dispatch_after_unwinding() {
    const CHILD: &str = "HYPHAE_INLINE_EXECUTION_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::env::current_exe().and_then(|exe| {
            Command::new(exe)
                .env(CHILD, "1")
                .env("HYPHAE_WORKER_THREADS", "4")
                .status()
        });
        assert!(matches!(status, Ok(s) if s.success()), "{status:?}");
        return;
    }
    hyphae::scheduler::set_wave_threshold_for_test(4);
    let caller = thread::current().id();
    let sources: Vec<_> = (0..4).map(|_| Cell::new(0)).collect();
    let (send, observed) = std::sync::mpsc::channel();
    let _guards: Vec<_> = sources
        .iter()
        .map(|source| {
            let send = send.clone();
            source.subscribe(move |signal| {
                if matches!(signal, Signal::Value(n) if **n != 0) {
                    assert!(send.send(thread::current().id()).is_ok());
                }
            })
        })
        .collect();
    let wave = |value| {
        batch(|| sources.iter().for_each(|s| s.set(value)));
        let threads: Vec<_> = observed.try_iter().collect();
        assert_eq!(threads.len(), sources.len());
        threads
    };
    assert!(wave(1).iter().all(|&id| id != caller));
    with_inline_execution(|| {
        assert!(catch_unwind(|| with_inline_execution(|| resume_unwind(Box::new(())))).is_err());
        assert!(wave(2).iter().all(|&id| id == caller));
    });
    assert!(catch_unwind(|| with_inline_execution(|| resume_unwind(Box::new(())))).is_err());
    assert!(wave(3).iter().all(|&id| id != caller));
}
