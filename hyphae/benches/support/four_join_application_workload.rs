//! Immutable application-shaped workload shared by the Phase 5 benchmark and preflight test.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::{Arc, Mutex};

use hyphae::{
    CellImmutable, CellMap, Gettable, MapDiff, MapQuery, pipeline::Materialize, traits::LeftJoinExt,
};

pub const APPLICATION_ROWS: u64 = 10_000;
pub const RELATION_KEYS: u64 = 2_048;
pub const MATCHES_PER_KEY: u64 = 4;
pub const EXPECTED_STAGE_MASK: u8 = 0b1111;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationRow {
    pub foreign_keys: [u64; 4],
    pub payload: u64,
    pub generation: u64,
    pub stage_mask: u8,
    pub match_counts: [u8; 4],
    pub folds: [u64; 4],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationDimension {
    pub relation: u64,
    pub payload: u64,
}

pub struct ApplicationFixture {
    pub source: CellMap<u64, Arc<ApplicationRow>>,
    pub output: CellMap<u64, Arc<ApplicationRow>, CellImmutable>,
}

fn foreign_keys(key: u64, rekey: bool) -> [u64; 4] {
    let shift = u64::from(rekey && key.is_multiple_of(10));
    [
        (key.wrapping_mul(5).wrapping_add(17 + shift)) % RELATION_KEYS,
        (key.wrapping_mul(13).wrapping_add(43 + shift)) % RELATION_KEYS,
        (key.wrapping_mul(29).wrapping_add(101 + shift)) % RELATION_KEYS,
        (key.wrapping_mul(53).wrapping_add(211 + shift)) % RELATION_KEYS,
    ]
}

pub fn application_row(key: u64, generation: u64, rekey: bool) -> Arc<ApplicationRow> {
    Arc::new(ApplicationRow {
        foreign_keys: foreign_keys(key, rekey),
        payload: key
            .wrapping_mul(0x9e37_79b9)
            .wrapping_add(generation.wrapping_mul(0x85eb_ca6b)),
        generation,
        stage_mask: 0,
        match_counts: [0; 4],
        folds: [0; 4],
    })
}

fn dimension_payload(stage: usize, right_key: u64) -> u64 {
    right_key
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(u32::try_from(stage * 7 + 3).unwrap_or(3))
        ^ u64::try_from(stage + 1)
            .unwrap_or(0)
            .wrapping_mul(0xd6e8_feb8_6659_fd93)
}

fn dimensions(stage: usize) -> CellMap<u64, Arc<ApplicationDimension>> {
    let map = CellMap::new();
    let entries = (0..RELATION_KEYS * MATCHES_PER_KEY)
        .map(|right_key| {
            (
                right_key,
                Arc::new(ApplicationDimension {
                    relation: right_key / MATCHES_PER_KEY,
                    payload: dimension_payload(stage, right_key),
                }),
            )
        })
        .collect();
    map.insert_many(entries);
    map
}

fn fold_stage(
    row: &ApplicationRow,
    matches: &[(u64, Arc<ApplicationDimension>)],
    stage: usize,
) -> Arc<ApplicationRow> {
    // Join bucket construction starts from a concurrent-map snapshot, whose
    // iteration order is intentionally unspecified. Use an order-independent
    // application reduction while the separately observed output diff stream
    // remains strictly order-sensitive.
    let fold = matches.iter().fold(
        0x517c_c1b7_2722_0a95_u64 ^ u64::try_from(stage).unwrap_or(0),
        |acc, (right_key, dimension)| {
            acc ^ right_key.wrapping_mul(0x9e37_79b9).rotate_left(11)
                ^ dimension.payload.rotate_left(23)
        },
    );
    let mut output = row.clone();
    output.payload = output.payload.rotate_left(9) ^ fold;
    output.stage_mask |= 1_u8 << stage;
    output.match_counts[stage] = u8::try_from(matches.len()).unwrap_or(u8::MAX);
    output.folds[stage] = fold;
    Arc::new(output)
}

pub fn build_fixture() -> ApplicationFixture {
    let source = CellMap::new();
    source.insert_many(
        (0..APPLICATION_ROWS)
            .map(|key| (key, application_row(key, 0, false)))
            .collect(),
    );
    let first = dimensions(0);
    let second = dimensions(1);
    let third = dimensions(2);
    let fourth = dimensions(3);
    // The third join deliberately promotes the public two-join plan into a
    // genuine arbitrary-length JoinRegion; the fourth extends that region.
    let output = source
        .clone()
        .left_join_by(
            first,
            |_key, row| row.foreign_keys[0],
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_stage(row, matches, 0))
        .left_join_by(
            second,
            |_key, row| row.foreign_keys[1],
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_stage(row, matches, 1))
        .left_join_by(
            third,
            |_key, row| row.foreign_keys[2],
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_stage(row, matches, 2))
        .left_join_by(
            fourth,
            |_key, row| row.foreign_keys[3],
            |_key, dimension| dimension.relation,
        )
        .map_joined_values(|_key, row, matches| fold_stage(row, matches, 3))
        .materialize();
    ApplicationFixture { source, output }
}

fn shuffled_keys() -> Vec<u64> {
    let mut keys: Vec<_> = (0..APPLICATION_ROWS).collect();
    let mut state = 0x243f_6a88_85a3_08d3_u64;
    for index in (1..keys.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bound = u64::try_from(index + 1).unwrap_or(1);
        let swap = usize::try_from(state % bound).unwrap_or(0);
        keys.swap(index, swap);
    }
    keys
}

pub fn update_batch(generation: u64, rekey: bool) -> Vec<(u64, Arc<ApplicationRow>)> {
    shuffled_keys()
        .into_iter()
        .map(|key| (key, application_row(key, generation, rekey)))
        .collect()
}

pub fn reference_row(key: u64, generation: u64, rekey: bool) -> Arc<ApplicationRow> {
    let mut row = application_row(key, generation, rekey);
    for stage in 0..4 {
        let relation = row.foreign_keys[stage];
        let matches: Vec<_> = (0..MATCHES_PER_KEY)
            .map(|offset| {
                let right_key = relation * MATCHES_PER_KEY + offset;
                (
                    right_key,
                    Arc::new(ApplicationDimension {
                        relation,
                        payload: dimension_payload(stage, right_key),
                    }),
                )
            })
            .collect();
        row = fold_stage(&row, &matches, stage);
    }
    row
}

pub fn reference_updates(
    old_generation: u64,
    old_rekey: bool,
    generation: u64,
    rekey: bool,
) -> Vec<MapDiff<u64, Arc<ApplicationRow>>> {
    shuffled_keys()
        .into_iter()
        .map(|key| MapDiff::Update {
            key,
            old_value: reference_row(key, old_generation, old_rekey),
            new_value: reference_row(key, generation, rekey),
        })
        .collect()
}

fn hash_bytes(mut digest: u128, bytes: &[u8]) -> u128 {
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    for byte in bytes {
        digest ^= u128::from(*byte);
        digest = digest.wrapping_mul(PRIME);
    }
    digest
}

fn hash_row(mut digest: u128, key: u64, row: &ApplicationRow) -> u128 {
    digest = hash_bytes(digest, &key.to_le_bytes());
    for foreign_key in row.foreign_keys {
        digest = hash_bytes(digest, &foreign_key.to_le_bytes());
    }
    digest = hash_bytes(digest, &row.payload.to_le_bytes());
    digest = hash_bytes(digest, &row.generation.to_le_bytes());
    digest = hash_bytes(digest, &[row.stage_mask]);
    digest = hash_bytes(digest, &row.match_counts);
    for fold in row.folds {
        digest = hash_bytes(digest, &fold.to_le_bytes());
    }
    digest
}

pub fn final_state_digest(output: &CellMap<u64, Arc<ApplicationRow>, CellImmutable>) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    (0..APPLICATION_ROWS).fold(OFFSET, |digest, key| {
        let row = output
            .get_value(&key)
            .expect("every application row must exist");
        hash_row(digest, key, &row)
    })
}

pub fn reference_digest(generation: u64, rekey: bool) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    (0..APPLICATION_ROWS).fold(OFFSET, |digest, key| {
        hash_row(digest, key, &reference_row(key, generation, rekey))
    })
}

fn ordered_diff_digest(changes: &[MapDiff<u64, Arc<ApplicationRow>>]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    changes
        .iter()
        .fold(OFFSET, |mut digest, change| match change {
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => {
                digest = hash_bytes(digest, &[2]);
                digest = hash_row(digest, *key, old_value);
                hash_row(digest, *key, new_value)
            }
            MapDiff::Insert { key, value } => {
                digest = hash_bytes(digest, &[1]);
                hash_row(digest, *key, value)
            }
            MapDiff::Remove { key, old_value } => {
                digest = hash_bytes(digest, &[3]);
                hash_row(digest, *key, old_value)
            }
            MapDiff::Initial { entries } => entries
                .iter()
                .fold(hash_bytes(digest, &[0]), |inner, (key, value)| {
                    hash_row(inner, *key, value)
                }),
            MapDiff::Batch { changes } => {
                let digest = hash_bytes(digest, &[4]);
                hash_bytes(
                    ordered_diff_digest(changes) ^ digest,
                    &changes.len().to_le_bytes(),
                )
            }
        })
}

pub fn assert_preflight() {
    let fixture = build_fixture();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_sink = Arc::clone(&observed);
    let _guard = fixture.output.subscribe_diffs(move |diff| {
        if !matches!(diff, MapDiff::Initial { .. }) {
            observed_sink
                .lock()
                .expect("trace lock poisoned")
                .push(diff.clone());
        }
    });
    fixture.source.insert_many(update_batch(1, false));

    let expected = [MapDiff::Batch {
        changes: reference_updates(0, false, 1, false),
    }];
    let actual = observed.lock().expect("trace lock poisoned").clone();
    assert_eq!(actual.len(), expected.len(), "callback boundary count");
    let (
        MapDiff::Batch {
            changes: actual_changes,
        },
        MapDiff::Batch {
            changes: expected_changes,
        },
    ) = (&actual[0], &expected[0])
    else {
        panic!("expected one batch callback");
    };
    assert_eq!(
        actual_changes.len(),
        expected_changes.len(),
        "published change count"
    );
    for (ordinal, (actual_change, expected_change)) in
        actual_changes.iter().zip(expected_changes).enumerate()
    {
        assert_eq!(
            actual_change, expected_change,
            "published diff mismatch at input ordinal {ordinal}"
        );
    }
    assert_eq!(
        fixture.output.entries().materialize().get().len(),
        usize::try_from(APPLICATION_ROWS).unwrap_or(usize::MAX)
    );
    for key in 0..APPLICATION_ROWS {
        let row = fixture.output.get_value(&key).expect("settled output row");
        assert_eq!(row.generation, 1, "insert_many must settle synchronously");
        assert_eq!(row.stage_mask, EXPECTED_STAGE_MASK);
        assert_eq!(
            row.match_counts,
            [u8::try_from(MATCHES_PER_KEY).unwrap_or(u8::MAX); 4]
        );
    }
    let digest = final_state_digest(&fixture.output);
    assert_eq!(digest, reference_digest(1, false));
    assert_eq!(digest, 0xb394_c0e6_dc59_b647_f2dc_1b05_314b_0043);
    let trace_digest = ordered_diff_digest(&actual);
    assert_eq!(trace_digest, 0x959c_dbd3_c843_b5ae_31f7_5752_0811_3219);
}
