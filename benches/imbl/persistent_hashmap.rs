#![allow(
    clippy::pedantic,
    clippy::disallowed_types,
    clippy::if_same_then_else,
    missing_docs
)]
use criterion::{Bencher, Criterion, criterion_group, criterion_main};
use std::borrow::Borrow;
use std::collections::HashMap as StdHashMap;
use std::hash::{Hash, RandomState};
use std::hint::black_box;
use std::iter::FromIterator;
use std::sync::Arc;

use archery::{ArcK, RcK};
use biasedrc::BrcK;
use imbl::GenericHashMap;
use rpds::HashTrieMap;

mod utils;
use utils::*;

// Trait to abstract over different map implementations
trait BenchMap<K, V>: Clone + FromIterator<(K, V)>
where
    K: Clone + Hash + Eq,
    V: Clone,
{
    const IMMUTABLE: bool = true;
    type Iter<'a>: Iterator<Item = (&'a K, &'a V)>
    where
        Self: 'a,
        K: 'a,
        V: 'a;

    fn new() -> Self;
    fn insert(&mut self, k: K, v: V) -> Option<V>;
    fn insert_clone(&self, k: K, v: V) -> Self;
    fn remove(&mut self, k: &K) -> Option<V>;
    fn remove_clone(&self, k: &K) -> Self;
    fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized;
    fn iter(&self) -> Self::Iter<'_>;
}

macro_rules! impl_imbl {
    ($ptr_kind:ident) => {
        // Implementation for imbl::HashMap
        impl<K, V> BenchMap<K, V> for GenericHashMap<K, V, RandomState, $ptr_kind>
        where
            K: Clone + Hash + Eq,
            V: Clone,
        {
            type Iter<'a>
                = imbl::hashmap::Iter<'a, K, V, $ptr_kind>
            where
                K: 'a,
                V: 'a;

            fn new() -> Self {
                Default::default()
            }

            fn insert(&mut self, k: K, v: V) -> Option<V> {
                self.insert(k, v)
            }

            fn insert_clone(&self, k: K, v: V) -> Self {
                self.update(k, v)
            }

            fn remove(&mut self, k: &K) -> Option<V> {
                self.remove(k)
            }

            fn remove_clone(&self, k: &K) -> Self {
                self.without(k)
            }

            fn get<Q>(&self, k: &Q) -> Option<&V>
            where
                K: Borrow<Q>,
                Q: Hash + Eq + ?Sized,
            {
                self.get(k)
            }

            fn iter(&self) -> Self::Iter<'_> {
                self.iter()
            }
        }
    };
}
impl_imbl!(ArcK);
impl_imbl!(BrcK);
impl_imbl!(RcK);

// Implementation for std::collections::HashMap
impl<K, V> BenchMap<K, V> for StdHashMap<K, V>
where
    K: Clone + Hash + Eq,
    V: Clone,
{
    const IMMUTABLE: bool = false;
    type Iter<'a>
        = std::collections::hash_map::Iter<'a, K, V>
    where
        K: 'a,
        V: 'a;

    fn new() -> Self {
        StdHashMap::new()
    }

    fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.insert(k, v)
    }

    fn insert_clone(&self, k: K, v: V) -> Self {
        let mut ret = self.clone();
        ret.insert(k, v);
        ret
    }

    fn remove(&mut self, k: &K) -> Option<V> {
        self.remove(k)
    }

    fn remove_clone(&self, k: &K) -> Self {
        let mut ret = self.clone();
        ret.remove(k);
        ret
    }

    fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get(k)
    }

    fn iter(&self) -> Self::Iter<'_> {
        self.iter()
    }
}

macro_rules! impl_rpds {
    ($ptr_kind:ident) => {
        // Implementation for rpds::HashTrieMapSync
        impl<K, V> BenchMap<K, V> for HashTrieMap<K, V, $ptr_kind>
        where
            K: Clone + Hash + Eq,
            V: Clone,
        {
            type Iter<'a>
                = rpds::map::hash_trie_map::Iter<'a, K, V, $ptr_kind>
            where
                K: 'a,
                V: 'a;

            fn new() -> Self {
                Default::default()
            }

            fn insert(&mut self, k: K, v: V) -> Option<V> {
                self.insert_mut(k, v);
                None
            }

            fn insert_clone(&self, k: K, v: V) -> Self {
                self.insert(k, v)
            }

            fn remove(&mut self, k: &K) -> Option<V> {
                if self.remove_mut(k) {
                    None // rpds doesn't return the removed value
                } else {
                    None
                }
            }

            fn remove_clone(&self, k: &K) -> Self {
                self.remove(k)
            }

            fn get<Q>(&self, k: &Q) -> Option<&V>
            where
                K: Borrow<Q>,
                Q: Hash + Eq + ?Sized,
            {
                self.get(k)
            }

            fn iter(&self) -> Self::Iter<'_> {
                self.iter()
            }
        }
    };
}
impl_rpds!(ArcK);
impl_rpds!(BrcK);
impl_rpds!(RcK);

// Generic benchmark functions
fn bench_lookup<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let order = reorder(&keys);
    let m: M = keys.into_iter().zip(values).collect();
    b.iter(|| {
        for k in &order {
            black_box(m.get(k));
        }
    })
}

fn bench_lookup_ne<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size * 2);
    let values = V::generate(size);
    let order = reorder(&keys[size..]);
    let m: M = keys.into_iter().zip(values).collect();
    b.iter(|| {
        for k in &order {
            black_box(m.get(k));
        }
    })
}

fn bench_insert<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    b.iter(|| {
        let mut m = M::new();
        for (k, v) in keys.clone().into_iter().zip(values.clone()) {
            m = m.insert_clone(k, v);
        }
        m
    })
}

fn bench_insert_mut<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    b.iter(|| {
        let mut m = M::new();
        for (k, v) in keys.clone().into_iter().zip(values.clone()) {
            m.insert(k, v);
        }
        m
    })
}

fn bench_remove<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let order = reorder(&keys);
    let map: M = keys.into_iter().zip(values).collect();
    b.iter(|| {
        let mut m = map.clone();
        for k in &order {
            m = m.remove_clone(k);
        }
        m
    })
}

fn bench_remove_mut<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let order = reorder(&keys);
    let map: M = keys.into_iter().zip(values).collect();
    b.iter(|| {
        let mut m = map.clone();
        for k in &order {
            m.remove(k);
        }
        m
    })
}

fn bench_insert_once<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let korder = reorder(&keys);
    let vorder = reorder(&values);
    let m: M = keys.clone().into_iter().zip(values).collect();
    b.iter(|| {
        for (k, v) in korder.iter().zip(vorder.iter()).take(100) {
            black_box(m.insert_clone(k.clone(), v.clone()));
        }
    })
}

fn bench_remove_once<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let order = reorder(&keys);
    let map: M = keys.clone().into_iter().zip(values).collect();
    b.iter(|| {
        for k in order.iter().take(100) {
            black_box(map.remove_clone(k));
        }
    })
}

fn bench_iter<M, K, V>(b: &mut Bencher, size: usize)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let keys = K::generate(size);
    let values = V::generate(size);
    let m: M = keys.into_iter().zip(values).collect();
    b.iter(|| {
        for p in m.iter() {
            black_box(p);
        }
    })
}

macro_rules! do_bench {
    ($c:ident, $group:literal, $target:ident<$key:path, $value:path $(, $extra:ident)?>) => {
        bench_group::<$target<$key, $value, $($extra,)* ArcK>, $key, $value>($c, &format!($group, ptr = "arc"));
        bench_group::<$target<$key, $value, $($extra,)? BrcK>, $key, $value>($c, &format!($group, ptr = "brc"));
        bench_group::<$target<$key, $value, $($extra,)? BrcK>, $key, $value>($c, &format!($group, ptr = "rc"));
    };
}

// Benchmark functions for each map type
fn bench_hashmap(c: &mut Criterion) {
    do_bench!(c, "imbl_hashmap_{ptr}_i64", GenericHashMap<i64, i64, RandomState>);
    do_bench!(
        c,
        "imbl_hashmap_{ptr}_str",
        GenericHashMap<Arc<String>, Arc<String>, RandomState>
    );
}

fn bench_rpds(c: &mut Criterion) {
    do_bench!(
        c,
        "rpds_hashtrie_{ptr}_str",
        HashTrieMap<Arc<String>, Arc<String>>
    );
    do_bench!(
        c,
        "rpds_hashtrie_{ptr}_str",
        HashTrieMap<Arc<String>, Arc<String>>
    );

    bench_group::<HashTrieMap<Arc<String>, Arc<String>>, Arc<String>, Arc<String>>(c, "rpds_str");
}

fn bench_stdhashmap(c: &mut Criterion) {
    bench_group::<StdHashMap<i64, i64>, i64, i64>(c, "std_hashmap_i64");
    bench_group::<StdHashMap<Arc<String>, Arc<String>>, Arc<String>, Arc<String>>(
        c,
        "std_hashmap_str",
    );
}

// Helper function to run all benchmarks for a specific map/key/value type
fn bench_group<M, K, V>(c: &mut Criterion, group_name: &str)
where
    M: BenchMap<K, V>,
    K: TestData,
    V: TestData,
{
    let mut group = c.benchmark_group(group_name);

    for size in &[100, 1000, 10000, 100000] {
        group.bench_function(format!("lookup_{}", size), |b| {
            bench_lookup::<M, K, V>(b, *size)
        });
    }

    for size in &[10000, 100000] {
        group.bench_function(format!("lookup_ne_{}", size), |b| {
            bench_lookup_ne::<M, K, V>(b, *size)
        });
    }

    for size in &[100, 1000, 10000, 100000] {
        group.bench_function(format!("insert_mut_{}", size), |b| {
            bench_insert_mut::<M, K, V>(b, *size)
        });
    }

    for size in &[100, 1000, 10000] {
        group.bench_function(format!("remove_mut_{}", size), |b| {
            bench_remove_mut::<M, K, V>(b, *size)
        });
    }

    for size in &[1000, 10000] {
        group.bench_function(format!("iter_{}", size), |b| {
            bench_iter::<M, K, V>(b, *size)
        });
    }

    if M::IMMUTABLE {
        for size in &[100, 1000, 10000] {
            group.bench_function(format!("insert_{}", size), |b| {
                bench_insert::<M, K, V>(b, *size)
            });

            group.bench_function(format!("remove_{}", size), |b| {
                bench_remove::<M, K, V>(b, *size)
            });
        }

        for size in &[100, 1000, 10000, 100000] {
            group.bench_function(format!("insert_once_{}", size), |b| {
                bench_insert_once::<M, K, V>(b, *size)
            });

            group.bench_function(format!("remove_once_{}", size), |b| {
                bench_remove_once::<M, K, V>(b, *size)
            });
        }
    }

    group.finish();
}

// Main benchmark entry point
fn hashmap_benches(c: &mut Criterion) {
    bench_hashmap(c);

    bench_stdhashmap(c);

    bench_rpds(c);
}

criterion_group!(benches, hashmap_benches);
criterion_main!(benches);
