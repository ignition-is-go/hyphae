//! Write hooks run inside an open batch, while diff delivery is deferred.
//! An integration test because `batch` is process-wide.

use std::sync::Arc;

use hyphae::{CellMap, batch, cell_map::MapWrite};
use parking_lot::Mutex;

#[test]
fn hooks_report_inside_an_open_batch_while_diffs_wait() {
    let map = CellMap::<u32, u32>::new();
    let indexed = Arc::new(Mutex::new(Vec::new()));
    let diffs = Arc::new(Mutex::new(0_usize));
    let log = indexed.clone();
    let _hook = map.register_write_hook(move |write| {
        let entry = match write {
            MapWrite::Insert { key, value } => (*key, Some(*value)),
            MapWrite::Update { key, new, .. } => (*key, Some(*new)),
            MapWrite::Remove { key, .. } => (*key, None),
        };
        log.lock().push(entry);
    });
    let seen = diffs.clone();
    let _diffs = map.subscribe_diffs(move |_| *seen.lock() += 1);
    let delivered_before = *diffs.lock();
    batch(|| {
        map.insert(1, 10);
        assert_eq!(*indexed.lock(), [(1, Some(10))]);
        map.insert(1, 11);
        map.remove(&1);
        assert_eq!(*indexed.lock(), [(1, Some(10)), (1, Some(11)), (1, None)]);
        assert_eq!(
            *diffs.lock(),
            delivered_before,
            "diff delivery is still deferred"
        );
    });
    assert_eq!(*diffs.lock(), delivered_before + 3);
}
