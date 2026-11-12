use super::*;

#[test]
fn transaction_settles_diamond_once_and_cuts_off_equal_inputs() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(1_i64);
    let b = rt.input(2_i64);
    let calls = Rc::new(std::cell::Cell::new(0_usize));
    let counter = calls.clone();
    let left = a.cell().map(|v| v.saturating_mul(2));
    let right = a.cell().map(|v| v.saturating_mul(3));
    let diamond = left.map2(&right, |l, r| l.saturating_add(*r))?;
    let result = diamond.map2(&b.cell(), move |a, b| {
        counter.set(counter.get().saturating_add(1));
        a.saturating_add(*b)
    })?;
    let observed = result.observe();
    check_eq(&(observed.read()), &(Err(Error::Unavailable)))?;
    rt.transaction(|_| Ok(()))?;
    check_eq(&(observed.read()?), &(7))?;
    calls.set(0);
    rt.transaction(|tx| {
        tx.set(&a, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(observed.read()?), &(70))?;
    check_eq(&(calls.get()), &(1))?;
    let before = rt.stats().recomputed;
    rt.transaction(|tx| {
        tx.set(&a, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(rt.stats().recomputed), &(before))?;
    Ok(())
}

#[test]
fn repeated_writes_preserve_a_b_a_and_distinct_occurrences() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input((0_u64, 'a'));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let output = seen.clone();
    let _subscription = rt.subscribe(&input.cell(), move |value, _| {
        output.borrow_mut().push(*value);
    })?;
    rt.transaction(|_| Ok(()))?;
    seen.borrow_mut().clear();
    rt.transaction(|tx| {
        tx.set(&input, (1, 'b'))?;
        tx.set(&input, (2, 'b'))?;
        tx.set(&input, (2, 'b'))?;
        tx.set(&input, (0, 'a'))
    })?;
    check_eq(&(*seen.borrow()), &(vec![(1, 'b'), (2, 'b'), (0, 'a')]))?;
    Ok(())
}

#[test]
fn feedback_is_next_transaction_and_callbacks_can_be_local() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_i64);
    let b = rt.input(0_i64);
    let b_for_callback = b.clone();
    let errors = Rc::new(RefCell::new(Vec::new()));
    let callback_errors = errors.clone();
    let _subscription = rt.subscribe(&a.cell(), move |v, tx| {
        if let Err(error) = tx.set(&b_for_callback, v.saturating_add(1)) {
            callback_errors.borrow_mut().push(error);
        }
    })?;
    let observed = b.cell().observe();
    rt.transaction(|tx| tx.set(&a, 5))?;
    check_eq(&(observed.read()?), &(0))?;
    rt.transaction(|_| Ok(()))?;
    check_eq(&(observed.read()?), &(6))?;
    check(errors.borrow().is_empty())?;
    Ok(())
}

#[test]
fn only_demanded_branches_compute_and_switch_releases_old_branch() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(false);
    let left = rt.input(1_i64);
    let right = rt.input(10_i64);
    let left_calls = Rc::new(std::cell::Cell::new(0_usize));
    let right_calls = Rc::new(std::cell::Cell::new(0_usize));
    let lc = left_calls.clone();
    let rc = right_calls.clone();
    let l = left.cell().map(move |v| {
        lc.set(lc.get().saturating_add(1));
        *v
    });
    let r = right.cell().map(move |v| {
        rc.set(rc.get().saturating_add(1));
        *v
    });
    let selected = selector.cell().switch_map(
        move |_, use_right| {
            if *use_right { r.clone() } else { l.clone() }
        },
    );
    let observed = selected.observe();
    rt.transaction(|_| Ok(()))?;
    check_eq(&(observed.read()?), &(1))?;
    check_eq(&(left_calls.get()), &(1))?;
    check_eq(&(right_calls.get()), &(0))?;
    rt.transaction(|tx| {
        tx.set(&selector, true)?;
        tx.set(&right, 20)
    })?;
    check_eq(&(observed.read()?), &(20))?;
    let lc = left_calls.get();
    rt.transaction(|tx| tx.set(&left, 100))?;
    check_eq(&(left_calls.get()), &(lc))?;
    drop(observed);
    let rc = right_calls.get();
    rt.transaction(|tx| tx.set(&right, 30))?;
    check_eq(&(right_calls.get()), &(rc))?;
    Ok(())
}

#[test]
fn dynamically_created_scopes_are_dropped_and_replaced() -> TestResult {
    struct Lifetime(Rc<std::cell::Cell<usize>>);
    impl Drop for Lifetime {
        fn drop(&mut self) {
            self.0.set(self.0.get().saturating_sub(1));
        }
    }
    let mut rt = Runtime::new();
    let selector = rt.input(0_i64);
    let alive = Rc::new(std::cell::Cell::new(0_usize));
    let lifetime_counter = alive.clone();
    let cell = selector.cell().switch_map(move |builder, value| {
        lifetime_counter.set(lifetime_counter.get().saturating_add(1));
        let lifetime = Lifetime(lifetime_counter.clone());
        builder.constant(*value).map(move |v| {
            let _keep = &lifetime;
            v.saturating_mul(2)
        })
    });
    let observed = cell.observe();
    for value in 0..100_i64 {
        rt.transaction(|tx| tx.set(&selector, value))?;
        check_eq(&(observed.read()?), &(value.saturating_mul(2)))?;
        check_eq(&(alive.get()), &(1))?;
    }
    drop(observed);
    drop(cell);
    rt.transaction(|_| Ok(()))?;
    check_eq(&(alive.get()), &(0))?;
    Ok(())
}

#[test]
fn subscription_drop_cancels_demand_and_delivery() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0);
    let calls = Rc::new(std::cell::Cell::new(0_usize));
    let counter = calls.clone();
    let subscription = rt.subscribe(&input.cell(), move |_, _| {
        counter.set(counter.get().saturating_add(1));
    })?;
    drop(subscription);
    rt.transaction(|tx| tx.set(&input, 1))?;
    check_eq(&(calls.get()), &(0))?;
    Ok(())
}

#[test]
fn foreign_inputs_are_rejected_without_partial_commit() -> TestResult {
    let mut rt = Runtime::new();
    let other = Runtime::new();
    let input = rt.input(0_i64);
    let foreign = other.input(0_i64);
    let observed = input.cell().observe();
    rt.transaction(|_| Ok(()))?;
    check_eq(
        &(rt.transaction(|tx| {
            tx.set(&input, 1)?;
            tx.set(&foreign, 2)
        })),
        &(Err(Error::ForeignRuntime)),
    )?;
    check_eq(&(observed.read()?), &(0))?;
    check(matches!(
        input
            .cell()
            .map2(&foreign.cell(), |a, b| a.saturating_add(*b)),
        Err(Error::ForeignRuntime)
    ))?;
    Ok(())
}

#[test]
fn explicit_thread_bridges_bound_ingress_and_publish_after_settlement() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0_i64);
    let doubled = input.cell().map(|v| v.saturating_mul(2));
    let publication = rt.publish(&doubled)?;
    let snapshot = publication.snapshot();
    let bridge = rt.input_bridge(&input, 2)?;
    let sender = bridge.sender();
    check_eq(&(snapshot.read()), &(None))?;
    let worker_snapshot = snapshot.clone();
    let worker = std::thread::spawn(move || {
        check_eq(&(sender.try_send(3)), &(Ok(())))?;
        check_eq(&(sender.try_send(4)), &(Ok(())))?;
        check(matches!(
            sender.try_send(5),
            Err(mpsc::TrySendError::Full(5))
        ))?;
        check_eq(&(worker_snapshot.read()), &(None))?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    worker.join().map_err(|_| "worker panicked")??;
    check_eq(&(rt.drain_inputs(1)?), &(1))?;
    check_eq(&(snapshot.read()), &(Some(6)))?;
    check_eq(&(rt.drain_inputs(1)?), &(1))?;
    check_eq(&(snapshot.read()), &(Some(8)))?;
    drop(publication);
    rt.transaction(|tx| tx.set(&input, 9))?;
    check_eq(&(snapshot.read()), &(Some(8)))?;
    Ok(())
}

#[test]
fn local_values_do_not_require_send_or_sync() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(Rc::new(1_i64));
    let mapped = input.cell().map(|v| Rc::new(v.saturating_add(2)));
    let observed = mapped.observe();
    rt.transaction(|_| Ok(()))?;
    check_eq(&(*observed.read()?), &(3))?;
    Ok(())
}

#[test]
fn computation_failure_poisons_runtime() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(0);
    let broken = input
        .cell()
        .map(|_| -> i64 { std::panic::resume_unwind(Box::new("failure")) });
    let _observed = broken.observe();
    check_eq(&(rt.transaction(|_| Ok(()))), &(Err(Error::Poisoned)))?;
    check_eq(
        &(rt.transaction(|tx| tx.set(&input, 1))),
        &(Err(Error::Poisoned)),
    )?;
    Ok(())
}

#[test]
fn nth_input_writes_share_a_step_independent_of_staging_order() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_i64);
    let b = rt.input(0_i64);
    let joined = a.cell().map2(&b.cell(), |a, b| (*a, *b))?;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let output = seen.clone();
    let _subscription = rt.subscribe(&joined, move |v, _| output.borrow_mut().push(*v))?;
    rt.transaction(|_| Ok(()))?;
    seen.borrow_mut().clear();
    rt.transaction(|tx| {
        tx.set(&a, 1)?;
        tx.set(&a, 2)?;
        tx.set(&b, 10)?;
        tx.set(&b, 20)
    })?;
    check_eq(&(*seen.borrow()), &(vec![(1, 10), (2, 20)]))?;
    Ok(())
}

#[test]
fn derived_equality_suppresses_downstream_work() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(1_i64);
    let parity = input.cell().map(|v| v.checked_rem(2).unwrap_or_default());
    let calls = Rc::new(std::cell::Cell::new(0_usize));
    let counter = calls.clone();
    let tail = parity.map(move |v| {
        counter.set(counter.get().saturating_add(1));
        *v
    });
    let _observed = tail.observe();
    rt.transaction(|_| Ok(()))?;
    rt.transaction(|tx| tx.set(&input, 3))?;
    check_eq(&(calls.get()), &(1))?;
    Ok(())
}

#[test]
fn foreign_dynamic_branch_and_effect_panics_poison() -> TestResult {
    let mut rt = Runtime::new();
    let other = Runtime::new();
    let foreign = other.constant(1_i64);
    let selected = rt.constant(false).switch_map(move |_, _| foreign.clone());
    let _observer = selected.observe();
    check_eq(&(rt.transaction(|_| Ok(()))), &(Err(Error::Poisoned)))?;
    let mut rt = Runtime::new();
    let cell = rt.constant(1_i64);
    let _subscription =
        rt.subscribe(&cell, |_, _| std::panic::resume_unwind(Box::new("effect")))?;
    check_eq(&(rt.transaction(|_| Ok(()))), &(Err(Error::Poisoned)))?;
    check_eq(&(rt.transaction(|_| Ok(()))), &(Err(Error::Poisoned)))?;
    Ok(())
}

type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;
fn check(condition: bool) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err("check failed".into())
    }
}
fn check_eq<T: PartialEq + fmt::Debug>(actual: &T, expected: &T) -> TestResult {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("actual {actual:?} != expected {expected:?}").into())
    }
}

#[test]
fn bridge_lifetime_disconnects_and_draining_is_fair_across_calls() -> TestResult {
    let mut rt = Runtime::new();
    let a = rt.input(0_i64);
    let b = rt.input(0_i64);
    let a_bridge = rt.input_bridge(&a, 4)?;
    let b_bridge = rt.input_bridge(&b, 4)?;
    let sa = a_bridge.sender();
    let sb = b_bridge.sender();
    for value in [1, 2, 3] {
        sa.try_send(value)?;
        sb.try_send(value)?;
    }
    let oa = a.cell().observe();
    let ob = b.cell().observe();
    check_eq(&rt.drain_inputs(1)?, &1)?;
    check_eq(&oa.read()?, &1)?;
    check_eq(&ob.read()?, &0)?;
    check_eq(&rt.drain_inputs(1)?, &1)?;
    check_eq(&ob.read()?, &1)?;
    drop(a_bridge);
    check(matches!(
        sa.try_send(4),
        Err(mpsc::TrySendError::Disconnected(4))
    ))?;
    drop(rt);
    check(matches!(
        sb.try_send(4),
        Err(mpsc::TrySendError::Disconnected(4))
    ))?;
    Ok(())
}

#[test]
fn values_need_no_debug_and_fold_publishes_one_settled_snapshot() -> TestResult {
    #[derive(Clone, Eq, PartialEq)]
    struct NoDebug(u64);
    let mut rt = Runtime::new();
    let a = rt.input(NoDebug(1));
    let b = rt.input(NoDebug(2));
    let sum = rt.fold(&[a.cell(), b.cell()], 0_u64, |sum, v| {
        sum.saturating_add(v.0)
    })?;
    let published = rt.publish(&sum)?;
    let snapshot = published.snapshot();
    rt.transaction(|tx| {
        tx.set(&a, NoDebug(10))?;
        tx.set(&b, NoDebug(20))
    })?;
    check_eq(&snapshot.read(), &Some(30))?;
    send_sync::<Snapshot<std::cell::Cell<u64>>>();
    send_sync::<InputSender<std::cell::Cell<u64>>>();
    Ok(())
}

#[test]
fn escaped_branch_observers_become_unavailable_after_switch() -> TestResult {
    let mut rt = Runtime::new();
    let selector = rt.input(0_u64);
    let escaped = Rc::new(RefCell::new(None));
    let old_scope = escaped.clone();
    let switched = selector.cell().switch_map(move |builder, value| {
        let branch = builder.constant(*value);
        if *value == 0 {
            *old_scope.borrow_mut() = Some(branch.observe());
        }
        branch
    });
    let observed = switched.observe();
    rt.transaction(|_| Ok(()))?;
    let escaped = escaped
        .borrow_mut()
        .take()
        .ok_or("missing branch observer")?;
    rt.transaction(|tx| tx.set(&selector, 1))?;
    check_eq(&observed.read()?, &1)?;
    check_eq(&escaped.read(), &Err(Error::Unavailable))?;
    Ok(())
}

fn send_sync<T: Send + Sync>() {}

#[test]
fn observability_uses_backend_labels_and_counters() -> TestResult {
    let mut rt = Runtime::new();
    let input = rt.input(1_u64);
    let cell = input
        .cell()
        .map(|v| v.saturating_add(1))
        .with_name("diagnostic-output");
    let observer = cell.observe();
    rt.transaction(|_| Ok(()))?;
    check(observer.graph_dot().contains("diagnostic-output"))?;
    check(rt.stats().recomputed > 0)?;
    Ok(())
}
