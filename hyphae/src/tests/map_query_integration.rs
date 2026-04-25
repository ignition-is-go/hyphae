//! Integration tests for MapQuery type.

use crate::{CellMap, MapQuery, traits::CellValue, traits::InnerJoinExt};

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

#[test]
fn inner_join_plan_materializes_to_joined_cell_map() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);

    let mat = l.clone().inner_join(r.clone()).materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some((1, 10)));
    assert_eq!(mat.get_value(&"b".to_string()), None);

    r.insert("b".into(), 20);
    assert_eq!(mat.get_value(&"b".to_string()), Some((2, 20)));
}

#[test]
fn inner_join_chain_installs_one_subscription_per_root() {
    use crate::traits::DepNode;

    let a = CellMap::<String, i32>::new();
    let b = CellMap::<String, i32>::new();
    let c = CellMap::<String, i32>::new();
    a.insert("k".into(), 1);
    b.insert("k".into(), 2);
    c.insert("k".into(), 3);

    let initial_a = DepNode::subscriber_count(&a.inner.diffs_cell);
    let initial_b = DepNode::subscriber_count(&b.inner.diffs_cell);
    let initial_c = DepNode::subscriber_count(&c.inner.diffs_cell);

    let mat = a
        .clone()
        .inner_join(b.clone())
        .inner_join(c.clone())
        .materialize();

    assert_eq!(
        DepNode::subscriber_count(&a.inner.diffs_cell),
        initial_a + 1
    );
    assert_eq!(
        DepNode::subscriber_count(&b.inner.diffs_cell),
        initial_b + 1
    );
    assert_eq!(
        DepNode::subscriber_count(&c.inner.diffs_cell),
        initial_c + 1
    );

    a.insert("k".into(), 99);
    assert_eq!(mat.get_value(&"k".to_string()), Some(((99, 2), 3)));
}

use crate::traits::LeftJoinExt;

#[test]
fn left_join_plan_keeps_unmatched_left() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);

    let mat = l.clone().left_join(r.clone()).materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some((1, vec![10])));
    assert_eq!(mat.get_value(&"b".to_string()), Some((2, vec![])));
}

use crate::traits::LeftSemiJoinExt;

#[test]
fn left_semi_join_plan_keeps_left_with_match() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);

    let mat = l.clone().left_semi_join(r.clone()).materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some(1));
    assert_eq!(mat.get_value(&"b".to_string()), None);
}

use crate::traits::ProjectMapExt;

#[test]
fn project_plan_filters_and_transforms() {
    let src = CellMap::<String, i32>::new();
    src.insert("a".into(), 5);

    let mat = src
        .clone()
        .project(|k, v| Some((format!("p:{k}"), v * 10)))
        .materialize();
    assert_eq!(mat.get_value(&"p:a".to_string()), Some(50));

    src.insert("b".into(), 7);
    assert_eq!(mat.get_value(&"p:b".to_string()), Some(70));
}

use crate::traits::ProjectManyExt;

#[test]
fn project_many_plan_emits_multiple_rows_per_source() {
    let src = CellMap::<String, i32>::new();
    src.insert("x".into(), 2);

    let mat = src
        .clone()
        .project_many(|k, v| {
            vec![
                (format!("a:{k}"), v * 10),
                (format!("b:{k}"), v * 100),
            ]
        })
        .materialize();
    assert_eq!(mat.get_value(&"a:x".to_string()), Some(20));
    assert_eq!(mat.get_value(&"b:x".to_string()), Some(200));
}

use crate::traits::MultiLeftJoinExt;

#[test]
fn multi_left_join_plan_collects_matches_per_key() {
    let l = CellMap::<String, Vec<String>>::new();
    let r = CellMap::<String, (String, i32)>::new();
    l.insert("l1".into(), vec!["g1".into(), "g2".into()]);
    r.insert("r1".into(), ("g1".into(), 10));
    r.insert("r2".into(), ("g2".into(), 20));

    let mat = l
        .clone()
        .multi_left_join_by(r.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
        .materialize();
    let (_, right_vals) = mat.get_value(&"l1".to_string()).unwrap();
    assert_eq!(right_vals.len(), 2);
}

use crate::traits::ProjectCellExt;

#[test]
fn project_cell_plan_reacts_to_inner_pipeline_emissions() {
    use crate::{MapExt, pipeline::Pipeline};

    let src = CellMap::<String, i32>::new();
    let weights = CellMap::<String, i32>::new();
    weights.insert("a".to_string(), 1);
    weights.insert("b".to_string(), 1);

    src.insert("a".to_string(), 10);
    src.insert("b".to_string(), 20);

    let weights_for_mapper = weights.clone();
    let mat = src
        .clone()
        .project_cell(move |key, value| {
            // For each row, derive a pipeline that pulls the weight from
            // weights and produces Option<(String, i32)>.
            let key = key.clone();
            let value = *value;
            let weights_inner = weights_for_mapper.clone();
            weights_inner
                .get(&key)
                .map(move |w| Some((key.clone(), value * w.unwrap_or(0))))
                .materialize()
        })
        .materialize();

    assert_eq!(mat.get_value(&"a".to_string()), Some(10));

    // Source row update flows through the per-row Watchable.
    src.insert("a".to_string(), 99);
    assert_eq!(mat.get_value(&"a".to_string()), Some(99));

    // Inner cell update also flows through.
    weights.insert("a".to_string(), 3);
    assert_eq!(mat.get_value(&"a".to_string()), Some(297));

    // Source row removal drops the output row.
    src.remove(&"a".to_string());
    assert_eq!(mat.get_value(&"a".to_string()), None);
}
