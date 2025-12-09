#![allow(clippy::disallowed_types, reason = "Need Arc to hold queue")]
use crate::runtime::QueuedObject;
use alloc::sync::{Arc, Weak};
use arbitrary_int::prelude::*;
use atomic::Atomic;
use core::cell::Cell;
use core::mem::ManuallyDrop;
use core::num::{NonZeroU16, NonZeroUsize};
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use crossbeam_queue::SegQueue;
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, bytemuck::NoUninit)]
#[repr(u8)]
enum ThreadStateFlag {
    /// Indicates that a thread is alive,
    /// but has no queued objects.
    Live = 0,
    /// Indicates that a thread is both alive and has queued objects.
    QueuedObjects,
    /// Indicates the thread is currently executing its destructor.
    ///
    /// While this state is present, the death lock must not be acquired.
    /// Otherwise, other threads could block the thread destructor.
    ///
    /// This state implies that [`LocalThreadState::current`] will never succeed again,
    /// ensuring the biased thread will not manipulate the shared count.
    Dying,
    /// Indicates that the thread is dead and has finished executing the destructor.
    ///
    /// This means that the queue will never be emptied,
    /// but the .
    Dead,
}
impl ThreadStateFlag {
    #[inline]
    pub fn is_live(self) -> bool {
        match self {
            ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => true,
            ThreadStateFlag::Dying | ThreadStateFlag::Dead => false,
        }
    }
    #[inline]
    pub fn is_dead_or_dying(self) -> bool {
        !self.is_live()
    }
}

/// The queue of objects to be merged by the biased thread.
///
/// # Safety
/// If this queue is [dropped](drop),
/// the remaining queued objects will be implicitly processed.
/// This the
pub struct ObjectQueue {
    short_id: ShortThreadId,
    inner: SegQueue<QueuedObject>,
}
impl ObjectQueue {
    /// Create a new queue,
    /// guaranteeing drop will not occur while the biased thread is live.
    ///
    /// # Safety
    /// The [`Drop`] implementation of the queue must not be called unless
    /// [`super::explicit_merge`] can be safely called on all the objects in the queue.
    #[inline]
    pub unsafe fn new(short_id: ShortThreadId) -> ObjectQueue {
        ObjectQueue {
            short_id,
            inner: SegQueue::new(),
        }
    }

    /// Push an object onto the queue.
    ///
    /// # Safety
    /// The queued object is valid.
    ///
    /// Should only be called by [`SharedThreadInfo::queue_object`].
    #[inline]
    pub unsafe fn push(&self, object: QueuedObject) {
        self.inner.push(object);
    }

    /// Empty the queue by repeatedly calling [`super::explicit_merge`].
    ///
    /// # Safety
    /// Same requirements as [`super::explicit_merge`].
    /// In particular, if the biased thread is live,
    ///that is the only thread this can be done on.
    #[cold]
    #[inline(never)]
    unsafe fn do_process(&self) {
        while let Some(object) = self.inner.pop() {
            // SAFETY: Caller guarantees this is a safe to do
            unsafe {
                super::explicit_merge(self.short_id, object);
            }
        }
    }
}
impl Drop for ObjectQueue {
    fn drop(&mut self) {
        // SAFETY: Our destruction can only happen once it is safe to process the objects
        // This is guaranteed by the caller of `Self::new`
        unsafe { self.do_process() }
    }
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
    /// The queue of objects that need to be processed.
    ///
    /// This will be freed on thread death.
    queued_objects: Weak<ObjectQueue>,
}
impl SharedThreadInfo {
    #[inline]
    pub fn get_by_id(id: ShortThreadId) -> Option<&'static SharedThreadInfo> {
        THREADS.get(id.index())?.ok()
    }

    /// Queue the object if the biased thread is live,
    /// or call [`super::explicit_merge`] if the thread is dead.
    ///
    /// # Safety
    /// Must be called at most once per object,
    /// or else a data race could occur.
    ///
    /// The queued object must be valid.
    ///
    /// # Panics
    /// This function is infallible, but may abort if internal corruption is discovered.
    #[cold]
    #[inline(never)]
    pub unsafe fn queue_object(&self, object: QueuedObject) {
        nounwind::abort_unwind(|| {
            match self.queued_objects.upgrade() {
                Some(queue) => {
                    // SAFETY: Caller guarantees the queue is valid
                    // Either the thread is live and will later process it,
                    // or is in the process of being destroyed.
                    // In the latter case, the queue destructor will handle things.
                    unsafe {
                        queue.push(object);
                    }
                    // if we are still "live", update the state to indicate the queue is nonempty.
                    // We don't care about promptness so can use a relaxed ordering.
                    // This does not matter if we are being destroyed
                    // since the queue destructor will process the object.
                    let _ = self.state_flag.compare_exchange(
                        ThreadStateFlag::Live,
                        ThreadStateFlag::QueuedObjects,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }
                None => {
                    let this_state = self.state_flag.load(Ordering::Acquire);
                    assert!(
                        this_state.is_dead_or_dying(),
                        "thread is {this_state:?} but has no queue"
                    );
                    // SAFETY: The thread is dead or dying, so it cannot mutate the biased count.
                    // This method is called at most once per object,
                    // so it cannot possibly race with any other threads.
                    unsafe {
                        super::explicit_merge(self.short_id, object);
                    }
                }
            }
        });
    }
}

/// Information local to the biased thread.
///
/// The existence of this type implies the thread can participate in BRC.
/// All of this information is stored directly in the TLS without boxing.
pub struct LocalThreadState {
    shared_info: &'static SharedThreadInfo,
    short_id: ShortThreadId,
    /// Holds the primary strong reference to the object queue,
    /// which will be destroyed and implicitly emptied on thread death.
    ///
    /// We use an [`Arc`] so that the [`Weak`] in the [`SharedThreadInfo`]
    /// is automatically updated on destruction.
    /// It is possible that the queue will be kept alive slightly past thread death
    /// by the [`SharedThreadInfo::queue_object`] function.
    /// This is fine as long as it is not dropped before the thread death.
    ///
    /// This is [`ManuallyDrop`] to be clearer about the invariants of the [`ObjectQueue`].
    queue: ManuallyDrop<Arc<ObjectQueue>>,
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
    /// This is similar reasoning for why [`core::thread::LocalKey::with`] is safe.
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
        match THIS_THREAD_STATE_FAST.with(|fast| (fast.status.get(), fast.short_id.get())) {
            (LocalThreadStatus::Uninit, None) => Err(LocalThreadAccessError::Uninitialized),
            (LocalThreadStatus::DeadOrDying, None) => Err(LocalThreadAccessError::Dead),
            (LocalThreadStatus::Active, Some(short_id)) => Ok(short_id),
            (_, Some(_)) | (LocalThreadStatus::Active, None) => {
                // SAFETY: Thread state is invalid
                unsafe { core::hint::unreachable_unchecked() }
            }
        }
    }

    #[inline]
    pub fn currently_needs_collect() -> bool {
        THIS_THREAD_STATE_FAST.with(|fast| {
            // compiles to comparison against zero
            !matches!(
                fast.shared_state_flag.get().load(Ordering::Relaxed),
                ThreadStateFlag::Live
            )
        })
    }

    /// The slow path for [`crate::collect`].
    ///
    /// This is a separate function to indicate that it is a cold path and to favor outlining.
    #[cold]
    #[inline(never)]
    pub(super) fn collect_slow() {
        // we ignore any access error
        let _ = Self::with_current(|state| {
            nounwind::abort_unwind(|| {
                if std::thread::panicking() {
                    // skip collection if we are panicking (helpful if called by Drop)
                    return;
                }
                // This match compiles into a comparison against zero
                if !matches!(
                    state.shared_info.state_flag.load(Ordering::Relaxed),
                    ThreadStateFlag::Live
                ) {
                    state.collect_force();
                }
            });
        });
    }

    /// Forcibly perform thread-local cleanup operations.
    ///
    /// This requires acquiring a state lock to prevent thread death.
    #[cold]
    #[inline(never)]
    pub fn collect_force(&self) {
        nounwind::abort_unwind(|| {
            let this_state = self.shared_info.state_flag.load(Ordering::Acquire);
            match this_state {
                ThreadStateFlag::Live | ThreadStateFlag::QueuedObjects => {
                    // this can still be destroyed if the state
                    // is changed to dying after we perform the initial read.
                    // In that case, skip processing, as the destructors handled things.
                    let Some(queue) = Weak::upgrade(&self.shared_info.queued_objects) else {
                        return;
                    };
                    loop {
                        // SAFETY: We are the biased thread,
                        // so can safely adjust the RCs without a lock
                        unsafe {
                            queue.do_process();
                        }
                        // Update the state to indicate we have processed things.
                        let _ = self.shared_info.state_flag.compare_exchange(
                            ThreadStateFlag::QueuedObjects,
                            ThreadStateFlag::Live,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        // It is possible a race causes us to make a mistake in the line above:
                        // If we go to sleep after `do_process`
                        // another thread could add to the queue and
                        // cause us to ignore its status flag here.
                        // To avoid this, we check again to see if the queue really is empty.
                        if queue.inner.is_empty() {
                            break;
                        }
                    }
                }
                ThreadStateFlag::Dead | ThreadStateFlag::Dying => {
                    // do nothing, as the destructor is executing
                    // and is responsible for handling things
                }
            }
        });
    }
}
impl Drop for LocalThreadState {
    fn drop(&mut self) {
        // update the local state to indicate we are dead
        // This should prevent this thread from modifying any biased counters
        THIS_THREAD_STATE_FAST.with(|fast| {
            assert_eq!(
                fast.status.replace(LocalThreadStatus::DeadOrDying),
                LocalThreadStatus::Active,
            );
            assert_eq!(fast.short_id.replace(None), Some(self.short_id));
            fast.shared_state_flag.set(&DUMMY_STATE_FLAG);
        });
        // now we can officially switch or shared state flag to "dying"
        let old_state = self
            .shared_info
            .state_flag
            .swap(ThreadStateFlag::Dying, Ordering::SeqCst);
        assert!(old_state.is_live(), "Cannot destroy a {old_state:?} thread");
        // drop the shared reference to the queue
        // This may not run the destructor immediately if a queue_object call is in progress.
        // SAFETY: Performed exactly once
        unsafe { ManuallyDrop::drop(&mut self.queue) };
        // switch shared state to "dead"
        assert_eq!(
            self.shared_info.state_flag.compare_exchange(
                ThreadStateFlag::Dying,
                ThreadStateFlag::Dead,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ),
            Ok(ThreadStateFlag::Dying),
        );
    }
}
/// The status of the local thread state.
///
/// Stored in the [`ThreadStateFlag`].
#[derive(Debug, Copy, Clone, Eq, PartialEq, bytemuck::NoUninit)]
#[repr(u8)]
enum LocalThreadStatus {
    /// Indicates that the thread is dead or being destroyed.
    DeadOrDying,
    /// Indicates that the thread has not been fully initialized yet.
    ///
    /// This can happen because the [`THIS_THREAD_STATE`] TLS hasn't been lazy-initialized yet,
    /// or because there is an [`ThreadIdOverflowError`].
    Uninit,
    Active,
}

static DUMMY_STATE_FLAG: Atomic<ThreadStateFlag> = Atomic::new(ThreadStateFlag::Dying);
/// The "fast" version of [`LocalThreadState`],
/// which does not require a destructor or initializer.
#[derive(Debug)]
pub struct LocalThreadStateFast {
    status: Cell<LocalThreadStatus>,
    short_id: Cell<Option<ShortThreadId>>,
    /// A reference to the thread state flag stored in the [`SharedThreadState`].
    ///
    /// Used to tell if collection needs to be performed.
    ///
    /// If the thread is destroyed or uninitialized, this will be set to [`DUMMY_STATE_FLAG`].
    shared_state_flag: Cell<&'static Atomic<ThreadStateFlag>>,
}
thread_local! {
    /// Information on this thread's participation in biased reference counting.
    static THIS_THREAD_STATE: Result<LocalThreadState, ThreadStateInitError> = nounwind::abort_unwind(init_thread);
    /// A more basic version of [`LocalThreadState`] which is `Copy` and const-initialized.
    ///
    /// The most important purpose is to prevent [`THIS_THREAD_STATE`] from being re-initialized after destruction,
    /// by storing the [`LocalThreadStatus`].
    /// It gives faster access to the [`ShortThreadId`] and the ``needs_collect` flag
    /// without going through a lazy-init check.
    static THIS_THREAD_STATE_FAST: LocalThreadStateFast = const { LocalThreadStateFast {
        status: Cell::new(LocalThreadStatus::Uninit),
        short_id: Cell::new(None),
        shared_state_flag: Cell::new(&DUMMY_STATE_FLAG),
    } };
}
/// If this is true, we have run out of valid thread ids.
///
/// This avoids expanding the [`THREADS`] vector when we are out of ids.
static SHORT_THREAD_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);
static THREADS: boxcar::Vec<Result<&'static SharedThreadInfo, ThreadIdOverflowError>> =
    boxcar::Vec::new();

fn init_thread() -> Result<LocalThreadState, ThreadStateInitError> {
    let old_status = THIS_THREAD_STATE_FAST.with(|fast| fast.status.get());
    match old_status {
        LocalThreadStatus::DeadOrDying => {
            // this can happen if the TLS is destroyed then re-initialized.
            // We do not want to deal with this scenario as we may have transferred ownership.
            return Err(ThreadStateInitError::AlreadyDied);
        }
        LocalThreadStatus::Uninit => {} // exactly as expected
        LocalThreadStatus::Active => {
            panic!("Thread already initialized")
        }
    }
    if SHORT_THREAD_IDS_EXHAUSTED.load(Ordering::Acquire) {
        Err(ThreadIdOverflowError.into())
    } else {
        let mut queued_objects = None;
        let index = THREADS.push_with(|id| {
            let id = UniqueThreadId::from_index(id);
            match ShortThreadId::try_from(id) {
                Ok(short_id) => {
                    // SAFETY: Destructor is only called when the thread dies
                    // or if this function panics
                    // In the latter case, no objects will have been queued.
                    queued_objects = Some(Arc::new(unsafe { ObjectQueue::new(short_id) }));
                    Ok(Box::leak(Box::new(SharedThreadInfo {
                        _id: id,
                        short_id,
                        state_flag: Atomic::new(ThreadStateFlag::Live),
                        queued_objects: Arc::downgrade(queued_objects.as_ref().unwrap()),
                    })))
                }
                Err(ThreadIdOverflowError) => {
                    // prevent other threads from attempting this
                    SHORT_THREAD_IDS_EXHAUSTED.store(true, Ordering::Release);
                    Err(ThreadIdOverflowError)
                }
            }
        });
        let shared_info = THREADS[index]?;
        assert_eq!(
            THIS_THREAD_STATE_FAST.with(|fast| {
                (
                    core::ptr::from_ref(fast.shared_state_flag.replace(&shared_info.state_flag)),
                    fast.status.replace(LocalThreadStatus::Active),
                    fast.short_id.replace(Some(shared_info.short_id)),
                )
            }),
            (
                core::ptr::from_ref(&DUMMY_STATE_FLAG),
                LocalThreadStatus::Uninit,
                None
            )
        );
        Ok(LocalThreadState {
            shared_info,
            short_id: shared_info.short_id,
            queue: ManuallyDrop::new(queued_objects.unwrap()),
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
pub struct ShortThreadId(NonZeroU16);
impl ShortThreadId {
    pub const BITS: u32 = 12;
    pub const MAX: u12 = u12::MAX;

    #[inline]
    pub const fn new(x: u12) -> Option<Self> {
        // NOTE: Cannot use ? in const fn
        if x.value() != 0 {
            // SAFETY: Just checked to be nonzero
            Some(unsafe { ShortThreadId(NonZeroU16::new_unchecked(x.value())) })
        } else {
            None
        }
    }

    #[inline]
    pub const fn value(self) -> u12 {
        // SAFETY: Known to fit into 12 bits
        unsafe { u12::new_unchecked(self.0.get()) }
    }

    #[inline]
    pub const fn index(self) -> usize {
        // SAFETY: Known to be nonzero, so subtraction cannot overflow
        unsafe { self.0.get().unchecked_sub(1) as usize }
    }
}
impl TryFrom<UniqueThreadId> for ShortThreadId {
    type Error = ThreadIdOverflowError;

    #[inline]
    fn try_from(value: UniqueThreadId) -> Result<Self, Self::Error> {
        let value = NonZeroU16::try_from(value.0).map_err(|_| ThreadIdOverflowError)?;
        if value.get() <= Self::MAX.value() {
            Ok(ShortThreadId(value))
        } else {
            Err(ThreadIdOverflowError)
        }
    }
}
