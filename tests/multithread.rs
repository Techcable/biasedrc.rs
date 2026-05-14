//! Tests the interactions between multiple threads.
use biasedrc::Brc;
use std::sync::Barrier;
use std::sync::atomic::Ordering;

use portable_atomic::AtomicU32;

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
///
/// This corresponds to [`BehaviorAfterMerge::ImmediateDestruction`].
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

/// Tests multiple threads,
/// scoped so that it is necessary to add to the queue and merge reference counts.
///
/// This corresponds to [`BehaviorAfterMerge::StillShared`].
#[test]
fn requires_merge_then_still_shared() {
    let drop_counter = AtomicU32::new(0);
    let (sender, receiver) = crossbeam_channel::bounded(0);
    let begin_collection = Barrier::new(2);
    let end_collection = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let sender = sender;
            let biased = Brc::new(DropCounter(&drop_counter));
            sender.send([Brc::clone(&biased), biased]).unwrap();
            begin_collection.wait();
            biasedrc::collect_force();
            end_collection.wait();
        });
        scope.spawn(|| {
            let receiver = receiver;
            let [biased1, biased2] = receiver.recv().unwrap();
            assert_eq!(Brc::shared_count(&biased1), 0);
            drop(biased1); // will make the shared count negative, requiring queuing
            assert_eq!(Brc::shared_count(&biased2), -1);
            begin_collection.wait();
            end_collection.wait();
            assert_eq!(Brc::shared_count(&biased2), 1);
            assert_eq!(drop_counter.load(Ordering::SeqCst), 0);
            drop(biased2);
            assert_eq!(drop_counter.load(Ordering::SeqCst), 1);
        });
    });
}

#[derive(Default, Copy, Clone, Debug)]
enum BehaviorAfterMerge {
    /// This is the default
    #[default]
    ImmediateDestruction,
    StillShared,
}

#[test]
fn requires_merge_after_thread_death() {
    requires_merge_after_thread_death_with(BehaviorAfterMerge::ImmediateDestruction);
}

#[test]
fn requires_merge_after_thread_death_then_still_shared() {
    requires_merge_after_thread_death_with(BehaviorAfterMerge::StillShared);
}

/// Tests multiple threads,
/// scoped so that it is necessary to add to the queue and merge reference counts
/// after the thread has already died.
///
/// There are two cases: One where we have to execute the destructor immediately after merge
/// ([`BehaviorAfterMerge::ImmediateDestruction`]
/// and one where shared references are still live so we do the destruction later ([`BehaviorAfterMerge::StillShared`]).
fn requires_merge_after_thread_death_with(mode: BehaviorAfterMerge) {
    let counter = AtomicU32::new(0);
    let (sender, receiver) = crossbeam_channel::bounded(0);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            let sender = sender;
            let obj = Brc::new(DropCounter(&counter));
            // send the biased reference to the other thread
            sender.send(obj).unwrap();
        });
        scope.spawn(|| {
            let receiver = receiver;
            // take ownership the biased Brc that the first thread created
            let biased = receiver.recv().unwrap();
            let shared = match mode {
                BehaviorAfterMerge::ImmediateDestruction => None,
                BehaviorAfterMerge::StillShared => Some(Brc::clone(&biased)),
            };
            // wait until the first thread dies
            first.join().expect("Failed to join first thread");
            assert_eq!(counter.load(Ordering::SeqCst), 0);
            // this will drop the shared count to -1, necessitating a merge
            drop(biased);
            // the merge should have been performed immediately since the first thread has died
            assert_eq!(
                counter.load(Ordering::SeqCst),
                match mode {
                    BehaviorAfterMerge::ImmediateDestruction => 1,
                    BehaviorAfterMerge::StillShared => 0,
                }
            );
            drop(shared);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        });
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
