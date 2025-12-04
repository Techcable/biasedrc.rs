#![allow(clippy::disallowed_types, missing_docs)]
use biasedrc::Brc;
use std::rc::Rc;
use std::sync::Arc;
use triomphe::Arc as ArcT;

macro_rules! bench_clones {
    ($c:ident => $($target:ident),*) => {
        $($c.bench_function(concat!(stringify!($target), "::clone"), |b| {
            let rc = $target::new(7);
            b.iter_with_large_drop(|| $target::clone(&rc));
        });)*
    };
}

macro_rules! bench_drops {
    ($c:ident => $($target:ident),*) => {
        $($c.bench_function(concat!(stringify!($target), "::drop_unique"), |b| {
            b.iter_batched(
                || $target::new(7),
                drop,
                criterion::BatchSize::SmallInput,
            )
        });
        $c.bench_function(concat!(stringify!($target), "::drop_shared"), |b| {
            let rc = $target::new(7);
            b.iter_batched(
                || $target::clone(&rc),
                drop,
                criterion::BatchSize::SmallInput,
            )
        });)*
    };
}

pub fn main() {
    let mut c = criterion::Criterion::default().configure_from_args();

    bench_clones!(c => Arc, ArcT, Brc, Rc);
    bench_drops!(c => Arc, ArcT, Brc, Rc);
}
