//! # hyphae-leptos
//!
//! Bridge [hyphae](https://docs.rs/hyphae) reactive cells and cell-maps into
//! [Leptos](https://leptos.dev) 0.8 signals and fine-grained stores, so hyphae
//! can drive a Leptos UI directly.
//!
//! hyphae owns the reactive *computation graph* (lock-free cells, joins,
//! time-based operators, cell-maps); Leptos owns the *view* layer. This crate
//! is the seam: it subscribes to hyphae and pushes changes into Leptos
//! reactivity, tying every subscription's lifetime to the Leptos owner so it is
//! released when the component is disposed.
//!
//! ## Cells → signals
//!
//! ```ignore
//! use hyphae::Cell;
//! use hyphae_leptos::ToLeptosSignal;
//!
//! // A mutable cell bridges both ways -> RwSignal (an <input> and the cell
//! // stay in sync).
//! let temperature = Cell::new(20.0);
//! let bound = temperature.to_leptos_signal();
//!
//! // A derived/locked (immutable) cell bridges one way -> ReadSignal.
//! let reading = temperature.clone().lock().to_leptos_signal();
//! view! { <p>"Temp: " {move || reading.get()}</p> };
//! ```
//!
//! ## CellMaps → stores
//!
//! Each key owns its own signal, so editing one entry re-renders only that
//! row — the fine-grained behaviour of a Leptos store, applied to a dynamic,
//! diff-driven [`hyphae::CellMap`].
//!
//! ```ignore
//! use hyphae::CellMap;
//! use hyphae_leptos::CellMapStoreExt;
//!
//! let users: CellMap<u64, String> = CellMap::new();
//! let store = users.to_leptos_store();
//!
//! view! {
//!     // `value` returns a `ReadSignal<Option<V>>` — present once the key's
//!     // first diff lands (subscribe-before-data), `None` until then.
//!     <For each=move || store.keys().get() key=|k| *k let:id>
//!         <li>{move || store.value(&id).get()}</li>
//!     </For>
//! }
//! ```
//!
//! ## Threading
//!
//! The bridge targets the browser (client-side rendering), where both hyphae
//! notifications and Leptos reactivity run on the single JS event-loop thread.
//! All Leptos handles used here ([`reactive_graph::signal::RwSignal`],
//! [`reactive_graph::owner::StoredValue`]) use the default thread-safe storage,
//! so the subscription closures satisfy hyphae's `Send + Sync` bound; updates
//! must still originate on the reactive thread.

mod signal;
mod store;

pub use signal::ToLeptosSignal;
pub use store::{CellMapStore, CellMapStoreExt, MapGroup, NestedMapStore, NestedMapStoreExt};
