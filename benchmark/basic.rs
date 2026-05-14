#![allow(clippy::disallowed_types)]
use biasedrc::{BiasedCountError, Brc};
use criterion::Criterion;
use std::rc::Rc;
use std::sync::Arc;
use triomphe::Arc as ArcT;

/// Abstracts over different types of recounted pointers.
///
/// Similar to [`archery::SharedPointerKind`].
///
/// [archery `SharedPointerKind`]: https://docs.rs/archery/1.2/archery/shared_pointer/kind/trait.SharedPointerKind.html
trait SharedPtrKind {
    type Ref<T>: Sized + Clone;
    fn bench_name(op: &str) -> String;
    fn new<T>(value: T) -> Self::Ref<T>;
}
mod kinds {
    use super::SharedPtrKind;
    macro_rules! simple_shared_ptr {
        ($($target:ident),+ $(,)?) => {$(
            pub struct $target;
            impl SharedPtrKind for $target {
                type Ref<T> = super::$target::<T>;
                fn bench_name(op: &str) -> String {
                    format!("{tp}::{op}", tp = stringify!($target))
                }
                #[inline]
                fn new<T>(value: T) -> Self::Ref<T> {
                    super::$target::new(value)
                }
            }
        )*};
    }
    simple_shared_ptr!(Arc, ArcT, Rc, Brc, BrcShared);
}

fn bench_new<K: SharedPtrKind>(c: &mut Criterion) {
    c.bench_function(&K::bench_name("new"), |b| {
        b.iter_with_large_drop(|| K::new(7));
    });
}

fn bench_clone<K: SharedPtrKind>(c: &mut Criterion) {
    let rc = K::new(7);
    c.bench_function(&K::bench_name("clone"), |b| {
        b.iter_with_large_drop(|| K::Ref::clone(&rc));
    });
}

fn bench_drop<K: SharedPtrKind>(c: &mut Criterion) {
    c.bench_function(&K::bench_name("drop_unique"), |b| {
        b.iter_batched(|| K::new(7), drop, criterion::BatchSize::SmallInput);
    });
    c.bench_function(&K::bench_name("drop_shared"), |b| {
        let rc = K::new(7);
        b.iter_batched(
            || K::Ref::clone(&rc),
            drop,
            criterion::BatchSize::SmallInput,
        );
    });
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
    /// so we have to emulate it with a good deal of overhead.
    /// This overhead is fine if we restrict this function to the initial setup.
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

macro_rules! bench_all {
    ($c:ident, ops: [], kinds: $($ignored:tt)*) => ({});
    ($c:ident, ops: [$op:ident $(, $op_extra:ident)*], kinds: $($kind:ident),+) => ({
        paste3::paste! {
            $([<bench_ $op>]::<kinds::$kind>(&mut $c);)*
        }
        // recurse for remaining ops. this is done to avoid capture iteration problems
        bench_all!($c, ops: [$($op_extra),*], kinds: $($kind),*)
    })
}

pub fn main() {
    let mut c = Criterion::default().configure_from_args();

    bench_all!(c, ops: [new], kinds: Arc, ArcT, Brc, Rc);
    bench_all!(c, ops: [clone, drop], kinds: Arc, ArcT, Brc, BrcShared, Rc);
}
