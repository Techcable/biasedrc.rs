use biasedrc::BrcK;
use criterion::{Criterion, criterion_group, criterion_main};
use rpds::HashTrieMap;
use std::hint::black_box;

pub type HashTrieMapBrc<K, V> = HashTrieMap<K, V, BrcK>;

fn rpds_hash_trie_map_brc_insert(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds hash trie map brc insert", move |b| {
        b.iter(|| {
            let mut map = HashTrieMapBrc::default();

            for i in 0..limit {
                map = map.insert(i, -(i as isize));
            }

            map
        });
    });
}

fn rpds_hash_trie_map_brc_insert_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds hash trie map brc insert mut", move |b| {
        b.iter(|| {
            let mut map = HashTrieMapBrc::default();

            for i in 0..limit {
                map.insert_mut(i, -(i as isize));
            }

            map
        });
    });
}

fn rpds_hash_trie_map_brc_remove(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds hash trie map brc remove", move |b| {
        b.iter_with_setup(
            || {
                let mut map = HashTrieMapBrc::default();

                for i in 0..limit {
                    map.insert_mut(i, -(i as isize));
                }

                map
            },
            |mut map| {
                for i in 0..limit {
                    map = map.remove(&i);
                }

                map
            },
        );
    });
}

fn rpds_hash_trie_map_brc_remove_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds hash trie map brc remove mut", move |b| {
        b.iter_with_setup(
            || {
                let mut map = HashTrieMapBrc::default();

                for i in 0..limit {
                    map.insert_mut(i, -(i as isize));
                }

                map
            },
            |mut map| {
                for i in 0..limit {
                    map.remove_mut(&i);
                }

                map
            },
        );
    });
}

fn rpds_hash_trie_map_brc_get(c: &mut Criterion) {
    let limit = 100_000;
    let mut map = HashTrieMapBrc::default();

    for i in 0..limit {
        map.insert_mut(i, -(i as isize));
    }

    c.bench_function("rpds hash trie map brc get", move |b| {
        b.iter(|| {
            for i in 0..limit {
                black_box(map.get(&i));
            }
        });
    });
}

fn rpds_hash_trie_map_brc_iterate(c: &mut Criterion) {
    let limit = 1_000_000;
    let mut map = HashTrieMapBrc::default();

    for i in 0..limit {
        map.insert_mut(i, -(i as isize));
    }

    c.bench_function("rpds hash trie map brc iterate", move |b| {
        b.iter(|| {
            for kv in map.iter() {
                black_box(kv);
            }
        });
    });
}

#[allow(unused_variables)]
fn rpds_hash_trie_map_brc_iterate_parallel(c: &mut Criterion) {
    #[cfg(false)] // #[cfg(feature = "rayon")]
    {
        use rayon::prelude::*;

        let limit = 1_000_000;
        let mut map = HashTrieMapBrc::default();

        for i in 0..limit {
            map.insert_mut(i, -(i as isize));
        }

        c.bench_function("rpds hash trie map brc iterate parallel", move |b| {
            b.iter(|| {
                map.par_iter().for_each(|kv| {
                    black_box(kv);
                });
            });
        });
    }
}

criterion_group!(
    benches,
    rpds_hash_trie_map_brc_insert,
    rpds_hash_trie_map_brc_insert_mut,
    rpds_hash_trie_map_brc_remove,
    rpds_hash_trie_map_brc_remove_mut,
    rpds_hash_trie_map_brc_get,
    rpds_hash_trie_map_brc_iterate,
    rpds_hash_trie_map_brc_iterate_parallel
);
criterion_main!(benches);
