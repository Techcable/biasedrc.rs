//! Tests the interactions between multiple threads.
use biasedrc::Brc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicU32, Ordering};

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

/// Tests multiple threads, scoped so that it is necessary to add to the queue and merge reference counts.
#[test]
fn requires_merge() {
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
fn dominated_biased() {
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
fn unbias() {
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
