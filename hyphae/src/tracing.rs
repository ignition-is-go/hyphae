use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use dashmap::DashMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CellTraceSnapshot {
    pub id: Uuid,
    pub name: Option<String>,
    pub caller: Option<String>,
    pub subscriber_count: usize,
    pub owned_count: usize,
    pub notify_count: u64,
    pub total_notify_time_ns: u64,
    pub last_notify_time_ns: u64,
    pub slowest_subscriber_ns: u64,
}

#[derive(Debug, Default)]
struct CellTraceEntry {
    name: Option<String>,
    caller: Option<String>,
    subscriber_count: usize,
    owned_count: usize,
    notify_count: u64,
    total_notify_time_ns: u64,
    last_notify_time_ns: u64,
    slowest_subscriber_ns: u64,
}

static TRACE_LOG_EVERY: OnceLock<Option<u64>> = OnceLock::new();
static TRACE_TOP_N: OnceLock<usize> = OnceLock::new();
static TRACE_RECORDS: OnceLock<DashMap<Uuid, CellTraceEntry>> = OnceLock::new();
static TRACE_NOTIFY_SEQ: AtomicU64 = AtomicU64::new(0);

fn trace_records() -> &'static DashMap<Uuid, CellTraceEntry> {
    TRACE_RECORDS.get_or_init(DashMap::new)
}

fn trace_log_every() -> Option<u64> {
    *TRACE_LOG_EVERY.get_or_init(|| {
        std::env::var("RSHIP_HYPHA_TRACE_LOG_EVERY")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
    })
}

fn trace_top_n() -> usize {
    *TRACE_TOP_N.get_or_init(|| {
        std::env::var("RSHIP_HYPHA_TRACE_TOP_N")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(20)
    })
}

pub fn register_cell(id: Uuid, caller: Option<String>) {
    trace_records().entry(id).or_insert_with(|| CellTraceEntry {
        caller,
        ..Default::default()
    });
}

pub fn deregister_cell(id: &Uuid) {
    trace_records().remove(id);
}

pub fn update_name(id: Uuid, name: String) {
    if let Some(mut entry) = trace_records().get_mut(&id) {
        entry.name = Some(name);
    }
}

pub fn update_subscriber_count(id: Uuid, subscriber_count: usize) {
    if let Some(mut entry) = trace_records().get_mut(&id) {
        entry.subscriber_count = subscriber_count;
    }
}

pub fn update_owned_count(id: Uuid, owned_count: usize) {
    if let Some(mut entry) = trace_records().get_mut(&id) {
        entry.owned_count = owned_count;
    }
}

pub fn record_notify(
    id: Uuid,
    duration_ns: u64,
    subscriber_count: usize,
    owned_count: usize,
    slowest_subscriber_ns: u64,
) {
    {
        let mut entry = trace_records().entry(id).or_default();
        entry.subscriber_count = subscriber_count;
        entry.owned_count = owned_count;
        entry.notify_count += 1;
        entry.total_notify_time_ns += duration_ns;
        entry.last_notify_time_ns = duration_ns;
        entry.slowest_subscriber_ns = entry.slowest_subscriber_ns.max(slowest_subscriber_ns);
    }

    if let Some(log_every) = trace_log_every() {
        let seq = TRACE_NOTIFY_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        if seq % log_every == 0 {
            let _ = catch_unwind(AssertUnwindSafe(|| log_hot_cells(trace_top_n())));
        }
    }
}

pub fn hot_cells(limit: usize) -> Vec<CellTraceSnapshot> {
    let mut snapshots: Vec<_> = trace_records()
        .iter()
        .map(|entry| CellTraceSnapshot {
            id: *entry.key(),
            name: entry.name.clone(),
            caller: entry.caller.clone(),
            subscriber_count: entry.subscriber_count,
            owned_count: entry.owned_count,
            notify_count: entry.notify_count,
            total_notify_time_ns: entry.total_notify_time_ns,
            last_notify_time_ns: entry.last_notify_time_ns,
            slowest_subscriber_ns: entry.slowest_subscriber_ns,
        })
        .collect();

    snapshots.sort_by(|left, right| {
        right
            .total_notify_time_ns
            .cmp(&left.total_notify_time_ns)
            .then_with(|| right.notify_count.cmp(&left.notify_count))
    });
    snapshots.truncate(limit);
    snapshots
}

pub fn log_hot_cells(limit: usize) {
    let snapshots = hot_cells(limit);
    if snapshots.is_empty() {
        log::info!("[hyphae-trace] no traced cells recorded yet");
        return;
    }

    log::info!("[hyphae-trace] top {} hot cells:", snapshots.len());
    for snapshot in snapshots {
        let name = snapshot
            .name
            .clone()
            .unwrap_or_else(|| format!("Cell({})", &snapshot.id.to_string()[..8]));
        let caller = snapshot.caller.unwrap_or_else(|| "<unknown>".to_string());
        log::info!(
            "[hyphae-trace] name={} notify_count={} total_ms={:.3} last_ms={:.3} slowest_sub_ms={:.3} subscribers={} owned={} caller={}",
            name,
            snapshot.notify_count,
            snapshot.total_notify_time_ns as f64 / 1_000_000.0,
            snapshot.last_notify_time_ns as f64 / 1_000_000.0,
            snapshot.slowest_subscriber_ns as f64 / 1_000_000.0,
            snapshot.subscriber_count,
            snapshot.owned_count,
            caller,
        );
    }
}
