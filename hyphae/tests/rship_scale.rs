//! The rship-scale scenarios at reduced scale. Each test runs one scenario of
//! `benches/support/rship_scale` at 1/40 of load-test size and asserts its
//! correctness counts: no sink missed a write, saw a writer's writes out of
//! order, saw half of a write, saw a glitch, or ended on the wrong value.
//! Nothing here asserts a time. The timed runs are the `rship_scale` bench.

#[path = "../benches/support/rship_scale/mod.rs"]
mod rship_scale;

macro_rules! scenario {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let violations = rship_scale::violations(stringify!($name));
            assert!(violations.is_empty(), "{}: {violations:?}", stringify!($name));
        }
    )*};
}

scenario!(
    tick, probe, shared, pulse, burst, edit, churn, setup, overload, fanout, hotcone, feedback,
    writers, diamond, deep, demand, oracle,
);

/// The checks above can only be trusted if they fail when the rule is
/// broken. The self-test breaks each rule on purpose.
#[test]
fn checkers_detect_broken_rules() {
    let counts = rship_scale::selftest();
    for name in [
        "selftest.reorder",
        "selftest.torn",
        "selftest.tick_gaps",
        "selftest.tick_reorder",
        "selftest.seq_gaps",
        "selftest.seq_reorder",
    ] {
        assert!(
            counts.get(name).is_some_and(|count| *count > 0.0),
            "{name} did not fire: {counts:?}"
        );
    }
}
