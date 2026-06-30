//! Bridge hyphae cells into Leptos reactive signals.
//!
//! The cell's mutability marker decides the bridge, so the type system picks the
//! right one for you:
//!
//! - [`Cell<T, CellMutable>`](hyphae::Cell) → [`RwSignal<T>`], bridged **both
//!   ways**. Cell changes push into the signal; signal writes push back into the
//!   cell. (Requires `T: PartialEq` to break the feedback loop.)
//! - [`Cell<T, CellImmutable>`](hyphae::Cell) → [`ReadSignal<T>`], bridged **one
//!   way** — a derived/locked cell something else owns and writes can only be
//!   read here.
//!
//! The conversion is named `to_leptos_signal` (not `into_`): it borrows the cell
//! and *subscribes* to it, leaving the original handle alive and emitting.

use hyphae::{
    Cell, CellImmutable, CellMutable, CellValue, Gettable, Mutable, Signal as HyphaeSignal,
    Watchable,
};
use leptos::prelude::{Effect, Get, ReadSignal, RwSignal, Set, WithUntracked, on_cleanup};

/// Tie a hyphae subscription to the current reactive owner: when the owning
/// component/scope is disposed, the guard drops and the subscription releases.
///
/// Must be called inside a reactive owner (component body or effect).
fn keep_alive(guard: hyphae::SubscriptionGuard) {
    on_cleanup(move || drop(guard));
}

/// Project a hyphae cell into the matching Leptos signal type.
///
/// Implemented for both cell flavours; the associated [`Signal`](Self::Signal)
/// type and the directionality follow the cell's mutability — see the
/// [module docs](self). Call within a reactive owner so the underlying
/// subscription is released when the component is disposed.
pub trait ToLeptosSignal {
    /// The Leptos signal type produced: `RwSignal<T>` for a mutable cell,
    /// `ReadSignal<T>` for an immutable one.
    type Signal;

    /// Create the Leptos signal, seeded with the cell's current value and driven
    /// by it. Borrows the cell (subscribes to it) rather than consuming it.
    fn to_leptos_signal(&self) -> Self::Signal;
}

/// Mutable cell → read/write signal, bridged both ways.
impl<T: CellValue + PartialEq> ToLeptosSignal for Cell<T, CellMutable> {
    type Signal = RwSignal<T>;

    fn to_leptos_signal(&self) -> RwSignal<T> {
        let signal = RwSignal::new(self.get());

        // cell -> signal: only push when the value actually differs, so the
        // echo of our own signal -> cell write doesn't re-fire the signal.
        let guard = self.subscribe(move |s| {
            if let HyphaeSignal::Value(value) = s {
                let value = (**value).clone();
                if signal.with_untracked(|current| current != &value) {
                    signal.set(value);
                }
            }
        });
        keep_alive(guard);

        // signal -> cell: write back only on a real change, so the echo of a
        // cell -> signal update terminates instead of looping.
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

/// Immutable cell → read-only signal, bridged one way.
impl<T: CellValue> ToLeptosSignal for Cell<T, CellImmutable> {
    type Signal = ReadSignal<T>;

    fn to_leptos_signal(&self) -> ReadSignal<T> {
        let signal = RwSignal::new(self.get());
        let guard = self.subscribe(move |s| {
            if let HyphaeSignal::Value(value) = s {
                signal.set((**value).clone());
            }
        });
        keep_alive(guard);
        signal.read_only()
    }
}

#[cfg(test)]
mod tests {
    use hyphae::{Cell, Mutable};
    use leptos::prelude::{GetUntracked, Owner};

    use super::ToLeptosSignal;

    #[test]
    fn mutable_cell_drives_rw_signal() {
        let owner = Owner::new();
        owner.with(|| {
            let cell = Cell::new(5);
            let signal = cell.to_leptos_signal(); // RwSignal<i32>
            assert_eq!(signal.get_untracked(), 5);

            cell.set(10);
            assert_eq!(signal.get_untracked(), 10);

            cell.set(42);
            assert_eq!(signal.get_untracked(), 42);
        });
    }

    #[test]
    fn immutable_cell_drives_read_signal() {
        let owner = Owner::new();
        owner.with(|| {
            // `.lock()` converts a mutable cell into an immutable handle.
            let source = Cell::new(1);
            let view = source.clone().lock();
            let signal = view.to_leptos_signal(); // ReadSignal<i32>
            assert_eq!(signal.get_untracked(), 1);

            source.set(7);
            assert_eq!(signal.get_untracked(), 7);
        });
    }
}
