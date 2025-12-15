//! Basic smoke tests.

use biasedrc::{Brc, Weak};
use std::error::Error;
use std::fmt::Debug;
use std::sync::atomic::{AtomicU32, Ordering};
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
    let count = AtomicU32::new(0);
    let one = Brc::new(DropCounter(&count));
    let two = Brc::clone(&one);
    drop(one);
    drop(two);
    assert_eq!(count.load(Ordering::SeqCst), 1);
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

#[test]
fn alloc_from_box() {
    let one: Box<str> = Box::from("hello");
    let one: Brc<str> = Brc::from(one);
    assert_eq!(one.as_ref(), "hello");
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

struct DropCounter<'a>(&'a AtomicU32);
impl Drop for DropCounter<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                Some(x.checked_add(1).unwrap())
            });
    }
}

#[test]
fn alloc_slice() {
    let x: Brc<[u32]> = Brc::from([1, 2, 3].as_slice());
    assert_eq!(*x, [1, 2, 3]);
    drop(x);
}

#[test]
fn alloc_vec() {
    let x: Brc<[Box<u32>]> = Brc::from(vec![Box::new(1), Box::new(2), Box::new(3)]);
    assert_eq!((*x).iter().map(|x| **x).collect::<Vec<_>>(), [1, 2, 3],);
    drop(x);
}

#[test]
fn alloc_iter() {
    // this should not need buffering
    let x: Brc<[u32]> = (0..=20).collect();
    assert_eq!(
        x.iter().copied().collect::<Vec<_>>(),
        (0..=20).collect::<Vec<u32>>()
    );
}

#[test]
fn get_mut() {
    {
        let mut one = Brc::new(42);
        assert_eq!(*Brc::get_mut(&mut one).unwrap(), 42);
    }
    {
        let one = Brc::new(42);
        let mut two = Brc::clone(&one);
        assert_eq!(Brc::get_mut(&mut two), None);
    }
    {
        let one = Brc::new(42);
        std::thread::scope(move |scope| {
            scope.spawn(move || {
                let mut two = Brc::clone(&one);
                drop(one);
                assert_eq!(
                    Brc::get_mut(&mut two),
                    None,
                    "Calling get_mut should fail on non-biased thread"
                );
            });
        });
    }
}

#[test]
fn weak_downgrade_then_upgrade() {
    let strong = Brc::new(42);
    let weak = Brc::downgrade(&strong);
    let upgrade = Weak::upgrade(&weak).unwrap();
    assert_eq!(strong, upgrade);
}

#[test]
fn weak_forget_then_upgrade() {
    let strong = Brc::new(42);
    let weak = Brc::downgrade(&strong);
    drop(strong);
    assert_eq!(Weak::upgrade(&weak), None);
}

#[test]
fn weak_dummy_upgrade() {
    let weak = Weak::<i32>::new();
    assert_eq!(weak.upgrade(), None);
}

#[test]
fn weak_slices() {
    let strong: Brc<[i32]> = Brc::from(vec![1, 2, 3]);
    let weak = Brc::downgrade(&strong);
    assert_eq!(Some(strong), Weak::upgrade(&weak));
    let strong: Brc<str> = Brc::from("foo");
    let weak = Brc::downgrade(&strong);
    assert_eq!(Some(strong), Weak::upgrade(&weak));
}
