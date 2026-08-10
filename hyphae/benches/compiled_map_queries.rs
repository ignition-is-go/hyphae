//! Before/after benchmarks for the statically compiled `MapQuery` engine.
//!
//! This harness deliberately uses application-shaped, fixed static plans so
//! the current recursive installer and the future compiled runtime perform the
//! same logical work. Keep revision-specific API adapters mechanical when the
//! v3-breaking semantic operator names land.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    CellMap, MapQuery,
    traits::{LeftJoinExt, MapEntriesExt, MapValuesExt, SelectExt},
};

const ROWS: u64 = 1_000;

#[derive(Clone, Debug, PartialEq)]
struct Row {
    relation: u64,
    payload: u64,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct Dimension {
    relation: u64,
    payload: u64,
}

fn source_rows() -> CellMap<u64, Arc<Row>> {
    let source = CellMap::new();
    for key in 0..ROWS {
        source.insert(
            key,
            Arc::new(Row {
                relation: key % 64,
                payload: key.saturating_mul(17),
                generation: 0,
            }),
        );
    }
    source
}

fn dimensions(salt: u64) -> CellMap<u64, Arc<Dimension>> {
    let source = CellMap::new();
    for key in 0..ROWS {
        source.insert(
            key,
            Arc::new(Dimension {
                relation: key % 64,
                payload: key.saturating_mul(31).wrapping_add(salt),
            }),
        );
    }
    source
}

fn updated_row(key: u64, generation: u64) -> Arc<Row> {
    Arc::new(Row {
        relation: key % 64,
        payload: key
            .saturating_mul(17)
            .wrapping_add(generation.saturating_mul(2)),
        generation,
    })
}

fn updated_dimension(key: u64, generation: u64) -> Arc<Dimension> {
    Arc::new(Dimension {
        relation: key % 64,
        payload: key.saturating_mul(31).wrapping_add(generation),
    })
}

fn fold_matches(row: &Row, matches: &[Arc<Dimension>], salt: u64) -> Arc<Row> {
    let payload = matches.iter().fold(row.payload, |acc, dimension| {
        acc.rotate_left(5) ^ dimension.payload.wrapping_add(salt)
    });
    Arc::new(Row {
        relation: row.relation,
        payload,
        generation: row.generation,
    })
}

fn fold_indexed_matches(row: &Row, matches: &[(u64, Arc<Dimension>)], salt: u64) -> Arc<Row> {
    let payload = matches.iter().fold(row.payload, |acc, (_, dimension)| {
        acc.rotate_left(5) ^ dimension.payload.wrapping_add(salt)
    });
    Arc::new(Row {
        relation: row.relation,
        payload,
        generation: row.generation,
    })
}

fn bench_projection_region(c: &mut Criterion) {
    let source = source_rows();
    let output = source
        .clone()
        .select(|row| row.payload % 2 == 0)
        .map_values(|_key, row| {
            Arc::new(Row {
                relation: row.relation,
                payload: row.payload.rotate_left(7),
                generation: row.generation,
            })
        })
        .select(|row| row.relation < 64)
        .map_values(|_key, row| {
            Arc::new(Row {
                relation: row.relation,
                payload: row.payload.wrapping_mul(33),
                generation: row.generation,
            })
        })
        .materialize();

    let mut generation = 0_u64;
    c.bench_function("compiled_query/projection_region/single", |b| {
        b.iter(|| {
            generation = generation.wrapping_add(1);
            source.insert(0, updated_row(0, black_box(generation)));
            black_box(output.get_value(&0));
        });
    });
}

fn bench_two_join_region(c: &mut Criterion) {
    let source = source_rows();
    let first = dimensions(1);
    let second = dimensions(2);
    let output = source
        .clone()
        .left_join_by(
            first,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 1))
        .left_join_by(
            second,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 2))
        .materialize();

    let mut generation = 0_u64;
    c.bench_function("compiled_query/two_join_region/single", |b| {
        b.iter(|| {
            generation = generation.wrapping_add(1);
            source.insert(0, updated_row(0, black_box(generation)));
            black_box(output.get_value(&0));
        });
    });
}

fn bench_repeated_relation_four_join(c: &mut Criterion) {
    let source = source_rows();
    let shared_dimension = dimensions(7);
    let output = source
        .clone()
        .left_join_by(
            shared_dimension.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 1))
        .left_join_by(
            shared_dimension.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 2))
        .left_join_by(
            shared_dimension.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 3))
        .left_join_by(
            shared_dimension.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 4))
        .materialize();

    let mut generation = 0_u64;
    c.bench_function("compiled_query/repeated_relation_four_join/single", |b| {
        b.iter(|| {
            generation = generation.wrapping_add(1);
            source.insert(0, updated_row(0, black_box(generation)));
            black_box(output.get_value(&0));
        });
    });

    let mut right_generation = 0_u64;
    c.bench_function(
        "compiled_query/repeated_relation_four_join/repeated_right_single",
        |b| {
            b.iter(|| {
                right_generation = right_generation.wrapping_add(1);
                shared_dimension.insert(0, updated_dimension(0, black_box(right_generation)));
                black_box(output.get_value(&0));
            });
        },
    );
}

fn bench_rekey_between_joins(c: &mut Criterion) {
    let source = source_rows();
    let first = dimensions(11);
    let second = dimensions(13);
    let output = source
        .clone()
        .left_join_by(
            first,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_entries(|key, (row, matches)| (key.wrapping_add(ROWS), fold_matches(row, matches, 11)))
        .left_join_by(
            second,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 13))
        .materialize();

    let mut generation = 0_u64;
    c.bench_function("compiled_query/rekey_between_joins/single", |b| {
        b.iter(|| {
            generation = generation.wrapping_add(1);
            source.insert(0, updated_row(0, black_box(generation)));
            black_box(output.get_value(&ROWS));
        });
    });
}

fn bench_two_join_batches(c: &mut Criterion) {
    let mut group = c.benchmark_group("compiled_query/two_join_region/batch");
    for batch_size in [1_u64, 10, 100, 1_000, 10_000] {
        let source = source_rows();
        let first = dimensions(17);
        let second = dimensions(19);
        let output = source
            .clone()
            .left_join_by(
                first,
                |_key, row| row.relation,
                |_key, dimension| dimension.relation,
            )
            .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 17))
            .left_join_by(
                second,
                |_key, row| row.relation,
                |_key, dimension| dimension.relation,
            )
            .map_joined_values(|_key, row, matches| fold_indexed_matches(row, matches, 19))
            .materialize();

        let mut generation = 0_u64;
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &size| {
                b.iter(|| {
                    generation = generation.wrapping_add(1);
                    let changes = (0..size)
                        .map(|key| (key, updated_row(key, generation)))
                        .collect();
                    source.insert_many(changes);
                    black_box(output.get_value(&0));
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_projection_region,
    bench_two_join_region,
    bench_repeated_relation_four_join,
    bench_rekey_between_joins,
    bench_two_join_batches,
);
criterion_main!(benches);
