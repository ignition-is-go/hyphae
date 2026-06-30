//! Bridge hyphae cells into Leptos reactive signals.

use hyphae::{CellValue, Mutable, Signal as HyphaeSignal, Watchable};
use leptos::prelude::{Effect, Get, RwSignal, Set, WithUntracked, on_cleanup};

/// Tie a hyphae subscription to the current reactive owner: when the owning
/// component/scope is disposed, the guard drops and the subscription releases.
///
/// Must be called inside a reactive owner (component body or effect).
fn keep_alive(guard: hyphae::SubscriptionGuard) {
    on_cleanup(move || drop(guard));
}

/// One-way projection of any hyphae [`Watchable`] into a Leptos signal.
pub trait IntoLeptosSignal<T: CellValue>: Watchable<T> {
    /// Create a Leptos `RwSignal` seeded with the cell's current value and
    /// driven by it: every hyphae `Signal::Value` pushes into the signal,
    /// re-running the Leptos effects/views that read it.
    ///
    /// Writes to the returned signal do **not** flow back to the cell — use
    /// [`BindLeptosSignal::bind_leptos_signal`] for two-way binding. Call
    /// within a reactive owner so the underlying subscription is cleaned up
    /// with the component.
    fn into_leptos_signal(&self) -> RwSignal<T> {
        let signal = RwSignal::new(self.get());
        let guard = self.subscribe(move |s| {
            if let HyphaeSignal::Value(value) = s {
                signal.set((**value).clone());
            }
        });
        keep_alive(guard);
        signal
    }
}

impl<T: CellValue, W: Watchable<T>> IntoLeptosSignal<T> for W {}

/// Two-way binding for mutable hyphae cells.
pub trait BindLeptosSignal<T: CellValue>: Mutable<T> {
    /// Create a Leptos `RwSignal` two-way bound to this cell: cell changes push
    /// into the signal, and signal writes push back into the cell.
    ///
    /// Requires `T: PartialEq` to break the feedback loop — each side only
    /// propagates when the value actually differs, so an echo (cell → signal →
    /// cell) terminates instead of looping. Call within a reactive owner.
    fn bind_leptos_signal(&self) -> RwSignal<T>
    where
        T: PartialEq,
    {
        let signal = RwSignal::new(self.get());

        // cell -> signal
        let guard = self.subscribe(move |s| {
            if let HyphaeSignal::Value(value) = s {
                let value = (**value).clone();
                if signal.with_untracked(|current| current != &value) {
                    signal.set(value);
                }
            }
        });
        keep_alive(guard);

        // signal -> cell
        let cell = self.clone();
        Effect::new(move |_| {
            let value = signal.get();
            if cell.get() != value {
                cell.set(value);
            }
        });

        signal
    }
}

impl<T: CellValue, M: Mutable<T>> BindLeptosSignal<T> for M {}

#[cfg(test)]
mod tests {
    use hyphae::{Cell, Mutable};
    use leptos::prelude::{GetUntracked, Owner};

    use super::IntoLeptosSignal;

    #[test]
    fn cell_drives_signal() {
        let owner = Owner::new();
        owner.with(|| {
            let cell = Cell::new(5);
            let signal = cell.into_leptos_signal();
            assert_eq!(signal.get_untracked(), 5);

            cell.set(10);
            assert_eq!(signal.get_untracked(), 10);

            cell.set(42);
            assert_eq!(signal.get_untracked(), 42);
        });
    }
}
