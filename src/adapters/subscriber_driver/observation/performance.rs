use super::*;
use std::hint::black_box;
use std::time::Instant;

fn report(name: &str, values: &mut [u128]) {
    values.sort_unstable();
    println!(
        "{name}: samples={} ns p50={} p95={} p99={} max={}",
        values.len(),
        values[values.len() / 2],
        values[values.len() * 95 / 100],
        values[values.len() * 99 / 100],
        values[values.len() - 1]
    );
}

#[test]
#[ignore = "optimized observational overhead fixture; run explicitly"]
fn reconciliation_observation_overhead() {
    assert!(
        !black_box(cfg!(debug_assertions)),
        "requires optimized release profile"
    );
    const WARMUP: usize = 100;
    const SAMPLES: usize = 1000;
    const LOGS: usize = 32;
    let b = block(1);
    let reference = reference(&b, &(0..LOGS as u64).collect::<Vec<_>>());
    let records: Vec<_> = reference
        .values()
        .cloned()
        .map(|log| {
            ReactiveInputRecord::new(ReactiveInput::Log(log), context(InputSource::Subscription))
        })
        .collect();
    let interests = interests();
    let mut disabled_log = Vec::with_capacity(SAMPLES);
    let mut enabled_log = Vec::with_capacity(SAMPLES);
    let mut disabled_compare = Vec::with_capacity(SAMPLES);
    let mut enabled_compare = Vec::with_capacity(SAMPLES);
    for iteration in 0..WARMUP + SAMPLES {
        // Alternate pair order to avoid always favoring the same cache state.
        for enabled in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let mut ledger = enabled.then(Ledger::default);
            if let Some(ledger) = ledger.as_mut() {
                header(ledger, &b);
            }
            let started = Instant::now();
            for record in &records {
                if let Some(ledger) = black_box(&mut ledger).as_mut() {
                    ledger.observe(black_box(record));
                }
            }
            let per_log = started.elapsed().as_nanos() / LOGS as u128;
            let started = Instant::now();
            let result = black_box(&ledger).as_ref().map(|ledger| {
                ledger.compare(
                    black_box(&b),
                    black_box(&interests),
                    Some(black_box(&reference)),
                )
            });
            black_box(&result);
            let comparison = started.elapsed().as_nanos();
            if let Some(result) = result {
                assert_eq!(
                    (
                        result.equal_blocks(),
                        result.reference_logs(),
                        result.consumed_logs()
                    ),
                    (1, LOGS as u64, LOGS as u64)
                );
            }
            if iteration >= WARMUP {
                if enabled {
                    enabled_log.push(per_log);
                    enabled_compare.push(comparison);
                } else {
                    disabled_log.push(per_log);
                    disabled_compare.push(comparison);
                }
            }
        }
    }
    println!(
        "reconciliation_observation: warmups={WARMUP} pairs={SAMPLES} logs_per_block={LOGS} rpc_elements=0 retries=0 fallbacks=0 (local fixture; driver request-count equivalence tested separately)"
    );
    report("disabled_per_log", &mut disabled_log);
    report("enabled_per_log", &mut enabled_log);
    report("disabled_compare", &mut disabled_compare);
    report("enabled_compare_32_logs", &mut enabled_compare);
}
