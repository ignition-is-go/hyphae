//! Compare cheap and compute-heavy waves at several widths.
//! Run separate processes with `HYPHAE_WAVE_THRESHOLD=64` and 100000.
//! Every case verifies the final value on every cell.

use std::{hint::black_box, time::Instant};

use hyphae::{Cell, Gettable, MapExt, Materialize, Mutable};
fn main() {
    println!("width,work,ns_per_frame");
    for width in [32, 64, 128, 256, 512, 1024] {
        for work in [0, 4096, 65_536] {
            let sources: Vec<_> = (0..width).map(|_| Cell::new(0_u64)).collect();
            let outputs: Vec<_> = sources
                .iter()
                .map(|source| {
                    source
                        .clone()
                        .map(move |value| {
                            let mut acc = *value;
                            for i in 0..work {
                                acc = black_box(acc.wrapping_mul(31).wrapping_add(i));
                            }
                            acc
                        })
                        .materialize()
                })
                .collect();
            let frames = if work == 65_536 { 30 } else { 150 };
            let start = Instant::now();
            for frame in 1..=frames {
                hyphae::batch(|| {
                    for source in &sources {
                        source.set(frame);
                    }
                });
            }
            let elapsed = start
                .elapsed()
                .as_nanos()
                .checked_div(u128::from(frames))
                .unwrap_or(0);
            let mut expected = frames;
            for i in 0..work {
                expected = black_box(expected.wrapping_mul(31).wrapping_add(i));
            }
            assert!(outputs.iter().all(|output| output.get() == expected));
            println!("{width},{work},{elapsed}");
        }
    }
}
