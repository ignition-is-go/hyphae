use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crossbeam::queue::SegQueue;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait BufferTimeExt<T>: Watchable<T> {
    /// Collect values over a time window before emitting.
    ///
    /// Emits a `Vec<T>` containing all values received during each time window.
    /// Empty windows emit an empty Vec.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, BufferTimeExt};
    /// use std::time::Duration;
    ///
    /// let source = Cell::new(0);
    /// let buffered = source.buffer_time(Duration::from_millis(100));
    ///
    /// source.set(1);
    /// source.set(2);
    /// // After 100ms, emits [1, 2]
    /// ```
    #[track_caller]
    fn buffer_time(&self, duration: Duration) -> Cell<Vec<T>, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<Vec<T>, CellMutable>::new(Vec::new());
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::buffer_time", name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let buffer: Arc<SegQueue<T>> = Arc::new(SegQueue::new());
        let first = Arc::new(AtomicBool::new(true));
        let completed = Arc::new(AtomicBool::new(false));

        // Spawn timer thread
        let buffer2 = buffer.clone();
        let weak2 = derived.downgrade();
        let comp = completed.clone();
        thread::spawn(move || {
            while !comp.load(Ordering::SeqCst) {
                thread::sleep(duration);
                if comp.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(d) = weak2.upgrade() {
                    let mut chunk = Vec::new();
                    while let Some(v) = buffer2.pop() {
                        chunk.push(v);
                    }
                    d.notify(Signal::value(chunk));
                } else {
                    break;
                }
            }
        });

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        buffer.push((**value).clone());
                    }
                    Signal::Complete => {
                        completed.store(true, Ordering::SeqCst);
                        // Emit remaining buffer
                        let mut remainder = Vec::new();
                        while let Some(v) = buffer.pop() {
                            remainder.push(v);
                        }
                        if !remainder.is_empty() {
                            d.notify(Signal::value(remainder));
                        }
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => {
                        completed.store(true, Ordering::SeqCst);
                        d.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> BufferTimeExt<T> for W {}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use std::{sync::Mutex, time::Instant};

    use super::*;
    use crate::Mutable;

    #[test]
    fn test_buffer_time() {
        let source = Cell::new(0);
        let buffered = source.buffer_time(Duration::from_millis(50));

        let emissions = Arc::new(Mutex::new(Vec::<Vec<i32>>::new()));
        let e = emissions.clone();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push((**v).clone());
            }
        });

        // Initial empty
        assert_eq!(emissions.lock().unwrap().len(), 1);

        // Add values
        source.set(1);
        source.set(2);
        source.set(3);

        let deadline = Instant::now() + Duration::from_millis(1000);
        loop {
            {
                let emitted = emissions.lock().unwrap();
                let has_expected = emitted.iter().any(|v| v == &vec![1, 2, 3]);
                if has_expected {
                    break;
                }
            }

            if Instant::now() >= deadline {
                let emitted = emissions.lock().unwrap();
                panic!("Timed out waiting for buffered emission [1, 2, 3], got {emitted:?}");
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_buffer_time_emits_remainder_on_complete() {
        let source = Cell::new(0);
        let buffered = source.buffer_time(Duration::from_millis(100));

        let emissions = Arc::new(Mutex::new(Vec::<Vec<i32>>::new()));
        let e = emissions.clone();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push((**v).clone());
            }
        });

        source.set(1);
        source.set(2);

        // Complete before timer
        source.complete();

        let deadline = Instant::now() + Duration::from_millis(1000);
        loop {
            {
                let emitted = emissions.lock().unwrap();
                let has_expected = emitted.iter().any(|v| v == &vec![1, 2]);
                if has_expected {
                    break;
                }
            }

            if Instant::now() >= deadline {
                let emitted = emissions.lock().unwrap();
                panic!("Timed out waiting for completion remainder [1, 2], got {emitted:?}");
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
}
