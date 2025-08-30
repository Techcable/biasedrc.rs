use core::sync::atomic::AtomicU32;
use core::marker::PhantomPinned;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use threadid::LiveThreadId;

mod threads;

/// The object header for a [`Brc`].
///
/// Seperated from the [`Brc`] to allow more detailed control of allocation.
///
/// # Safety
/// Calling [`Self::decrement`] incorrectly can lead to use-after-free.
/// It is assumed that a header will not be
///
/// The address of the header is relied upon in some cases,
/// so it must never move in memory after it is constructed.
/// This is not statically
///
/// [`Brc`]: crate::Brc
#[repr(C)]
pub struct RawBrcHeader {
    biased_word: AtomicU32,
    shared_word: AtomicU32,
    marker: PhantomPinned,
}
impl RawBrcHeader {
    /// Initialize the header, biasing towards the current thread.
    ///
    /// # Safety
    /// The resulting header must be pinned in-memory and never moved.
    pub fn init() {
        let index  = LiveThreadId::current().index();
    }
    /// Increment the object's strong count.
    ///
    /// # Safety
    /// This is a safe operation for the same reason that [`std::mem::forget`] is.
    pub fn increment_strong(self: Pin<&mut Self>) {
        let owner_tid = self.biased_word.load(Ordering::Relaxed) as usize;
        let this_tid = LiveThreadId::current().index();
    }
}
unsafe impl Send for RawBrcHeader {}
unsafe impl Sync for RawBrcHeader {}

struct BiasedWord {
    bits: u32
}
impl BiasedWord {
    fn tid(&self)
}

