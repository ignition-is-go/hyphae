//! Settle counters appended once a second to `$HYPHAE_SETTLE_STATS_OUT.<pid>`
//! for the load harness, and the owned subscriptions that could not be bound
//! to `<that path>.unbound`.

use std::{
    io::Write as _,
    sync::{Once, atomic::Ordering},
};

use super::{COMPLETED, INBOX, INTERVAL_MAX_FIRE_NANOS, SEEDED, stats};

static STARTED: Once = Once::new();

/// Start the dump thread the first time a ticket is seeded, when asked for.
pub(super) fn start() {
    STARTED.call_once(|| {
        let Ok(path) = std::env::var("HYPHAE_SETTLE_STATS_OUT") else {
            return;
        };
        let path = format!("{path}.{}", std::process::id());
        let spawned = std::thread::Builder::new()
            .name("hyphae-settle-stats".to_owned())
            .spawn(move || run(&path));
        if let Err(error) = spawned {
            log::error!("hyphae: cannot start the settle stats thread: {error}");
        }
    });
}

fn run(path: &str) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let s = stats();
        let hold = INTERVAL_MAX_FIRE_NANOS.swap(0, Ordering::Relaxed);
        let in_flight = SEEDED
            .load(Ordering::Acquire)
            .saturating_sub(COMPLETED.load(Ordering::Acquire));
        let _ = writeln!(
            file,
            "{now:.3} event_sets={} tickets={} fires={} node_fires={} pool_fires={} waits={} deferred_writes={} fire_nanos={} flush_sets={} max_in_flight={} in_flight={in_flight} backlog={} interval_max_fire_nanos={hold} label_queries={} frontier_scanned={} max_frontier={} index_rebuilds={} rebuild_overflows={} rebuild_dead={} rebuild_nanos={} relabels={} relabeled_slots={} reorders={} reordered_nodes={} parks={} side_reads={} undeclared_writes={} unbound_owned={} cycles={}",
            s.event_sets,
            s.tickets,
            s.fires,
            s.node_fires,
            s.pool_fires,
            s.waits,
            s.deferred_writes,
            s.fire_nanos,
            s.flush_sets,
            s.max_in_flight,
            INBOX.len().saturating_add(usize::try_from(in_flight).unwrap_or(usize::MAX)),
            s.label_queries,
            s.frontier_scanned,
            s.max_frontier,
            s.index_rebuilds,
            s.rebuild_overflows,
            s.rebuild_dead,
            s.rebuild_nanos,
            s.relabels,
            s.relabeled_slots,
            s.reorders,
            s.reordered_nodes,
            s.parks,
            s.side_reads,
            s.undeclared_writes,
            s.unbound_owned,
            s.cycles,
        );
        let cycles = super::reach::cycle_sites();
        if !cycles.is_empty()
            && let Ok(mut out) = std::fs::File::create(format!("{path}.cycles"))
        {
            for ((from, to), count) in cycles {
                let _ = writeln!(out, "{count} {from} -> {to}");
            }
        }
        let conflicts = super::reach::conflict_sites();
        if !conflicts.is_empty()
            && let Ok(mut out) = std::fs::File::create(format!("{path}.conflicts"))
        {
            for ((from, to), count) in conflicts {
                let _ = writeln!(out, "{count} {from} -> {to}");
            }
        }
        let sites = super::edges::unbound_sites();
        if !sites.is_empty()
            && let Ok(mut unbound) = std::fs::File::create(format!("{path}.unbound"))
        {
            for (site, count) in sites {
                let _ = writeln!(unbound, "{count} {site}");
            }
        }
    }
}
