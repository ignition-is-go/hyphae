# lv-7ff9 join-region threshold calibration

> Retention note: the raw captures referenced below are available from Git commit `4465c6dbe364bfee9f97136d64420549af14d800`. The current tree retains this decision record and its derived CSV inputs.

Base pushed HEAD: `f6e2b57b3dbc14648705b75bdd775c9726114432`
Commit SHA-to-be: assigned only after the required pre-commit review.

## Frozen method

The unchanged four-stage application graph costs 97 units per left row (JNil 1 + four stages × 24). Immutable sizes are `989, 990, 991, 1649, 1650, 1651`, giving costs `95,933`, `96,030`, `96,127`, `159,953`, `160,050`, and `160,147`. The original 10k fixture path and its state/trace goldens are unchanged.

Criterion warned that 50 samples could not fit the requested 5 s for 33 of 60 cases and extended collection as needed; every archived `sample.json` contains exactly 50 iterations/times. The verbatim warnings are preserved in stderr. Reported intervals below are Criterion’s pointwise (not simultaneous) slope 95% confidence intervals. Every final run was a fresh process with `HYPHAE_WORKER_THREADS=W`, W in 1/2/4, two repeats, 2 s warmup, 5 s measurement, 50 samples. The benchmark uses `iter_custom`; generation/batch creation and the required state preparation are outside the timer. Each measured operation checks its exact atomic branch delta and a settled output sentinel after stopping the timer. W1 runs only the sequential baseline. W2/W4 prepare either warmed-active (1650) or warmed-promoted-inactive (1650 then 989).

The calibration-only atomics count the actual Rayon `pool.install` dispatch and every actual left serial branch. The entire public diagnostics module and all calls/transition locals are `cfg(feature = "region-calibration")`; normal scheduler compilation has no counters, statics, loads, stores, or retained transition local.

## Branch/cost preflight

The integration truth table passed exactly:

- inactive: 989/990/991/1649 serial; 1650/1651 parallel with one inactive→parallel transition;
- active: 989 serial with one parallel→inactive transition; 990/991/1649/1650/1651 parallel;
- W1: every size serial.

Transition sizes 989, 1650, and 1651 validate branch/cost bracketing only and are not used as counterfactual timing evidence.

## Conservative counterfactual decision

Criterion slope 95% intervals (milliseconds per batch); win floor is inactive-serial lower95 / active-parallel upper95:

| workers | repeat | rows | serial 95% | parallel 95% | parallel win floor |
|---:|---:|---:|---:|---:|---:|
| 2 | 1 | 990 | 1.1969–1.2431 | 0.8120–0.8426 | 1.420x |
| 2 | 1 | 991 | 1.2460–1.3242 | 0.8328–0.8670 | 1.437x |
| 2 | 1 | 1649 | 1.9877–2.2091 | 1.2823–1.3393 | 1.484x |
| 2 | 2 | 990 | 1.1370–1.1676 | 0.8434–0.8856 | 1.284x |
| 2 | 2 | 991 | 1.1299–1.1635 | 0.8487–0.8996 | 1.256x |
| 2 | 2 | 1649 | 1.8305–1.8687 | 1.3548–1.4388 | 1.272x |
| 4 | 1 | 990 | 1.2273–1.3000 | 0.7325–0.8460 | 1.451x |
| 4 | 1 | 991 | 1.1906–1.2528 | 0.7614–0.8442 | 1.410x |
| 4 | 1 | 1649 | 2.0771–2.3111 | 1.2188–1.4517 | 1.431x |
| 4 | 2 | 990 | 1.2454–1.3326 | 0.7724–0.8398 | 1.483x |
| 4 | 2 | 991 | 1.2634–1.3152 | 0.7768–0.8393 | 1.505x |
| 4 | 2 | 1649 | 2.0758–2.1826 | 1.1278–1.2148 | 1.709x |

Every allowed comparison clears the predeclared 1.05 rule; the measured win floors span **1.256–1.709x** and the minimum is **1.256x**. Parallel execution robustly wins throughout the measured hysteresis band.

**Decision: retain `160,000 / 96,000`.** This is a calibrated conservative decision, not a failure to tune. The robust 1649 counterfactual win is real, but the enter threshold is already bracketed within one four-stage row: 1649 is 47 units below it and 1650 is 50 above. Those 47 units are only 0.029% of 160,000. Moving enter to 159,953 would gain only that single N=4 row, overfit one measured depth, extrapolate the change to regions with other stage costs, and invalidate the frozen policy truth table. The rule forbids that cross-depth extrapolation. Exit cannot move downward without an active-parallel 989 counterfactual; 989 is necessarily the measured exit transition under the frozen policy. No threshold is changed.

## Correctness and normal-path gates

- four-join 10k golden preflight: pass; goldens remain `b394c0e6dc59b647f2dc1b05314b0043` and `959cdbd3c843b5ae31f7575208113219`;
- truth-table integration preflight: pass;
- `cargo test --workspace --all-features`: pass;
- strict Clippy, workspace/all-target/all-feature: pass;
- explicit normal scheduler strict Clippy without calibration: pass;
- fmt and `git diff --check`: pass.

Normal-feature tiny-left was 3.6616–3.6949 µs and Criterion detected no change. The first repeated-typed run (4.6379–4.6683 µs) showed host variance above the historical gate; an immediate fresh-process confirmation was 4.4546–4.4882 µs, below the frozen 4.5042 µs reference and within the gate. Both outputs are retained rather than hiding the noisy run. Since production dispatch source changes are all cfg-elided in this build, the repeat confirms no normal-path instrumentation regression.

After raw capture, audit restored `update_batch` to its verbatim independent 10k implementation. The calibration helper and timed dependency slice did not change; golden/truth/full/Clippy gates were rerun. The manifest preserves separate capture and final harness hashes.

Derived CSVs remain beside this report. Verbatim stdout/stderr, Criterion JSON, the commands/environment manifest, and deterministic hashes are available at the retention commit named above.
