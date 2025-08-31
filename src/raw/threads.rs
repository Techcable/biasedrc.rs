use atomic::Atomic;
use core::ptr::NonNull;
use core::sync::atomic::AtomicBool;
use crossbeam_queue::SegQueue;
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockUpgradableReadGuard, RwLockWriteGuard};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::Ordering;

use crate::raw::{BiasedWord, QueuedObject};

#[derive(Copy, Clone, Debug)]
#[repr(transparent)]
pub struct UniqueThreadId(NonZeroUsize);
impl UniqueThreadId {
    const MIN: UniqueThreadId = UniqueThreadId({
        // SAFETY: One is not zero
        unsafe { NonZeroUsize::new_unchecked(1) }
    });
    #[inline]
    #[track_caller]
    pub fn from_index(index: usize) -> Self {
        UniqueThreadId(
            Self::MIN
                .0
                .checked_add(index)
                .expect("impossible to have more than usize::MAX - 1 threads"),
        )
    }
}

#[derive(Copy, Clone, Debug, bytemuck::NoUninit)]
#[repr(u8)]
enum ThreadStateFlag {
    /// Indicates that a thread is alive,
    /// but has no queued objects.
    Live,
    /// Indicates that a thread is both alive and has queued objects.
    QueuedObjects,
    /// Indicates the thread needs to die,
    /// and cleanup code must be executed.
    ///
    /// Used to avoid blocking in a thread destructor.
    /// Once the destructor completes,
    /// calling `LiveThreadState::current` will return an error,
    /// ensuring the biased thread will not manipulate the shared count.
    Dying,
    Dead,
    InvalidId,
}
impl ThreadStateFlag {
    fn is_live(&self) -> bool {
        matches!(self, ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects)
    }
}

enum LiveThreadState {
    /// Indicates the thread has an invalid ID,
    /// so cannot participate in based reference counting.
    InvalidId,
    Live {
        queued_objects: Box<SegQueue<QueuedObject>>,
    },
    Dead,
}
pub struct ThreadInfo {
    id: UniqueThreadId,
    /// The short id of this thread, or `None` if it cannot fit into a `ShortThreadId`.
    ///
    /// If this is `None`, then the thread cannot participate in biased reference counting.
    short_id: Option<ShortThreadId>,
    state_flag: Atomic<ThreadStateFlag>,
    /// The current state of the thread, potentially including the queue.
    ///
    /// Protected by a lock to prevent unexpected state transitions.
    state: RwLock<LiveThreadState>,
}
impl ThreadInfo {
    pub fn short_id(&self) -> Option<ShortThreadId> {}
    #[inline]
    pub fn current() -> Result<&'static ThreadInfo, InvalidThreadError> {
        THIS_THREAD
            .try_with(|x| x.ok_or(InvalidThreadError::IdOverflow))
            .map_err(|_| InvalidThreadError::DeadOrDying)
            .flatten()
    }

    #[cold]
    pub unsafe fn queue_object(&self, object: QueuedObject) -> Result<(), InvalidThreadError> {
        // don't do an upgradable_read here because that reduces concurrency
        let lock = self.state.read();
        let current_flag = self.state_flag.load(Ordering::Relaxed);
        match *current_flag {
            ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => {
                let LiveThreadState::Live { ref queued_objects } = &*lock else {
                    unreachable!("flag doesn't match state")
                };
                queued_objects.push(object);
                // It's fine to do a relaxed load because we don't care when the biased thread acknowledges.
                // Also, releasing the lock will do release fence anyway.
                self.state_flag
                    .store(ThreadStateFlag::QueuedObjects, Ordering::Relaxed);
                RwLockReadGuard::unlock_fair(lock);
                Ok(())
            }
            ThreadStateFlag::Dying => {
                drop(lock); // don't care if this unlock is fair
                let lock = self.state.upgradable_read();
                let current_flag = self.state_flag.load(Ordering::Relaxed);
                match *current_flag {
                    ThreadStateFlag::Dying => Err(InvalidThreadError::DeadOrDying),
                    ThreadStateFlag::Dead => {
                        // someone else dealt with the death, so we are done
                        Err(InvalidThreadError::DeadOrDying)
                    }
                    ThreadStateFlag::Live
                    | ThreadStateFlag::QueuedObjects
                    | ThreadStateFlag::InvalidId => {
                        unreachable!("impossible to transition from dying to {current_flag:?}")
                    }
                }
            }
        }
    }

    /// Destroy the thread
    ///
    /// # Safety
    /// The thread must actually be dead or dying,
    /// otherwise concurrent access to the biased counter will trigger undefined behavior.
    #[cold]
    unsafe fn do_destroy(&self, lock: RwLockWriteGuard<'_, LiveThreadState>) {
        match &mut *lock {
            LiveThreadState::Live { queued_objects } => for object in queued_objects {},
            LiveThreadState::Dead => unreachable!("already dead"),
            LiveThreadState::InvalidId => unreachable!("invalid id"),
        }
    }
}
static HAS_QUEUED_OBJECTS: AtomicBool = AtomicBool::new(false);

thread_local! {
    static THIS_THREAD: Option<&'static ThreadInfo> = init_thread();
}
/// If this is true, we have run out of valid thread ids.
static THREAD_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);
static THREADS: boxcar::Vec<ThreadInfo> = boxcar::Vec::new();

fn init_thread() -> Option<&'static ThreadInfo> {
    if THREAD_IDS_EXHAUSTED.load(Ordering::Acquire) {
        None
    } else {
        let index = THREADS.push_with(|id| {
            let id = UniqueThreadId::from_index(id);
            match ShortThreadId::try_from(id) {
                Ok(short_id) => ThreadInfo {
                    id,
                    short_id: Some(short_id),
                    state_flag: Atomic::new(ThreadStateFlag::Live),
                    state: Mutex::new(LiveThreadState::Live {
                        queued_objects: Vec::new(),
                    }),
                },
                Err(ThreadIdOverflowError) => ThreadInfo {
                    id,
                    short_id: None,
                    state_flag: Atomic::new(ThreadStateFlag::InvalidId),
                    state: Mutex::new(LiveThreadState::InvalidId),
                },
            }
        });
        Some(&THREADS[index])
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidThreadError {
    #[error("Threa is either dying or dead")]
    DeadOrDying,
    #[error("{}", ThreadIdOverflowError)]
    IdOverflow,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Thread ID overflows {} bits, so cannot be supported",
    ShortThreadId::BITS
)]
pub struct ThreadIdOverflowError;

/// A short thread identifier, which is guaranteed to fit in 18 bits,
/// with the zero value reserved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct ShortThreadId(NonZeroU32);
impl ShortThreadId {
    pub const BITS: u32 = 18;
    pub const MAX: arbitrary_int::u18 = arbitrary_int::u18::MAX;

    #[inline]
    pub const fn new(x: arbitrary_int::u18) -> Option<Self> {
        // NOTE: Cannot use ? in const fn
        if x.value() != 0 {
            // SAFETY: Just checked to be nonzero
            Some(unsafe { ShortThreadId(NonZeroU32::new_unchecked(x.value())) })
        } else {
            None
        }
    }

    #[inline]
    pub const fn value(self) -> arbitrary_int::u18 {
        // SAFETY: Known to fit into 18 bits
        unsafe { arbitrary_int::u18::new_unchecked(self.0.get()) }
    }
}
impl TryFrom<UniqueThreadId> for ShortThreadId {
    type Error = ThreadIdOverflowError;

    #[inline]
    fn try_from(value: UniqueThreadId) -> Result<Self, Self::Error> {
        let value = NonZeroU32::try_from(value.0).map_err(|_| ThreadIdOverflowError)?;
        if value.get() <= Self::MAX.value() {
            Ok(ShortThreadId(value))
        } else {
            Err(ThreadIdOverflowError)
        }
    }
}
