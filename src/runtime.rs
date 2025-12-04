use crate::runtime::threads::{
    InvalidSharedThreadError, LocalThreadAccessError, LocalThreadState, ShortThreadId,
};
use arbitrary_int::prelude::*;
use core::marker::PhantomPinned;
use core::sync::atomic::AtomicU32;
use std::ffi::c_void;
use std::fmt::{Debug, Formatter};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

mod threads;

/// An error returned by [`Brc::strong_count`](crate::Brc::strong_count)
/// if the reference count cannot be precisely determined.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Imprecise reference count due to biased thread (lower bound is {lower_bound})")]
pub struct ImpreciseRefCountError {
    lower_bound: usize,
}

/// The object header for a [`Brc`].
///
/// Separated from the [`Brc`] to allow more detailed control of allocation.
///
/// # Safety
/// Calling [`Self::decrement_strong`] incorrectly can lead to use-after-free.
///
/// The address of the header is assumed to be stable,
/// so it must never move in memory after it is constructed.
///
/// [`Brc`]: crate::Brc
#[repr(C)]
pub struct RawBrcHeader {
    biased_word: AtomicU32,
    shared_word: AtomicU32,
    marker: PhantomPinned,
}
impl RawBrcHeader {
    /// Lazily initialize the [`threads::THIS_THREAD_STATE`] thread-local variable,
    /// returning the [`ShortThreadId`] if any.
    ///
    /// Moving this to a separate function avoids a second TLS access in the hot-path [`Self::init`].
    /// We previously called [`LocalThreadState::with_current`] at the beginning of [`Self::init`]
    /// to ensure the thread state is fully initialized before we asked for the ID.
    /// However, this means we needed to access two thread locals:
    /// - [`threads::THIS_THREAD_STATE`] to get the ID and lazy-init the state
    /// - [`threads::THIS_THREAD_STATE_FAST`] to check if [`crate::collect`] is needed
    ///
    /// What is worse, the first TLS was lazy-initialized,
    /// so it needed an initialization check every time.
    /// Instead, we just check [`threads::THIS_THREAD_STATE_FAST`] in the hot-path,
    /// and call out to this function if the state hasn't been initialized yet.
    /// This is measurably faster (about 9%) than the old approach.
    #[cold]
    #[inline(never)]
    fn init_tid() -> Option<ShortThreadId> {
        LocalThreadState::with_current(|state| state.short_id()).ok()
    }

    /// Initialize the header, biasing towards the current thread.
    ///
    /// # Safety
    /// The resulting header must be pinned in-memory before it is ever used.
    #[inline]
    pub unsafe fn init() -> Self {
        let this_id = match LocalThreadState::existing_short_id() {
            Ok(short_id) => Some(short_id),
            Err(LocalThreadAccessError::Dead | LocalThreadAccessError::IdOverflow(_)) => {
                // in this case, the local state was already initialized,
                // but we cannot participate in biased reference counting
                None
            }
            Err(LocalThreadAccessError::Uninitialized) => {
                // Need to actually initialize the thread state
                Self::init_tid()
            }
        };
        match this_id {
            None => RawBrcHeader {
                shared_word: AtomicU32::new(
                    SharedWord {
                        shared_count: SharedCount::new(1),
                        // mark as merged
                        merged: true,
                        queued: false,
                    }
                    .to_raw(),
                ),
                biased_word: AtomicU32::new(BiasedWord::UNOWNED.to_raw()),
                marker: PhantomPinned,
            },
            Some(this_id) => RawBrcHeader {
                biased_word: AtomicU32::new(
                    BiasedWord {
                        biased_count: BiasedCount::new(1),
                        owner_id: Some(this_id),
                    }
                    .to_raw(),
                ),
                shared_word: AtomicU32::new(
                    SharedWord {
                        queued: false,
                        merged: false,
                        shared_count: SharedCount::new(0),
                    }
                    .to_raw(),
                ),
                marker: PhantomPinned,
            },
        }
    }

    /// Increment the object's strong count.
    ///
    /// # Panic
    /// Guaranteed to never unwind,
    /// although it may abort if a fatal issue is detected.
    /// In particular, a reference count overflow will trigger an abort.
    ///
    /// # Safety
    /// This is a safe operation for the same reason that [`std::mem::forget`] is.
    #[inline]
    pub fn increment_strong(&self) {
        if self.attempt_fast_increment().is_err() {
            self.slow_increment();
        }
    }

    /// Attempt to determine the number of strong references,
    /// returning an error if it cannot be precisely determined.
    #[inline]
    pub fn strong_count(&self) -> Result<usize, ImpreciseRefCountError> {
        let this_thread_id = LocalThreadState::existing_short_id().ok();
        let shared_word = SharedWord::from_raw(self.shared_word.load(Ordering::Acquire));
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        if shared_word.merged {
            debug_assert!(shared_word.shared_count.value() > 0, "{shared_word:?}");
            let res = shared_word.shared_count.value();
            debug_assert!(res >= 0, "bad merged refcount {res:?}");
            #[expect(
                clippy::cast_sign_loss,
                reason = "Reference count should be nonnegative for merged threads"
            )]
            Ok(res as usize)
        } else if biased_word.owner_id == this_thread_id {
            let (sum_count, overflow) = u32::from(biased_word.biased_count)
                .overflowing_add_signed(shared_word.shared_count.value());
            debug_assert!(
                !overflow,
                "Sum overflows for {shared_word:?} + {biased_word:?}"
            );
            Ok(sum_count as usize)
        } else {
            // We are not the owning thread, and the RCs have not been merged,
            // so we cannot know the true value of the reference counting.
            // However, since the biased count is always nonnegative,
            // we do have a lower bound.
            #[expect(clippy::cast_sign_loss, reason = "Ensured nonnegative before casting")]
            Err(ImpreciseRefCountError {
                lower_bound: shared_word.shared_count.value().max(0) as usize,
            })
        }
    }

    /// Attempt to determine if the reference count is unique.
    ///
    /// May have false negatives if not on the biased thread,
    /// but will never have false positives.
    ///
    /// # Panics
    /// If internal invariants are invalidated, this may panic.
    #[inline]
    pub fn is_unique(&self) -> bool {
        match self.strong_count() {
            Ok(count) => count == 1,
            Err(ImpreciseRefCountError { .. }) => false, // be conservative
        }
    }

    #[inline]
    fn attempt_fast_increment(&self) -> Result<(), FastIncrementFailure> {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let incremented_counter = biased_word
            .biased_count
            .checked_add(BiasedCount::new(1))
            .ok_or(FastIncrementFailure)?;
        let this_id = LocalThreadState::existing_short_id().map_err(|_| FastIncrementFailure)?;
        if biased_word.owner_id == Some(this_id) {
            self.biased_word.store(
                BiasedWord {
                    biased_count: incremented_counter,
                    ..biased_word
                }
                .to_raw(),
                Ordering::Relaxed,
            );
            Ok(())
        } else {
            Err(FastIncrementFailure)
        }
    }

    #[cold]
    fn slow_increment(&self) {
        // safe to use a relaxed CAS here, as justified in Arc::clone
        let new_word = SharedWord::from_raw(self.shared_word.fetch_add(1, Ordering::Relaxed));
        if new_word.shared_count > SharedWord::OVERFLOW_THRESHOLD {
            nounwind::abort_unwind(|| panic!("Refcount overflow"));
        }
    }

    /// Decrement the object's strong count,
    /// calling the specified destructor function on failure.
    ///
    /// # Panics
    /// This function can panic if dropping the underlying value panics.
    ///
    /// It can also panic if the internal state is corrupted in some way.
    /// However, this behavior is not guaranteed.
    /// In the future, internal errors may trigger an abort instead.
    ///
    /// # Safety
    /// Once a header is destroyed, it should never be used again.
    ///
    /// Undefined behavior if not correctly paired with [`Self::increment_strong`].
    /// The header must have been previously constructed using [`Self::init`],
    /// which allows skipping some initialization checks.
    #[inline]
    pub unsafe fn decrement_strong<D: DropInfo>(&self, drop: D) {
        // SAFETY: Caller guarantees drop function is valid and RC is owned
        match unsafe { self.fast_decrement(drop) } {
            Ok(()) => {} // nothing more to do
            Err(FastDecrementFailure) => {
                // SAFETY: Caller guarantees drop function is valid and RC is owned
                unsafe { self.slow_decrement_trampoline(drop) }
            }
        }
    }

    #[inline]
    unsafe fn fast_decrement<D: DropInfo>(&self, drop: D) -> Result<(), FastDecrementFailure> {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let owner_id = biased_word.owner_id.ok_or(FastDecrementFailure)?;
        let this_id = match LocalThreadState::existing_short_id() {
            Ok(short_id) => short_id,
            Err(
                LocalThreadAccessError::Uninitialized
                | LocalThreadAccessError::IdOverflow(_)
                | LocalThreadAccessError::Dead,
            ) => {
                // if this thread has not been initialized,
                // then we obviously cannot own the biased counter
                return Err(FastDecrementFailure);
            }
        };
        if this_id == owner_id {
            debug_assert_ne!(biased_word.biased_count.value(), 0);
            // SAFETY: Caller guarantees that refcnt > 0
            let new_biased_count = unsafe {
                BiasedCount::new_unchecked(biased_word.biased_count.value().unchecked_sub(1))
            };
            // can just update the reference count
            if new_biased_count.value() > 0 {
                // store updated reference count
                self.biased_word.store(
                    BiasedWord {
                        biased_count: new_biased_count,
                        ..biased_word
                    }
                    .to_raw(),
                    Ordering::Relaxed,
                );
                Ok(())
            } else {
                // SAFETY: We have already verified that we are the biased thread
                unsafe {
                    self.fast_decrement_slow::<D>(drop);
                }
                Ok(())
            }
        } else {
            Err(FastDecrementFailure)
        }
    }

    /// The slow-path for [`Self::fast_decrement`],
    /// still assuming this thread is the owner.
    ///
    /// We monomorphize this as it doesn't involve much code.
    ///
    /// # Safety
    /// Assumes that this thread is the owner of the object,
    /// and that it is still "biased".
    #[cold]
    #[inline(never)] // inlining this seems to harm performance
    unsafe fn fast_decrement_slow<D: DropInfo>(&self, drop: D) {
        let old_shared = SharedWord::from_raw(
            self.shared_word
                .fetch_or(SharedWord::MERGED_BIT, Ordering::AcqRel),
        );
        debug_assert!(!old_shared.merged);
        // only change is the addition of the merge bit
        let new_shared = SharedWord {
            merged: true,
            ..old_shared
        };
        debug_assert!(new_shared.shared_count.value() >= 0);
        if new_shared.shared_count.value() == 0 {
            let header_ptr = NonNull::from(self);
            // SAFETY: The pointer is valid, and it is time to deallocate
            unsafe { drop.dealloc(header_ptr) }
        } else {
            // release ownership
            self.biased_word
                .store(BiasedWord::UNOWNED.to_raw(), Ordering::Relaxed);
        }
    }

    /// Trampoline to call into [`Self::slow_decrement`].
    ///
    /// Saves a couple of instructions in the fast path,
    /// particularly when there is no pointer metadata,
    /// the layout is constant, or no destructor is needed.
    #[cold]
    #[inline(never)]
    unsafe fn slow_decrement_trampoline<D: DropInfo>(&self, drop: D) {
        // SAFETY: All invariants are responsibility of the caller
        unsafe { self.slow_decrement(drop.erase()) }
    }

    #[cold]
    #[inline(never)]
    unsafe fn slow_decrement(&self, drop: ErasedDropInfo) {
        let mut old = SharedWord::from_raw(self.shared_word.load(Ordering::Relaxed));
        let mut new: SharedWord;
        loop {
            new = SharedWord {
                shared_count: old
                    .shared_count
                    .checked_sub(SharedCount::new(1))
                    .expect("refcnt underflow"),
                ..old
            };
            if new.shared_count.value() < 0 {
                new.queued = true;
            }
            match self.shared_word.compare_exchange_weak(
                old.to_raw(),
                new.to_raw(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => old = SharedWord::from_raw(x),
            }
        }
        debug_assert!(!new.merged || new.shared_count.value() >= 0);
        if old.queued != new.queued {
            let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
            let owner_id = biased_word
                .owner_id
                .expect("due to negative refcnt, must have owner");
            // SAFETY: Queued object is
            match unsafe {
                threads::SharedThreadInfo::get_by_id(owner_id)
                    .unwrap_or_else(|| panic!("thread info for owner {owner_id:?} is undefined"))
                    .queue_object(QueuedObject {
                        header_ptr: NonNull::from(self),
                        drop: drop.clone(),
                    })
            } {
                Ok(()) => {}
                Err(InvalidSharedThreadError::DeadOrDying) => {
                    // SAFETY: Since the thread is dead, we can do the explicit merge
                    unsafe {
                        self::explicit_merge(
                            owner_id,
                            QueuedObject {
                                header_ptr: NonNull::from(self),
                                drop,
                            },
                        );
                    }
                }
            }
        } else if new.merged && new.shared_count.value() == 0 {
            // SAFETY: Valid to deallocate
            unsafe {
                drop.dealloc(NonNull::from(self));
            }
        }
    }
}
// SAFETY: We are thread safe
unsafe impl Send for RawBrcHeader {}
// SAFETY: Careful to be thread safe
unsafe impl Sync for RawBrcHeader {}
#[derive(Debug)]
struct FastIncrementFailure;
#[derive(Debug)]
struct FastDecrementFailure;
impl Debug for RawBrcHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let RawBrcHeader {
            marker: _,
            biased_word,
            shared_word,
        } = self;
        // Relaxed ordering is what Debug impl the atomics use
        f.debug_struct("RawBrcHeader")
            .field(
                "biased_word",
                &BiasedWord::from_raw(biased_word.load(Ordering::Relaxed)),
            )
            .field(
                "shared_word",
                &SharedWord::from_raw(shared_word.load(Ordering::Relaxed)),
            )
            .finish()
    }
}

/// The information needed by the runtime to drop a type.
//
/// This is roughly equivalent to a function pointer to [`core::ptr::drop_in_place`],
/// but with extra functionality to deal with fat-pointers,
/// computation of header offsets, and dynamic dispatch.
///
/// Use a monomorphized [`TypeInfo`] over an [`ErasedDestructorFunc`] wherever possible.
/// It not only avoids a virtual call, but can avoid passing some pointless parameters
/// like [`Self::header_offset`] (often a constant) or [`Self::erased_context`] (often a thin-pointer)
///
/// # Safety
/// This trait is safe to implement, but all uses are unsafe.
pub trait DropInfo: Copy {
    /// The offset to add to the header to get to the value.
    fn value_offset(&self) -> isize;
    /// The context needed to invoke [`Self::erased_dealloc`].
    fn erased_context(&self) -> ErasedDestructorContext;
    unsafe fn erased_dealloc(
        header_ptr: NonNull<RawBrcHeader>,
        ctx: ErasedDestructorContext,
        value_offset: isize,
    );
    /// Deallocate the specified header using this drop information.
    ///
    /// # Safety
    /// Same requirements as [`Self::erased_dealloc`].
    #[inline]
    unsafe fn dealloc(&self, header_ptr: NonNull<RawBrcHeader>) {
        // SAFETY: Requirements guaranteed by caller
        unsafe { Self::erased_dealloc(header_ptr, self.erased_context(), self.value_offset()) }
    }
    #[inline]
    fn erase(&self) -> ErasedDropInfo {
        ErasedDropInfo {
            value_offset: self.value_offset(),
            erased_ctx: self.erased_context(),
            erased_func: Self::erased_dealloc,
        }
    }
}
/// An erased version of [`DropInfo`].
///
/// This is similar to a `dyn DropTypeInfo` but is owned, sized,
/// and limited to a subset of the triat's functionality.
#[derive(Clone)]
pub struct ErasedDropInfo {
    value_offset: isize,
    erased_ctx: ErasedDestructorContext,
    erased_func: unsafe fn(NonNull<RawBrcHeader>, ErasedDestructorContext, isize),
}
impl ErasedDropInfo {
    #[inline]
    pub unsafe fn dealloc(&self, header_ptr: NonNull<RawBrcHeader>) {
        // SAFETY: Caller guarantees the validity of the pointer
        unsafe { (self.erased_func)(header_ptr, self.erased_ctx, self.value_offset) }
    }
}
/// The context for a [`DropInfo`], erased so that the real type is unknown.
///
/// This is a pointer to preserve provenance.
#[derive(Copy, Clone, Debug)]
#[repr(transparent)]
pub struct ErasedDestructorContext(pub *mut c_void);

type BiasedCount = u20;

#[derive(Copy, Clone, Debug)]
struct BiasedWord {
    owner_id: Option<ShortThreadId>,
    biased_count: BiasedCount,
}
impl BiasedWord {
    /// The number of bits this value takes when packed with [`SelF::to_raw`].
    ///
    /// This is not necessarily equal to `size_of::<Self>() * 8`,
    /// because that is the unpacked size,
    const BITS: usize = 32;
    const UNOWNED: BiasedWord = BiasedWord {
        owner_id: None,
        biased_count: BiasedCount::ZERO,
    };
    #[inline]
    fn to_raw(self) -> u32 {
        const {
            assert!(ShortThreadId::BITS as usize + BiasedCount::BITS == Self::BITS);
        }
        ((self.biased_count.value()) << ShortThreadId::BITS)
            | (self
                .owner_id
                .map_or(0, |value| value.value().value() as u32))
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        BiasedWord {
            owner_id: ShortThreadId::new(arbitrary_int::u12::masked_new(raw)),
            biased_count: BiasedCount::masked_new(raw >> ShortThreadId::BITS),
        }
    }
}

type SharedCount = i30;

#[derive(Copy, Clone, Debug)]
struct SharedWord {
    shared_count: SharedCount,
    /// This is set by the owner thread when it has merged the shared and biased counters.
    ///
    /// Once this is set, the reference count will never be biased again.
    merged: bool,
    /// Requests the owner thread to merge the reference counters,
    queued: bool,
}
impl SharedWord {
    /// The number of bits this value takes when packed with [`SelF::to_raw`].
    ///
    /// This is not necessarily equal to `size_of::<Self>() * 8`,
    /// because that is the unpacked size,
    const BITS: usize = 32;
    const MERGED_BIT: u32 = 1 << SharedCount::BITS;
    const QUEUED_BIT: u32 = 1 << (SharedCount::BITS + 1);
    /// The threshold past which a reference count should be considered to have overflown.
    ///
    /// This is checked against the result of calling [`fetch_add`] to check for overflow.
    /// Use of `fetch_add` is noticeably faster than calling [`checked_add`] in a CAS loop,
    /// but comes with the risk of the `fetch_add`
    /// overflowing and then the thread going to sleep before the panic can occur.
    /// If enough threads end up incrementing the counter, then go to sleep,
    /// the counter could silently wrap around and the final thread would not notice the overflow.
    /// However, for a 30-bit counter,
    /// it would take over 100 million threads to reach this threshold.
    /// This is safe enough we don't have to worry about it.
    const OVERFLOW_THRESHOLD: SharedCount = SharedCount::new(SharedCount::MAX.value() / 2);
    #[inline]
    fn to_raw(self) -> u32 {
        const {
            assert!(SharedCount::BITS + 2 == Self::BITS);
        }
        self.shared_count.to_bits()
            | ((self.merged as u32) << SharedCount::BITS)
            | ((self.queued as u32) << (SharedCount::BITS + 1))
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        SharedWord {
            shared_count: SharedCount::masked_new(raw),
            merged: (raw & Self::MERGED_BIT) != 0,
            queued: (raw & Self::QUEUED_BIT) != 0,
        }
    }
}

#[derive(Clone)]
pub(super) struct QueuedObject {
    pub header_ptr: NonNull<RawBrcHeader>,
    pub drop: ErasedDropInfo,
}
// SAFETY: This an immutable object
unsafe impl Send for QueuedObject {}
// SAFETY: This an immutable object
unsafe impl Sync for QueuedObject {}

#[cold]
pub(super) unsafe fn explicit_merge(biased_tid: ShortThreadId, object: QueuedObject) {
    // SAFETY: Validity guaranteed by caller
    let header = unsafe { object.header_ptr.as_ref() };
    // we own this so don't need a fence
    let biased = BiasedWord::from_raw(header.biased_word.load(Ordering::Relaxed));
    // now update the shared word
    assert_eq!(biased.owner_id, Some(biased_tid));
    let mut old_word = SharedWord::from_raw(header.shared_word.load(Ordering::Relaxed));
    let mut new_word: SharedWord;
    loop {
        assert!(!old_word.merged);
        let biased_count: SharedCount;
        {
            const {
                assert!(
                    BiasedCount::MAX.value() as i64 <= SharedCount::MAX.value() as i64,
                    "BiasedCount doesn't fit in a SharedCount"
                );
            }
            #[expect(clippy::cast_possible_wrap, reason = "checked above")]
            // SAFETY: We just checked above that a BiasedCount fits in a SharedCount
            unsafe {
                biased_count = SharedCount::new_unchecked(biased.biased_count.value() as i32);
            }
        }
        new_word = SharedWord {
            shared_count: old_word
                .shared_count
                .checked_add(biased_count)
                .expect("refcnt overflow when merging pointers"),
            merged: true,
            ..old_word
        };
        // since we branch on the result, I don't think that Relaxed ordering is safe
        match header.shared_word.compare_exchange_weak(
            old_word.to_raw(),
            new_word.to_raw(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual_value) => {
                old_word = SharedWord::from_raw(actual_value);
                continue;
            }
        }
    }
    assert!(new_word.shared_count.value() >= 0, "{new_word:?}");
    if new_word.shared_count.value() == 0 {
        // SAFETY: Caller promises the drop function is valid
        unsafe { object.drop.dealloc(object.header_ptr) }
    } else {
        // release ownership/unbias
        header.biased_word.store(
            BiasedWord {
                owner_id: None,
                biased_count: BiasedCount::ZERO,
            }
            .to_raw(),
            Ordering::Relaxed,
        );
    }
}

/// Perform thread-local cleanup operations if deemed necessary.
///
/// This is necessary to unbias reference counts migrating across threads.
///
/// This function is implicitly called by [`crate::Brc::new`],
/// [`crate::Brc::clone`], and [`crate::Brc::drop`],
/// so only it needs to be invoked implicitly if a thread goes a long time without calling these functions.
///
/// # Panics
/// Will never panic, but may abort if internal state is irreparably corrupted.
#[inline]
pub fn collect() {
    if LocalThreadState::currently_needs_collect() {
        LocalThreadState::collect_slow();
    }
}

/// Forcibly perform the [`cleanup`] operation, regardless of internal heuristics.
#[cold]
pub fn collect_force() {
    let _ = LocalThreadState::with_current(LocalThreadState::collect_force);
}
