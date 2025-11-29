//! Basic smoke tests.

use biasedrc::Brc;
use std::cell::Cell;

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
