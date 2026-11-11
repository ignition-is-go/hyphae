use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;

use super::{Cell, Error, Runtime};

#[test]
fn diamond_is_coherent_and_equal_values_stop_work() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(1_u64)?;
    let b = rt.input(2_u64)?;
    let left = rt.map(&a.cell(), |v| v.wrapping_mul(2))?;
    let right = rt.map(&b.cell(), |v| v.wrapping_mul(3))?;
    let sum = rt.map2(&left, &right, |a, b| a.wrapping_add(*b))?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let out = rt.subscribe(&sum, move |v, _| {
        log.lock().push(*v);
        Ok(())
    })?;
    check_eq(&(out.get(&rt)), &(Err(Error::Unsettled)))?;
    rt.stabilize()?;
    rt.transaction(|tx| {
        tx.set(&a, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(*seen.lock()), &(vec![8, 80]))?;
    let before = rt.stats().evaluations;
    rt.transaction(|tx| {
        tx.set(&a, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(rt.stats().evaluations), &(before))?;
    check_eq(&(out.get(&rt)?), &(80))?;
    Ok(())
}
#[test]
fn unequal_writes_are_ordered_and_equal_writes_are_skipped() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0_u64)?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let _out = rt.subscribe(&input.cell(), move |v, _| {
        log.lock().push(*v);
        Ok(())
    })?;
    rt.stabilize()?;
    rt.transaction(|tx| {
        for value in [1, 1, 2, 3, 3] {
            tx.set(&input, value)?;
        }
        Ok(())
    })?;
    check_eq(&(*seen.lock()), &(vec![0, 1, 2, 3]))?;
    Ok(())
}
#[test]
fn different_inputs_share_steps_without_losing_repeated_input_values() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_u64)?;
    let b = rt.input(0_u64)?;
    let joined = rt.map2(&a.cell(), &b.cell(), |a, b| (*a, *b))?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let _out = rt.subscribe(&joined, move |v, _| {
        log.lock().push(*v);
        Ok(())
    })?;
    rt.stabilize()?;
    rt.transaction(|tx| {
        tx.set(&a, 1)?;
        tx.set(&a, 2)?;
        tx.set(&b, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(*seen.lock()), &(vec![(0, 0), (1, 10), (2, 20)]))?;
    Ok(())
}
#[test]
fn unchanged_derived_value_cuts_off_descendants() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(1_u64)?;
    let parity = rt.map(&a.cell(), |v| v.checked_rem(2).unwrap_or(0))?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let tail = rt.map(&parity, move |v| {
        count.fetch_add(1, Ordering::Relaxed);
        *v
    })?;
    let out = rt.observe(&tail)?;
    rt.stabilize()?;
    rt.transaction(|tx| tx.set(&a, 3))?;
    check_eq(&(calls.load(Ordering::Relaxed)), &(1))?;
    check_eq(&(out.get(&rt)?), &(1))?;
    Ok(())
}
#[test]
fn branch_selection_avoids_unneeded_work_and_reactivates_latest_input() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(false)?;
    let a = rt.input(1_u64)?;
    let b = rt.input(10_u64)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let right = rt.map(&b.cell(), move |v| {
        count.fetch_add(1, Ordering::Relaxed);
        v.wrapping_mul(2)
    })?;
    let left = a.cell();
    let branch = rt.switch_map(&selector.cell(), move |_, choose| {
        Ok(if *choose { right.clone() } else { left.clone() })
    })?;
    let out = rt.observe(&branch)?;
    rt.stabilize()?;
    check_eq(&(calls.load(Ordering::Relaxed)), &(0))?;
    rt.transaction(|tx| tx.set(&b, 20))?;
    check_eq(&(calls.load(Ordering::Relaxed)), &(0))?;
    rt.transaction(|tx| tx.set(&selector, true))?;
    check_eq(&(out.get(&rt)?), &(40))?;
    rt.transaction(|tx| tx.set(&selector, false))?;
    rt.transaction(|tx| tx.set(&b, 30))?;
    check_eq(&(calls.load(Ordering::Relaxed)), &(1))?;
    rt.transaction(|tx| tx.set(&selector, true))?;
    check_eq(&(out.get(&rt)?), &(60))?;
    Ok(())
}
#[test]
fn rebuilding_branches_reclaims_slots_and_preserves_other_readers() -> TestResult {
    let mut rt = Runtime::new();
    let revision = rt.input(0_u64)?;
    let input = rt.input(3_u64)?;
    let shared = input.cell();
    let branch = rt.switch_map(&revision.cell(), move |builder, v| {
        let offset = *v;
        let mut tail = shared.clone();
        for _ in 0..8 {
            tail = builder.map(&tail, move |n| n.wrapping_add(offset))?;
        }
        Ok(tail)
    })?;
    let other = rt.map(&input.cell(), |v| v.wrapping_mul(100))?;
    let observed = rt.observe(&branch)?;
    let stable = rt.observe(&other)?;
    rt.stabilize()?;
    let baseline = rt.stats().live_nodes;
    for i in 1_u64..=1000 {
        rt.transaction(|tx| {
            tx.set(&revision, i)?;
            tx.set(&input, i)
        })?;
        check_eq(&(observed.get(&rt)?), &(i.wrapping_mul(9)))?;
        check_eq(&(stable.get(&rt)?), &(i.wrapping_mul(100)))?;
        check_eq(&(rt.stats().live_nodes), &(baseline))?;
    }
    check_eq(&(rt.stats().live_scopes), &(1))?;
    check(rt.stats().reclaimed_nodes >= 8000)?;
    Ok(())
}
#[test]
fn escaped_branch_handles_are_invalidated_without_breaking_current_output() -> TestResult {
    let mut rt = Runtime::new();
    let revision = rt.input(0_u64)?;
    let escaped = Arc::new(Mutex::new(None::<Cell<u64>>));
    let saved = escaped.clone();
    let switched = rt.switch_map(&revision.cell(), move |builder, v| {
        let cell = builder.constant(*v)?;
        *saved.lock() = Some(cell.clone());
        Ok(cell)
    })?;
    let current = rt.observe(&switched)?;
    rt.stabilize()?;
    let first = escaped.lock().clone().ok_or(Error::Unsettled)?;
    let old = rt.observe(&first)?;
    rt.stabilize()?;
    rt.transaction(|tx| tx.set(&revision, 1))?;
    check_eq(&(old.get(&rt)), &(Err(Error::InvalidScope)))?;
    check_eq(&(current.get(&rt)?), &(1))?;
    Ok(())
}
#[test]
fn nested_dynamic_scopes_survive_both_inner_and_outer_switches() -> TestResult {
    let mut rt = Runtime::new();
    let outer = rt.input(0_u64)?;
    let inner = rt.input(0_u64)?;
    let source = rt.input(1_u64)?;
    let iv = inner.cell();
    let src = source.cell();
    let branch = rt.switch_map(&outer.cell(), move |b, o| {
        let src = src.clone();
        let o = *o;
        b.switch_map(&iv, move |b, i| {
            let add = o.wrapping_add(*i);
            b.map(&src, move |v| v.wrapping_add(add))
        })
    })?;
    let observed = rt.observe(&branch)?;
    rt.stabilize()?;
    let count = rt.stats().live_nodes;
    for i in 1_u64..=100 {
        rt.transaction(|tx| {
            tx.set(&outer, i)?;
            tx.set(&inner, i.wrapping_mul(2))?;
            tx.set(&source, i)
        })?;
        check_eq(&(observed.get(&rt)?), &(i.wrapping_mul(4)))?;
        check_eq(&(rt.stats().live_nodes), &(count))?;
    }
    check_eq(&(rt.stats().live_scopes), &(2))?;
    Ok(())
}
#[test]
fn scope_retirement_invalidates_escaped_dependents() -> TestResult {
    let mut rt = Runtime::new();
    let revision = rt.input(0_u64)?;
    let escaped = Arc::new(Mutex::new(None::<Cell<u64>>));
    let saved = escaped.clone();
    let switched = rt.switch_map(&revision.cell(), move |b, v| {
        let cell = b.constant(*v)?;
        *saved.lock() = Some(cell.clone());
        Ok(cell)
    })?;
    let active = rt.observe(&switched)?;
    rt.stabilize()?;
    let cell = escaped.lock().clone().ok_or(Error::Unsettled)?;
    let tail = rt.map(&cell, |v| v.wrapping_add(1))?;
    let obsolete = rt.observe(&tail)?;
    rt.stabilize()?;
    rt.transaction(|tx| tx.set(&revision, 1))?;
    check_eq(&(obsolete.get(&rt)), &(Err(Error::InvalidScope)))?;
    check_eq(&(active.get(&rt)?), &(1))?;
    Ok(())
}
#[test]
fn feedback_waits_for_next_explicit_transaction() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_u64)?;
    let target = rt.input(0_u64)?;
    let write = target.clone();
    let _subscription = rt.subscribe(&a.cell(), move |v, next| {
        next.set(&write, v.wrapping_add(10))
    })?;
    let out = rt.observe(&target.cell())?;
    rt.stabilize()?;
    check_eq(&(out.get(&rt)?), &(0))?;
    check(rt.has_feedback())?;
    rt.stabilize()?;
    check_eq(&(out.get(&rt)?), &(10))?;
    rt.transaction(|tx| tx.set(&a, 2))?;
    check_eq(&(out.get(&rt)?), &(10))?;
    rt.stabilize()?;
    check_eq(&(out.get(&rt)?), &(12))?;
    Ok(())
}
#[test]
fn aborted_transaction_does_not_apply_staged_writes() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(1_u64)?;
    let out = rt.observe(&a.cell())?;
    rt.stabilize()?;
    check_eq(
        &(rt.transaction(|tx| {
            tx.set(&a, 2)?;
            Err(Error::Busy)
        })),
        &(Err(Error::Busy)),
    )?;
    check_eq(&(out.get(&rt)?), &(1))?;
    rt.transaction(|tx| tx.set(&a, 3))?;
    check_eq(&(out.get(&rt)?), &(3))?;
    Ok(())
}
#[test]
fn observer_drop_releases_demand_and_graph_storage() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(1_u64)?;
    let b = rt.map(&a.cell(), |v| v.wrapping_add(1))?;
    let out = rt.observe(&b)?;
    let clone = out.clone();
    rt.stabilize()?;
    drop(out);
    rt.transaction(|tx| tx.set(&a, 2))?;
    check_eq(&(clone.get(&rt)?), &(3))?;
    drop(clone);
    drop(b);
    rt.collect()?;
    check_eq(&(rt.stats().live_nodes), &(1))?;
    drop(a);
    rt.collect()?;
    check_eq(&(rt.stats().live_nodes), &(0))?;
    Ok(())
}
#[test]
fn deep_graph_uses_an_explicit_stack_and_reclaims_without_recursion() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_u64)?;
    let mut tail = a.cell();
    for _ in 0..10_000 {
        tail = rt.map(&tail, |v| v.wrapping_add(1))?;
    }
    let out = rt.observe(&tail)?;
    rt.stabilize()?;
    check_eq(&(out.get(&rt)?), &(10_000))?;
    rt.transaction(|tx| tx.set(&a, 1))?;
    check_eq(&(out.get(&rt)?), &(10_001))?;
    drop(out);
    drop(tail);
    rt.collect()?;
    check_eq(&(rt.stats().live_nodes), &(1))?;
    Ok(())
}
#[test]
fn identity_distinguishes_occurrences_with_equal_payloads() -> TestResult {
    #[derive(Clone, Eq, PartialEq)]
    struct Occurrence {
        sequence: u64,
        payload: u64,
    }
    let mut rt = Runtime::new();
    let input = rt.input(None::<Occurrence>)?;
    let count = rt.scan(&input.cell(), 0_u64, |n, event| {
        if event.is_some() {
            n.wrapping_add(1)
        } else {
            *n
        }
    })?;
    let out = rt.observe(&count)?;
    rt.stabilize()?;
    rt.transaction(|tx| {
        tx.set(
            &input,
            Some(Occurrence {
                sequence: 1,
                payload: 7,
            }),
        )?;
        tx.set(
            &input,
            Some(Occurrence {
                sequence: 2,
                payload: 7,
            }),
        )
    })?;
    check_eq(&(out.get(&rt)?), &(2))?;
    Ok(())
}
#[test]
fn foreign_handles_are_rejected_at_construction_and_commit() -> TestResult {
    let mut a = Runtime::new();
    let mut b = Runtime::new();
    let input = b.input(1_u64)?;
    check(matches!(
        a.map(&input.cell(), |v| *v),
        Err(Error::ForeignRuntime)
    ))?;
    check_eq(
        &(a.transaction(|tx| tx.set(&input, 2))),
        &(Err(Error::ForeignRuntime)),
    )?;
    Ok(())
}
#[test]
fn dynamic_cycles_fail_before_observation_and_poison_the_runtime() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(0_u64)?;
    let recursive = Arc::new(Mutex::new(None::<Cell<u64>>));
    let read = recursive.clone();
    let switched = rt.switch_map(&selector.cell(), move |b, _| {
        b.map(&read.lock().clone().ok_or(Error::Unsettled)?, |v| *v)
    })?;
    *recursive.lock() = Some(switched.clone());
    let _out = rt.observe(&switched)?;
    check_eq(&(rt.stabilize()), &(Err(Error::Cycle)))?;
    check_eq(&(rt.stabilize()), &(Err(Error::Poisoned)))?;
    Ok(())
}
#[test]
fn panicking_computation_poisoning_is_explicit() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0_u64)?;
    let mapped = rt.map(&input.cell(), |_| intentional_failure())?;
    let _out = rt.observe(&mapped)?;
    check(catch_unwind(AssertUnwindSafe(|| rt.stabilize())).is_err())?;
    check_eq(&(rt.stabilize()), &(Err(Error::Poisoned)))?;
    Ok(())
}
#[test]
fn runtime_and_handles_are_send_without_requiring_sync_values() -> TestResult {
    fn is_send<T: Send>() {}
    is_send::<Runtime>();
    is_send::<Cell<u64>>();
    let mut rt = Runtime::new();
    let input = rt.input(std::cell::Cell::new(1_u64))?;
    let out = rt.map(&input.cell(), std::cell::Cell::get)?;
    let obs = rt.observe(&out)?;
    let joined = std::thread::spawn(move || {
        rt.stabilize()?;
        obs.get(&rt)
    })
    .join()
    .map_err(|_| Error::Poisoned)??;
    check_eq(&(joined), &(1))?;
    Ok(())
}

#[test]
fn large_unequal_burst_preserves_every_value() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0_u64)?;
    let sum = rt.scan(&input.cell(), 0_u64, |sum, value| sum.wrapping_add(*value))?;
    let out = rt.observe(&sum)?;
    rt.stabilize()?;
    rt.transaction(|tx| {
        for value in 1..=20_000 {
            tx.set(&input, value)?;
        }
        Ok(())
    })?;
    check_eq(&(out.get(&rt)?), &(200_010_000))?;
    Ok(())
}
#[test]
fn dynamic_scope_can_own_stateful_operators() -> TestResult {
    let mut rt = Runtime::new();
    let revision = rt.input(0_u64)?;
    let input = rt.input(1_u64)?;
    let source = input.cell();
    let switched = rt.switch_map(&revision.cell(), move |b, _| {
        b.scan(&source, 0_u64, |sum, v| sum.wrapping_add(*v))
    })?;
    let out = rt.observe(&switched)?;
    rt.stabilize()?;
    check_eq(&(out.get(&rt)?), &(1))?;
    rt.transaction(|tx| tx.set(&input, 2))?;
    check_eq(&(out.get(&rt)?), &(3))?;
    rt.transaction(|tx| tx.set(&revision, 1))?;
    check_eq(&(out.get(&rt)?), &(2))?;
    Ok(())
}
#[test]
fn cycle_through_a_previously_cached_branch_fails_before_effects() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(false)?;
    let source = rt.input(1_u64)?;
    let option = Arc::new(Mutex::new(None::<Cell<u64>>));
    let read = option.clone();
    let initial = source.cell();
    let switched = rt.switch_map(&selector.cell(), move |_, flag| {
        if *flag {
            read.lock().clone().ok_or(Error::Unsettled)
        } else {
            Ok(initial.clone())
        }
    })?;
    let later = rt.map(&switched, |v| v.wrapping_add(1))?;
    *option.lock() = Some(later.clone());
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let _observer = rt.subscribe(&later, move |_, _| {
        count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })?;
    rt.stabilize()?;
    let before = seen.load(Ordering::Relaxed);
    check_eq(
        &(rt.transaction(|tx| tx.set(&selector, true))),
        &(Err(Error::Cycle)),
    )?;
    check_eq(&(seen.load(Ordering::Relaxed)), &(before))?;
    Ok(())
}
#[test]
fn generated_dynamic_dag_matches_full_recomputation() -> TestResult {
    enum Expr {
        Sum(usize, usize),
        Select(usize, usize, usize),
    }
    let mut rt = Runtime::new();
    let roots: Vec<_> = (0_u64..5).map(|v| rt.input(v)).collect::<Result<_, _>>()?;
    let mut cells: Vec<_> = roots.iter().map(super::Input::cell).collect();
    let mut expressions = Vec::new();
    let mut seed = 7_u64;
    let mut pick = |len: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        usize::try_from(
            seed.checked_rem(u64::try_from(len).unwrap_or(u64::MAX))
                .unwrap_or(0),
        )
        .unwrap_or(0)
    };
    for n in 0_usize..1000 {
        let a = pick(cells.len());
        let b = pick(cells.len());
        let c = pick(cells.len());
        let ac = cells.get(a).ok_or(Error::InvalidHandle)?;
        let bc = cells.get(b).ok_or(Error::InvalidHandle)?;
        let node = if n.is_multiple_of(7) {
            let left = ac.clone();
            let right = bc.clone();
            let selector = cells.get(c).ok_or(Error::InvalidHandle)?;
            expressions.push(Expr::Select(c, a, b));
            rt.switch_map(selector, move |_, v| {
                Ok(if v.is_multiple_of(2) {
                    left.clone()
                } else {
                    right.clone()
                })
            })?
        } else {
            expressions.push(Expr::Sum(a, b));
            rt.map2(ac, bc, |a, b| a.wrapping_add(*b))?
        };
        cells.push(node);
    }
    let observations: Vec<_> = cells
        .iter()
        .enumerate()
        .filter(|(i, _)| i.is_multiple_of(17))
        .map(|(i, c)| Ok((i, rt.observe(c)?)))
        .collect::<Result<_, Error>>()?;
    let mut values: Vec<_> = (0_u64..5).collect();
    for step in 0_u64..200 {
        for (value, index) in values.iter_mut().zip(0_u64..) {
            *value = step.wrapping_add(index);
        }
        rt.transaction(|tx| {
            for (input, value) in roots.iter().zip(&values) {
                tx.set(input, *value)?;
            }
            Ok(())
        })?;
        let mut oracle = values.clone();
        for expr in &expressions {
            let value = match *expr {
                Expr::Sum(a, b) => oracle
                    .get(a)
                    .ok_or(Error::InvalidHandle)?
                    .wrapping_add(*oracle.get(b).ok_or(Error::InvalidHandle)?),
                Expr::Select(c, a, b) => *oracle
                    .get(
                        if oracle.get(c).ok_or(Error::InvalidHandle)?.is_multiple_of(2) {
                            a
                        } else {
                            b
                        },
                    )
                    .ok_or(Error::InvalidHandle)?,
            };
            oracle.push(value);
        }
        for (index, out) in &observations {
            check_eq(
                &(out.get(&rt)?),
                oracle.get(*index).ok_or(Error::InvalidHandle)?,
            )?;
        }
    }
    Ok(())
}

#[test]
fn switching_away_from_the_selector_itself_keeps_the_selector_dependency() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(0_u64)?;
    let source = selector.cell();
    let fallback = rt.constant(100_u64)?;
    let switched = rt.switch_map(&selector.cell(), move |_, v| {
        Ok(if v.is_multiple_of(2) {
            source.clone()
        } else {
            fallback.clone()
        })
    })?;
    let out = rt.observe(&switched)?;
    rt.stabilize()?;
    for value in 1..=100 {
        rt.transaction(|tx| tx.set(&selector, value))?;
        check_eq(
            &(out.get(&rt)?),
            &(if value.is_multiple_of(2) { value } else { 100 }),
        )?;
    }
    Ok(())
}

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
fn check(condition: bool) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other("test condition failed").into())
    }
}
fn check_eq<T: PartialEq + std::fmt::Debug>(actual: &T, expected: &T) -> TestResult {
    if actual == expected {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("actual: {actual:?}; expected: {expected:?}")).into())
    }
}
fn intentional_failure() -> u64 {
    assert!(std::hint::black_box(false), "intentional callback failure");
    0
}

#[test]
fn equal_switch_outputs_do_not_recompute_downstream() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(false)?;
    let switched = rt.switch_map(&selector.cell(), |b, _| b.constant(7_u64))?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let tail = rt.map(&switched, move |v| {
        count.fetch_add(1, Ordering::Relaxed);
        *v
    })?;
    let out = rt.observe(&tail)?;
    rt.stabilize()?;
    rt.transaction(|tx| tx.set(&selector, true))?;
    check_eq(&calls.load(Ordering::Relaxed), &1)?;
    check_eq(&out.get(&rt)?, &7)?;
    Ok(())
}
#[test]
fn dropping_the_switch_owner_retires_its_branch_scope() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(false)?;
    let switched = rt.switch_map(&selector.cell(), |b, _| b.constant(7_u64))?;
    let out = rt.observe(&switched)?;
    rt.stabilize()?;
    check_eq(&rt.stats().live_scopes, &1)?;
    drop(out);
    drop(switched);
    rt.collect()?;
    check_eq(&rt.stats().live_scopes, &0)?;
    check_eq(&rt.stats().live_nodes, &1)?;
    Ok(())
}

#[test]
fn deeply_nested_switch_retirement_uses_an_explicit_stack() -> TestResult {
    fn nested(
        b: &mut super::Builder<'_>,
        selector: &Cell<bool>,
        source: &Cell<u64>,
        remaining: usize,
    ) -> Result<Cell<u64>, Error> {
        if remaining == 0 {
            return b.map(source, |v| v.wrapping_add(1));
        }
        let next = selector.clone();
        let source = source.clone();
        b.switch_map(selector, move |b, _| {
            nested(b, &next, &source, remaining.saturating_sub(1))
        })
    }
    let mut rt = Runtime::new();
    let selector = rt.input(false)?;
    let source = rt.input(3_u64)?;
    let choice = selector.cell();
    let value = source.cell();
    let switched = rt.switch_map(&selector.cell(), move |b, _| {
        nested(b, &choice, &value, 1500)
    })?;
    let out = rt.observe(&switched)?;
    rt.stabilize()?;
    let live = rt.stats().live_nodes;
    rt.transaction(|tx| tx.set(&selector, true))?;
    check_eq(&out.get(&rt)?, &4)?;
    check_eq(&rt.stats().live_nodes, &live)?;
    drop(out);
    drop(switched);
    rt.collect()?;
    check_eq(&rt.stats().live_scopes, &0)?;
    Ok(())
}
