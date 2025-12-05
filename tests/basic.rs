//! Basic smoke tests.

use biasedrc::Brc;
use std::error::Error;
use std::fmt::Debug;
use std::sync::Barrier;
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

/// Tests multiple threads, scoped so that it is necessary to add to the queue and merge reference counts.
#[test]
fn multithread_requires_merge() {
    let counter = AtomicU32::new(0);
    let one = Brc::new(DropCounter(&counter));
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let two = Brc::clone(&one);
                drop(one);
                biasedrc::collect_force();
                drop(two);
            })
            .join()
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        biasedrc::collect_force();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    });
}

/// Tests multiple threads, scoped so that all uses are dominated by a biased reference.
#[test]
fn multithread_dominated_biased() {
    let one = Brc::new(42);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let two = Brc::clone(&one);
            drop(two);
        });
        scope.spawn(|| {
            let three = Brc::clone(&one);
            drop(three);
        });
    });
    drop(one);
}

/// Tests multiple threads, scoped so that the biased thread has to unbias its reference
/// while shared references are still active.
#[test]
fn multithread_unbias() {
    use biasedrc::BiasedCountError::{NotBiased, WrongThread};
    let counter = AtomicU32::new(0);
    let finish_unbias = Barrier::new(2);
    // need to use two separate channels so that panics disconnect the channel
    let (send_biased, recv_biased) = crossbeam_channel::bounded(0);
    let (send_back_biased, recv_back_biased) = crossbeam_channel::bounded(0);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let send_biased = send_biased;
            let recv_back_biased = recv_back_biased;
            let biased = Brc::new(DropCounter(&counter)); // biased = 1 and shared = 0
            assert_eq!(Brc::biased_and_shared_counts(&biased), (Ok(1), 0));
            // wait until the other thread receives the object and clones it,
            // at which point we have biased = 1 and shared = 1
            send_biased.send(biased).unwrap();
            // wait until the other thread sends us back our biased reference,
            // so that we can drop it while they still have a live shared reference
            let biased = recv_back_biased.recv().unwrap();
            assert_eq!(Brc::biased_and_shared_counts(&biased), (Ok(1), 1));
            // after this, only the shared count is live, so we need to unbias it
            drop(biased);
            assert_eq!(counter.load(Ordering::SeqCst), 0);
            finish_unbias.wait(); // tell the other side we have dropped the biased count
        });
        scope.spawn(|| {
            let recv_biased = recv_biased;
            let send_back_biased = send_back_biased;
            // first we receive the biased reference,
            let biased = recv_biased.recv().unwrap();
            assert_eq!(
                Brc::biased_and_shared_counts(&biased),
                (Err(WrongThread), 0)
            );
            // clone it to get a shared reference
            let shared = Brc::clone(&biased);
            assert_eq!(
                Brc::biased_and_shared_counts(&shared),
                (Err(WrongThread), 1)
            );
            // send the biased reference back
            send_back_biased.send(biased).unwrap();
            // wait for the other thread to acknowledge the drop,
            finish_unbias.wait();
            // at which point the shared reference should no longer be biased,
            // and we should have a single shared reference
            assert_eq!(Brc::biased_and_shared_counts(&shared), (Err(NotBiased), 1));
            drop(shared);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        });
    });
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
