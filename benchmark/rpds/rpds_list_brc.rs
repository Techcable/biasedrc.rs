use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

type ListBrc<T> = rpds::List<T, biasedrc::BrcK>;

fn rpds_list_brc_push_front(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds list brc push front", move |b| {
        b.iter(|| {
            let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

            for i in 0..limit {
                list = list.push_front(i);
            }

            list
        });
    });
}

fn rpds_list_brc_push_front_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds list brc push front mut", move |b| {
        b.iter(|| {
            let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

            for i in 0..limit {
                list.push_front_mut(i);
            }

            list
        });
    });
}

fn rpds_list_brc_drop_first(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds list brc drop first", move |b| {
        b.iter_with_setup(
            || {
                let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

                for i in 0..limit {
                    list.push_front_mut(i);
                }

                list
            },
            |mut list| {
                for _ in 0..limit {
                    list = list.drop_first().unwrap();
                }

                list
            },
        );
    });
}

fn rpds_list_brc_drop_first_mut(c: &mut Criterion) {
    let limit = 100_000;

    c.bench_function("rpds list brc drop first mut", move |b| {
        b.iter_with_setup(
            || {
                let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

                for i in 0..limit {
                    list.push_front_mut(i);
                }

                list
            },
            |mut list| {
                for _ in 0..limit {
                    list.drop_first_mut();
                }

                list
            },
        );
    });
}

fn rpds_list_brc_reverse(c: &mut Criterion) {
    let limit = 2_000;

    c.bench_function("rpds list brc reverse", move |b| {
        b.iter_with_setup(
            || {
                let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

                for i in 0..limit {
                    list.push_front_mut(i);
                }

                list
            },
            |mut list| {
                for _ in 0..limit {
                    list = list.reverse();
                }

                list
            },
        );
    });
}

fn rpds_list_brc_reverse_mut(c: &mut Criterion) {
    let limit = 2_000;

    c.bench_function("rpds list brc reverse mut", move |b| {
        b.iter_with_setup(
            || {
                let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

                for i in 0..limit {
                    list.push_front_mut(i);
                }

                list
            },
            |mut list| {
                for _ in 0..limit {
                    list.reverse_mut();
                }

                list
            },
        );
    });
}

#[allow(clippy::explicit_iter_loop)]
fn rpds_list_brc_iterate(c: &mut Criterion) {
    let limit = 1_000_000;
    let mut list: ListBrc<usize> = ListBrc::new_with_ptr_kind();

    for i in 0..limit {
        list.push_front_mut(i);
    }

    c.bench_function("rpds list brc iterate", move |b| {
        b.iter(|| {
            for i in list.iter() {
                black_box(i);
            }
        });
    });
}

criterion_group!(
    benches,
    rpds_list_brc_push_front,
    rpds_list_brc_push_front_mut,
    rpds_list_brc_drop_first,
    rpds_list_brc_drop_first_mut,
    rpds_list_brc_reverse,
    rpds_list_brc_reverse_mut,
    rpds_list_brc_iterate
);
criterion_main!(benches);
