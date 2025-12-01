//! Basic smoke tests.

use biasedrc::Brc;
use std::cell::Cell;
use std::error::Error;
use std::fmt::Debug;
use unsize::{CoerceUnsize, CoerciblePtr};

/// Tests that allocation works, without testing [`Clone`].
#[test]
fn alloc_int() {
    let res = Brc::new(52);
    assert_eq!(*res, 52);
    drop(res);
}

#[test]
fn alloc_string() {
    let res = Brc::new(String::from("hello"));
    assert_eq!(&*res, "hello");
    drop(res);
}

#[test]
fn count_drop() {
    let count = Cell::new(0);
    let one = Brc::new(DropCounter(&count));
    let two = Brc::clone(&one);
    drop(one);
    drop(two);
    assert_eq!(count.get(), 1);
}

#[test]
fn alloc_with_clone() {
    let one = Brc::new(String::from("hello"));
    let two = Brc::clone(&one);
    assert_eq!(*two, "hello");
    assert_eq!(*one, *two);
    // reverse drop
    drop(one);
    drop(two);
}

#[derive(thiserror::Error, Debug)]
#[error("{msg}")]
struct SimpleError {
    msg: String,
}

/// Tests that coercions work using the `unsize` crate.
#[test]
fn coerce_unsize_crate() {
    /// Does an arbitrary coercion for `dyn Error`
    fn arbitrary_error<T: CoerciblePtr<dyn Error, Output = U>, U>(ptr: T) -> U
    where
        T::Pointee: Error + 'static,
    {
        ptr.unsize(unsize::Coercion!(to dyn Error))
    }

    const TEST_MSG: &str = "foo";
    fn check_error<U: AsRef<dyn Error>>(val: U) {
        assert_eq!(format!("{}", val.as_ref()), format!("{}", TEST_MSG));
    }

    let err = SimpleError {
        msg: TEST_MSG.into(),
    };
    check_error(arbitrary_error::<Brc<SimpleError>, Brc<dyn Error>>(
        Brc::new(err),
    ));
}

#[cfg(feature = "nightly-coerce")]
#[test]
fn nightly_coerce() {
    fn coerce<T: Error + 'static>(x: Brc<T>) -> Brc<dyn Error> {
        x
    }
    const TEST_MSG: &str = "foo";
    fn check_error<U: AsRef<dyn Error>>(val: U) {
        assert_eq!(format!("{}", val.as_ref()), format!("{}", TEST_MSG));
    }

    let err = SimpleError {
        msg: TEST_MSG.into(),
    };
    check_error(coerce(Brc::new(err)));
}

struct DropCounter<'a>(&'a Cell<u32>);
impl Drop for DropCounter<'_> {
    fn drop(&mut self) {
        self.0.update(|x| x + 1);
    }
}

/// Tests multiple threads, scoped so that it is necessary to add to the queue and merge reference counts.
#[test]
fn multithread_requires_merge() {
    let one = Brc::new(42);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let two = Brc::clone(&one);
            drop(one);
            biasedrc::collect_force();
            assert_eq!(*two, 42);
            drop(two);
        });
        biasedrc::collect_force();
    });
}
