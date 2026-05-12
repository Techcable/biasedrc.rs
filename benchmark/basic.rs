#![allow(clippy::disallowed_types)]
use biasedrc::{BiasedCountError, Brc};
use std::rc::Rc;
use std::sync::Arc;
use triomphe::Arc as ArcT;

macro_rules! bench_new {
    ($c:ident => $($target:ident),*) => {
        $($c.bench_function(concat!(stringify!($target), "::new"), |b| {
            b.iter_with_large_drop(|| $target::new(7));
        });)*
    };
}

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

/// A wrapper around a [`Brc`] which only affects the shared reference count.
pub struct BrcShared<T>(Brc<T>);
impl<T> BrcShared<T> {
    /// Creates a [`Brc`] with an initial shared reference count of 1,
    /// and a biased reference count of zero.
    ///
    /// # Benchmarking
    /// Do not benchmark this function.
    ///
    /// It gives no real-world data because whatever thread
    /// [`Brc::new`] is called on becomes the biased thread.
    /// This means `new` will always be called from the biased thread,
    /// and benchmarking something that can never happen makes no sense.
    /// Calling [`Brc::new`] doesn't need synchronization to mutate internal state,
    /// since the resulting object starts with only a single owner.
    ///
    /// ## Skewed Performance
    /// The most important reason that this function cannot be benchmarked
    /// is that it requires a sequence of clone/drop operations.
    /// This is because we do not currently offer a proper `Brc::new_shared` function,
    /// so have to emulate it with a good deal of overhead.
    /// This overhead is fine if we keep this function
    #[allow(clippy::missing_panics_doc)]
    pub fn new(value: T) -> Self {
        let biased = Brc::new(value);
        let shared = Brc::clone_shared(&biased);
        drop(biased);
        assert_eq!(
            Brc::biased_and_shared_counts(&shared),
            (Err(BiasedCountError::NotBiased), 1)
        );
        BrcShared(shared)
    }
}
impl<T> Clone for BrcShared<T> {
    fn clone(&self) -> Self {
        BrcShared(Brc::clone_shared(&self.0))
    }
}

pub fn main() {
    let mut c = criterion::Criterion::default().configure_from_args();

    bench_new!(c => Arc, ArcT, Brc, Rc);
    bench_clones!(c => Arc, ArcT, Brc, BrcShared, Rc);
    bench_drops!(c => Arc, ArcT, Brc, BrcShared, Rc);
}
