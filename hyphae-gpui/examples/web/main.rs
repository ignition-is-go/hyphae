use gloo_timers::callback::Timeout;
use gpui::{App, Bounds, Context, Entity, WindowBounds, WindowOptions, div, prelude::*, px, size};
use hyphae::{Cell, Mutable};
use hyphae_gpui::{CellEntity, ObserveCellEntityExt as _, ToGpuiEntity as _};

struct Smoke {
    value: u32,
    _cell: Entity<CellEntity<u32>>,
    _observation: gpui::Subscription,
}

impl Render for Smoke {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div().p(px(24.)).child(format!("received: {}", self.value))
    }
}

fn launch(cx: &mut App) {
    let source = Cell::new(1_u32);
    let cell = source.to_gpui_entity(cx);
    let bounds = Bounds::centered(None, size(px(480.), px(240.)), cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_window, cx| {
            cx.new(|cx| {
                let observation = cx.observe_cell(&cell, |smoke: &mut Smoke, value, cx| {
                    smoke.value = *value;
                    if *value == 42 {
                        if let Some(document) =
                            web_sys::window().and_then(|window| window.document())
                        {
                            document.set_title("hyphae-gpui: PASS");
                        }
                        web_sys::console::log_1(&"hyphae-gpui bridge received 42".into());
                    }
                    cx.notify();
                });
                Smoke {
                    value: cell.read(cx).value().copied().unwrap_or_default(),
                    _cell: cell,
                    _observation: observation,
                }
            })
        },
    );
    if window.is_err() {
        web_sys::console::error_1(&"failed to open smoke window".into());
        return;
    }

    // This browser callback is the event source. The adapter itself has no
    // timeout, frame callback, or polling loop.
    Timeout::new(250, move || source.set(42)).forget();
    cx.activate(true);
}

thread_local! {
    static APPLICATION: std::cell::RefCell<Option<gpui::ApplicationHandle>> = const { std::cell::RefCell::new(None) };
}

fn main() {
    gpui_platform::web_init();
    let app = gpui_platform::application().run_embedded(launch);
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(app));
}
