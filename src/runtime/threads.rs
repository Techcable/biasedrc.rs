use crate::runtime::QueuedObject;
use arbitrary_int::prelude::*;
use atomic::Atomic;
use core::ptr::NonNull;
use core::sync::atomic::AtomicBool;
use crossbeam_queue::SegQueue;
use parking_lot::{RwLock, RwLockReadGuard, RwLockUpgradableReadGuard, RwLockWriteGuard};
use std::cell::Cell;
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::thread::AccessError;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct UniqueThreadId(NonZeroUsize);
impl UniqueThreadId {
    const MIN: UniqueThreadId = UniqueThreadId(NonZeroUsize::new(1).unwrap());
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
    /// After this state is observed,
    /// the first thread to successfully acquire the lock is expected to perform the cleanup.
    ///
    /// This state implies that [`LocalThreadState::current`] will never succeed again,
    /// ensuring the biased thread will not manipulate the shared count.
    Dying,
    /// Indicates that the thread is dead and has been cleaned up.
    Dead,
}

/// The choice of concurrent queue means we only need a read lock on [`SharedThreadState`] to append to the queue.
///
/// This improves concurrency but increases memory usage and the cost of a append operation.
/// The [`LocalThreadState`] holds a cached pointer to the queue,
/// as the lock only prevents destruction and destruction will not happen while [`LocalThreadState`] is live.
type ObjectQueue = SegQueue<QueuedObject>;

/// The state shared across multiple threads and protected by a [`RwLock`].
///
/// Used to prevent use after free for the queue.
/// If the [`ThreadStateFlag::Dying`] flag is set,
/// it ensures that only one thread actually does the destruction.
enum SharedThreadState {
    Live { queued_objects: Box<ObjectQueue> },
    Dead,
}

/// Information about a particular thread participating in BRC,
/// which is safe to share with other threads.
///
/// The existence of this type implies the existence of .
///
/// This object can never be destroyed,
/// because there may still be live objects referencing it even after the thread has died.
pub struct SharedThreadInfo {
    /// The unique identifier for this thread.
    _id: UniqueThreadId,
    /// The short id of this thread, or `None` if it cannot fit into a [`ShortThreadId`].
    ///
    /// If this is `None`, then the thread cannot participate in biased reference counting.
    short_id: ShortThreadId,
    /// Indicates the state of the thread.
    state_flag: Atomic<ThreadStateFlag>,
    /// The current state of the thread, potentially including the queue.
    ///
    /// Protected by a lock to prevent unexpected state transitions.
    shared_state: RwLock<SharedThreadState>,
}
impl SharedThreadInfo {
    #[inline]
    pub fn get_by_id(id: ShortThreadId) -> Option<&'static SharedThreadInfo> {
        // SAFETY: Known to be nonzero, so subtraction will always work
        let index = unsafe { id.0.get().unchecked_sub(1) };
        THREADS.get(index as usize)?.ok()
    }

    #[cold]
    pub unsafe fn queue_object(
        &self,
        object: QueuedObject,
    ) -> Result<(), InvalidSharedThreadError> {
        // don't do an upgradable_read here because that reduces concurrency
        // acquiring a read lock here means that the thread will not die while we are working
        let lock = self.shared_state.read();
        let current_flag = self.state_flag.load(Ordering::Relaxed);
        match current_flag {
            ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => {
                let SharedThreadState::Live { queued_objects } = &*lock else {
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
                let lock = self.shared_state.upgradable_read();
                let current_flag = self.state_flag.load(Ordering::Relaxed);
                match current_flag {
                    ThreadStateFlag::Dying => {
                        let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
                        // SAFETY: We have been requested to destroy the info
                        unsafe {
                            self.do_destroy_shared(&mut lock);
                        }
                        RwLockWriteGuard::unlock_fair(lock);
                        // flag unchanged, so it is our responsibility to fix it
                        Err(InvalidSharedThreadError::DeadOrDying)
                    }
                    ThreadStateFlag::Dead => {
                        RwLockUpgradableReadGuard::unlock_fair(lock);
                        // someone else dealt with the death, so we are done
                        Err(InvalidSharedThreadError::DeadOrDying)
                    }
                    ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => {
                        unreachable!("impossible to transition from dying to {current_flag:?}")
                    }
                }
            }
            ThreadStateFlag::Dead => Err(InvalidSharedThreadError::DeadOrDying),
        }
    }

    /// Destroy the shared thread info.
    ///
    /// # Safety
    /// The thread must actually be dead or dying,
    /// otherwise concurrent access to the biased counter will trigger undefined behavior.
    #[cold]
    unsafe fn do_destroy_shared(&self, lock: &mut RwLockWriteGuard<'_, SharedThreadState>) {
        match &**lock {
            SharedThreadState::Live { queued_objects } => {
                THIS_THREAD_STATE_FAST.set(LocalThreadFastState::DeadOrDying);
                while let Some(object) = queued_objects.pop() {
                    // SAFETY: We are either the biased thread in a destructor,
                    // or it has requested death and we have exclusive permission to execute it
                    // Either way we cannot conflict with RC operations on that thread
                    unsafe {
                        super::explicit_merge(self.short_id, object);
                    }
                }
                let old_state = self
                    .state_flag
                    .swap(ThreadStateFlag::Dead, Ordering::Relaxed);
                match old_state {
                    ThreadStateFlag::Live
                    | ThreadStateFlag::QueuedObjects
                    | ThreadStateFlag::Dying => {}
                    ThreadStateFlag::Dead => unreachable!("{old_state:?}"),
                }
                **lock = SharedThreadState::Dead;
            }
            SharedThreadState::Dead => unreachable!("already dead"),
        }
    }
}

/// Information local to the biased thread.
///
/// The existence of this type implies the thread can participate in BRC.
/// All of this information is stored directly in the TLS without boxing.
///
/// Dropping this value deregisters the .
pub struct LocalThreadState {
    shared_info: &'static SharedThreadInfo,
    short_id: ShortThreadId,
    /// Holds a cached pointer to the queue,
    /// which allows this thread to avoid acquiring a read lock on the [`SharedThreadState`].
    ///
    /// This is possible because the thread cannot be destroyed while the [`LocalThreadState`] is live.
    /// The lock only exists to prevent use after free,
    /// and the queue is otherwise fully concurrent.
    ///
    /// This has the logical lifetime `&'self`,
    /// but we use a pointer since that is not currently possible to express.
    _cached_queue: NonNull<ObjectQueue>,
}
impl LocalThreadState {
    #[inline]
    pub fn short_id(&self) -> ShortThreadId {
        self.short_id
    }
    /// Access the current thread info inside the specified closure.
    ///
    /// # Safety
    /// This function is safe to invoke.
    ///
    /// There is a potential race if the destructor of [`LocalThreadState`]
    /// is invoked while the closure is running,
    /// as [`THIS_THREAD_STATE_FAST`] would be invalidated.
    ///
    /// This case cannot actually happen,
    /// as if the thread is live at the beginning of the closure,
    /// it will still be live by the end.
    /// This is similar reasoning for why [`std::thread::LocalKey::with`] is safe.
    ///
    /// It is well-defined to invoke this after the destructor is finished or in-progress.
    /// The state is updated appropriately at the beginning of the destructor,
    /// before any calls are made to external functions.
    /// This is necessary as destroying a thread could invoke arbitrary user-defined destructors,
    /// which could recursively call back into the runtime.
    #[inline]
    pub fn with_current<R>(
        func: impl FnOnce(&LocalThreadState) -> R,
    ) -> Result<R, LocalThreadAccessError> {
        match THIS_THREAD_STATE.try_with(|this| match this {
            Ok(state) => Ok(func(state)),
            Err(error) => Err(*error),
        }) {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(ThreadStateInitError::IdOverflow(cause))) => {
                Err(LocalThreadAccessError::IdOverflow(cause))
            }
            Ok(Err(ThreadStateInitError::AlreadyDied)) | Err(AccessError { .. }) => {
                Err(LocalThreadAccessError::Dead)
            }
        }
    }
    /// Return a reference to the current thread's short ID,
    /// or an error if the local thread is uninitialized or invalid (cannot participate in BRC)
    #[inline]
    pub fn existing_short_id() -> Result<ShortThreadId, LocalThreadAccessError> {
        match THIS_THREAD_STATE_FAST.get() {
            LocalThreadFastState::Uninit => Err(LocalThreadAccessError::Uninitialized),
            LocalThreadFastState::DeadOrDying => Err(LocalThreadAccessError::Dead),
            LocalThreadFastState::Active { short_id } => Ok(short_id),
        }
    }
}
impl Drop for LocalThreadState {
    fn drop(&mut self) {
        assert_eq!(
            THIS_THREAD_STATE_FAST.replace(LocalThreadFastState::DeadOrDying),
            LocalThreadFastState::Active {
                short_id: self.short_id
            }
        );
        // now attempt to destroy the thread
        match self.shared_info.shared_state.try_write() {
            Some(mut success) => {
                // SAFETY: We are the owning thread
                unsafe {
                    self.shared_info.do_destroy_shared(&mut success);
                }
            }
            None => {
                let old_state = self
                    .shared_info
                    .state_flag
                    .swap(ThreadStateFlag::Dying, Ordering::SeqCst);
                match old_state {
                    ThreadStateFlag::Dying | ThreadStateFlag::Dead => {
                        panic!("cannot kill a thread in state {old_state:?}")
                    }
                    ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => {}
                }
            }
        }
    }
}
/// The "fast" version of [`LocalThreadState`],
/// which does not require a destructor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LocalThreadFastState {
    /// Indicates that the thread is dead or being destroyed.
    DeadOrDying,
    /// Indicates that the thread has not been fully initialized yet.
    ///
    /// This can happen because the [`THIS_THREAD_STATE`] TLS hasn't been lazy-initialized yet,
    /// or because there is an [`ThreadIdOverflowError`].
    Uninit,
    Active {
        short_id: ShortThreadId,
    },
}
thread_local! {
    /// Information on this thread's participation in biased reference counting.
    static THIS_THREAD_STATE: Result<LocalThreadState, ThreadStateInitError> = nounwind::abort_unwind(init_thread);
    /// A faster version of [`THIS_THREAD_STATE`] which can skip initialization and destruction checks.
    ///
    /// This also caries a secondary role of of preventing [`THIS_THREAD_STATE`] from re-initializing after daeth.
    static THIS_THREAD_STATE_FAST: Cell<LocalThreadFastState> = const { Cell::new(LocalThreadFastState::Uninit) };
}
/// If this is true, we have run out of valid thread ids.
///
/// This avoids expanding the [`THREADS`] vector when we are out of ids.
static SHORT_THREAD_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);
static THREADS: boxcar::Vec<Result<&'static SharedThreadInfo, ThreadIdOverflowError>> =
    boxcar::Vec::new();

fn init_thread() -> Result<LocalThreadState, ThreadStateInitError> {
    match THIS_THREAD_STATE_FAST.get() {
        LocalThreadFastState::DeadOrDying => {
            // this can happen if the TLS is destroyed then re-initialized.
            // We do not want to deal with this scenario as we may have transferred ownership.
            return Err(ThreadStateInitError::AlreadyDied);
        }
        LocalThreadFastState::Uninit => {} // exactly as expected
        old_state @ LocalThreadFastState::Active { .. } => {
            panic!("Thread already initialized with state {old_state:?}")
        }
    }
    if SHORT_THREAD_IDS_EXHAUSTED.load(Ordering::Acquire) {
        Err(ThreadIdOverflowError.into())
    } else {
        let queued_objects = Box::new(SegQueue::new());
        let cached_queue = NonNull::from(queued_objects.deref());
        let index = THREADS.push_with(|id| {
            let id = UniqueThreadId::from_index(id);
            match ShortThreadId::try_from(id) {
                Ok(short_id) => Ok(Box::leak(Box::new(SharedThreadInfo {
                    _id: id,
                    short_id,
                    state_flag: Atomic::new(ThreadStateFlag::Live),
                    shared_state: RwLock::new(SharedThreadState::Live { queued_objects }),
                }))),
                Err(ThreadIdOverflowError) => {
                    // prevent other threads from attempting this
                    SHORT_THREAD_IDS_EXHAUSTED.store(true, Ordering::Release);
                    Err(ThreadIdOverflowError)
                }
            }
        });
        let shared_info = THREADS[index]?;
        assert_eq!(
            THIS_THREAD_STATE_FAST.replace(LocalThreadFastState::Active {
                short_id: shared_info.short_id
            }),
            LocalThreadFastState::Uninit
        );
        Ok(LocalThreadState {
            shared_info,
            short_id: shared_info.short_id,
            _cached_queue: cached_queue,
        })
    }
}

#[derive(Debug, thiserror::Error, Copy, Clone)]
pub enum ThreadStateInitError {
    #[error("Thread has already died so cannot be re-initialized")]
    AlreadyDied,
    #[error("Failed to initialize thread: {0}")]
    IdOverflow(#[from] ThreadIdOverflowError),
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidSharedThreadError {
    #[error("Thread is either dead or dying")]
    DeadOrDying,
}

#[derive(Copy, Clone, Debug, thiserror::Error, Eq, PartialEq)]
#[error(
    "Thread ID overflows {} bits, so cannot participate in biased reference counting",
    ShortThreadId::BITS
)]
pub struct ThreadIdOverflowError;

/// Indicates an error occurred calling [`LocalThreadState::with_current`]
/// or [`LocalThreadState::existing_short_id`].
#[derive(Debug, thiserror::Error, Clone, Eq, PartialEq)]
pub enum LocalThreadAccessError {
    #[error("Local thread has not been initialized yet")]
    Uninitialized,
    #[error("Local thread is either dead or dying")]
    Dead,
    #[error("Local thread cannot participate in biased reference counting: {0}")]
    IdOverflow(#[from] ThreadIdOverflowError),
}

/// A short thread identifier, which is guaranteed to fit in 18 bits,
/// with the zero value reserved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct ShortThreadId(NonZeroU32);
impl ShortThreadId {
    pub const BITS: u32 = 18;
    pub const MAX: u18 = u18::MAX;

    #[inline]
    pub const fn new(x: u18) -> Option<Self> {
        // NOTE: Cannot use ? in const fn
        if x.value() != 0 {
            // SAFETY: Just checked to be nonzero
            Some(unsafe { ShortThreadId(NonZeroU32::new_unchecked(x.value())) })
        } else {
            None
        }
    }

    #[inline]
    pub const fn value(self) -> u18 {
        // SAFETY: Known to fit into 18 bits
        unsafe { u18::new_unchecked(self.0.get()) }
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
