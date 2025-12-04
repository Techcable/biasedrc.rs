#![allow(missing_docs)]
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

type VectorBrc<T> = rpds::Vector<T, biasedrc::BrcK>;

fn rpds_vector_brcpush_back(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds vector brc push back", move |b| {
        b.iter(|| {
            let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

            for i in 0..limit {
                vector = vector.push_back(i);
            }

            vector
        });
    });
}

fn rpds_vector_brcpush_back_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds vector brc push back mut", move |b| {
        b.iter(|| {
            let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

            for i in 0..limit {
                vector.push_back_mut(i);
            }

            vector
        });
    });
}

fn rpds_vector_brcdrop_last(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds vector brc drop last", move |b| {
        b.iter_with_setup(
            || {
                let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

                for i in 0..limit {
                    vector.push_back_mut(i);
                }

                vector
            },
            |mut vector| {
                for _ in 0..limit {
                    vector = vector.drop_last().unwrap();
                }

                vector
            },
        );
    });
}

fn rpds_vector_brcdrop_last_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds vector brc drop last mut", move |b| {
        b.iter_with_setup(
            || {
                let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

                for i in 0..limit {
                    vector.push_back_mut(i);
                }

                vector
            },
            |mut vector| {
                for _ in 0..limit {
                    vector.drop_last_mut();
                }

                vector
            },
        );
    });
}

fn rpds_vector_brcget(c: &mut Criterion) {
    let limit = 1_000_000;
    let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

    for i in 0..limit {
        vector.push_back_mut(i);
    }

    c.bench_function("rpds vector brc get", move |b| {
        b.iter(|| {
            for i in 0..limit {
                black_box(vector.get(i));
            }
        });
    });
}

#[allow(clippy::explicit_iter_loop)]
fn rpds_vector_brciterate(c: &mut Criterion) {
    let limit = 1_000_000;
    let mut vector: VectorBrc<usize> = VectorBrc::new_with_ptr_kind();

    for i in 0..limit {
        vector.push_back_mut(i);
    }

    c.bench_function("rpds vector brc iterate", move |b| {
        b.iter(|| {
            for i in vector.iter() {
                black_box(i);
            }
        });
    });
}

criterion_group!(
    benches,
    rpds_vector_brcpush_back,
    rpds_vector_brcpush_back_mut,
    rpds_vector_brcdrop_last,
    rpds_vector_brcdrop_last_mut,
    rpds_vector_brcget,
    rpds_vector_brciterate
);
criterion_main!(benches);
