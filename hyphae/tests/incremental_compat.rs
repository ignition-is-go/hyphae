//! Differential transaction/topology contract against the existing local evaluator.
#![cfg(all(feature = "incremental-runtime", feature = "local-runtime"))]

use std::sync::Arc;

use hyphae::{incremental as backend, local};
use parking_lot::Mutex;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn dynamic_graph_matches_owned_evaluator_through_ordered_transactions() -> TestResult {
    let mut old = local::Runtime::new();
    let oa = old.input(0_u64)?;
    let ob = old.input(0_u64)?;
    let os = old.input(0_u64)?;
    let joined = old.map2(&oa.cell(), &ob.cell(), |a, b| {
        a.wrapping_mul(3).wrapping_add(*b)
    })?;
    let selected = old.switch_map(&os.cell(), move |builder, selector| {
        let revision = *selector;
        builder.map(&joined, move |v| v.wrapping_add(revision))
    })?;
    let old_seen = Arc::new(Mutex::new(Vec::new()));
    let output = old_seen.clone();
    let _old_guard = old.subscribe(&selected, move |value, _| {
        output.lock().push(*value);
        Ok(())
    })?;
    old.stabilize()?;

    let mut new = backend::Runtime::new();
    let na = new.input(0_u64);
    let nb = new.input(0_u64);
    let ns = new.input(0_u64);
    let joined = na
        .cell()
        .map2(&nb.cell(), |a, b| a.wrapping_mul(3).wrapping_add(*b))?;
    let selected = ns.cell().switch_map(move |_, selector| {
        let revision = *selector;
        joined.map(move |v| v.wrapping_add(revision))
    });
    let new_seen = Arc::new(Mutex::new(Vec::new()));
    let output = new_seen.clone();
    let _new_guard = new.subscribe(&selected, move |value, _| output.lock().push(*value))?;
    new.transaction(|_| Ok(()))?;

    let mut seed = 17_u64;
    for _ in 0..1000 {
        let mut writes = Vec::new();
        for _ in 0..8 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let which = seed.checked_rem(3).unwrap_or_default();
            let value = seed
                .checked_shr(16)
                .unwrap_or_default()
                .checked_rem(5)
                .unwrap_or_default();
            writes.push((which, value));
        }
        old.transaction(|tx| {
            for (which, value) in &writes {
                let input = match which {
                    0 => &oa,
                    1 => &ob,
                    _ => &os,
                };
                tx.set(input, *value)?;
            }
            Ok(())
        })?;
        new.transaction(|tx| {
            for (which, value) in &writes {
                let input = match which {
                    0 => &na,
                    1 => &nb,
                    _ => &ns,
                };
                tx.set(input, *value)?;
            }
            Ok(())
        })?;
        if *old_seen.lock() != *new_seen.lock() {
            return Err(format!(
                "notification mismatch: old={:?} new={:?}",
                *old_seen.lock(),
                *new_seen.lock()
            )
            .into());
        }
        old_seen.lock().clear();
        new_seen.lock().clear();
    }
    Ok(())
}
