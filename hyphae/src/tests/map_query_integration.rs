//! Integration tests for MapQuery type.

use crate::{CellMap, MapQuery, traits::CellValue};

#[test]
fn cell_map_is_map_query() {
    let m = CellMap::<String, i32>::new();
    m.insert("a".into(), 1);

    fn assert_query<K, V, Q: MapQuery<K, V>>(_: &Q)
    where
        K: CellValue + std::hash::Hash + Eq,
        V: CellValue,
    {
    }
    assert_query::<String, i32, _>(&m);
}

#[test]
fn materialize_plain_cell_map_mirrors_source() {
    let m = CellMap::<String, i32>::new();
    m.insert("a".into(), 10);

    let mat = m.clone().materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some(10));

    m.insert("b".into(), 20);
    assert_eq!(mat.get_value(&"b".to_string()), Some(20));
}

#[test]
fn materialize_propagates_updates_and_removes() {
    let m = CellMap::<String, i32>::new();
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);

    let mat = m.clone().materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some(1));
    assert_eq!(mat.get_value(&"b".to_string()), Some(2));

    m.insert("a".into(), 100);
    assert_eq!(mat.get_value(&"a".to_string()), Some(100));

    m.remove(&"b".into());
    assert_eq!(mat.get_value(&"b".to_string()), None);
}

#[test]
fn materialize_keeps_source_alive_via_subscription() {
    // Materializing consumes the source; the materialized cell map's owned
    // guard must keep the underlying inner alive so updates still flow.
    let m = CellMap::<String, i32>::new();
    m.insert("seed".into(), 0);

    let m_clone = m.clone();
    let mat = m_clone.materialize(); // consumes m_clone, but `m` still drives.

    m.insert("seed".into(), 42);
    assert_eq!(mat.get_value(&"seed".to_string()), Some(42));

    m.insert("new".into(), 7);
    assert_eq!(mat.get_value(&"new".to_string()), Some(7));
}
