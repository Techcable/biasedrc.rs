use std::collections::BinaryHeap;
use std::fmt::Binary;
use std::num::{NonZero, NonZeroU32, NonZeroUsize};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};
use std::sync::{Mutex, PoisonError, Weak};
use crossbeam_queue::SegQueue;
use crossbeam_utils::atomic::AtomicCell;
use crate::raw::BiasedWord;

pub struct UniqueThreadId(pub usize);

enum ThreadState {
    Alive,
    Dying,
    Dead,
}

struct LiveThreadState {
    id: UniqueThreadId,
    /// The short id of this thread, or `None` if it cannot fit into a `ShortThreadId`.
    ///
    /// If this is `None`, then the thread will .
    short_id: Option<ShortThreadId>,
    state: AtomicCell<LiveThreadState>,
    queued_objects: SegQueue<NonNull<BiasedWord>>,
}

static THREADS: boxcar::Vec<Weak<LiveThreadState>> = boxcar::Vec::new();

/// Indicates that a thread is not supported.
///
/// This error can only happen.
#[derive(Debug)]
pub struct UnsupportedThreadError(());

/// A short thread identifier, which is guaranteed to fit in 18 bits,
/// with one value reserved.
#[derive(Copy, Clone, Debug)]
pub struct ShortThreadId(u32);
impl ShortThreadId {
    pub const BITS: u32 = 18;
    /// The maximum valid thread id.
    pub const MAX: ShortThreadId = ShortThreadId(Self::RESERVED - 1);
    pub const RESERVED: u32 = (1u32 << 18) - 1;

    #[inline]
    pub const fn new(x: u32) -> Option<Self> {
        if x <= Self::MAX.0 {
            Some(ShortThreadId(x))
        } else {
            None
        }
    }

    #[inline]
    pub const fn value(self) -> u32 {
        self.0
    }
}
impl TryFrom<UniqueThreadId> for ShortThreadId {
    type Error = UnsupportedThreadError;

    fn try_from(value: UniqueThreadId) -> Result<Self, Self::Error> {
        u32::try_from(value.0).ok()
            .and_then(Self::new)
            .ok_or(UnsupportedThreadError(()))
    }
}
