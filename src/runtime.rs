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
    /// Initialize the header, biasing towards the current thread.
    ///
    /// # Safety
    /// The resulting header must be pinned in-memory before it is ever used.
    #[inline]
    pub unsafe fn init() -> Self {
        // Cannot use LocalThreadState::existing_short_id,
        // because the thread state may not exist and we want to initialize it.
        let this_id = LocalThreadState::with_current(LocalThreadState::short_id).ok();
        match this_id {
            None => RawBrcHeader {
                shared_word: AtomicU32::new(
                    SharedWord {
                        shared_count: i30::new(1),
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
                        biased_count: u14::new(1),
                        owner_id: Some(this_id),
                    }
                    .to_raw(),
                ),
                shared_word: AtomicU32::new(
                    SharedWord {
                        queued: false,
                        merged: false,
                        shared_count: i30::new(0),
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
            .checked_add(u14::new(1))
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
    #[inline(never)]
    fn slow_increment(&self) {
        nounwind::abort_unwind(|| {
            self.shared_word
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old| {
                    let old = SharedWord::from_raw(old);
                    let new_count = old
                        .shared_count
                        .checked_add(i30::new(1))
                        .expect("refcnt overflow");
                    Some(
                        SharedWord {
                            shared_count: new_count,
                            ..old
                        }
                        .to_raw(),
                    )
                })
                .unwrap();
        });
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
            let new_biased_count =
                unsafe { u14::new_unchecked(biased_word.biased_count.value().unchecked_sub(1)) };
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
    #[inline(never)]
    unsafe fn fast_decrement_slow<D: DropInfo>(&self, drop: D) {
        let new = SharedWord::from_raw(
            self.shared_word
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old| {
                    let old = SharedWord::from_raw(old);
                    debug_assert!(!old.merged);
                    Some(
                        SharedWord {
                            merged: old.merged,
                            ..old
                        }
                        .to_raw(),
                    )
                })
                .unwrap(),
        );
        debug_assert!(new.shared_count.value() >= 0);
        if new.shared_count.value() == 0 {
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
                    .checked_sub(i30::new(1))
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
                    .expect("owner thread info is undefined")
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

#[derive(Copy, Clone, Debug)]
struct BiasedWord {
    owner_id: Option<ShortThreadId>,
    biased_count: u14,
}
impl BiasedWord {
    const UNOWNED: BiasedWord = BiasedWord {
        owner_id: None,
        biased_count: u14::ZERO,
    };
    #[inline]
    fn to_raw(self) -> u32 {
        ((self.biased_count.value() as u32) << ShortThreadId::BITS)
            | (self.owner_id.map_or(0, |value| value.value().value()))
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        BiasedWord {
            owner_id: ShortThreadId::new(arbitrary_int::u18::masked_new(raw)),
            biased_count: arbitrary_int::u14::masked_new(raw >> ShortThreadId::BITS),
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct SharedWord {
    shared_count: i30,
    /// This is set by the owner thread when it has merged the shared and biased counters.
    ///
    /// Once this is set, the reference count will never be biased again.
    merged: bool,
    /// Requests the owner thread to merge the reference counters,
    queued: bool,
}
impl SharedWord {
    #[inline]
    fn to_raw(self) -> u32 {
        self.shared_count.to_bits() | ((self.merged as u32) << 30) | ((self.queued as u32) << 31)
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        SharedWord {
            shared_count: i30::masked_new(raw),
            merged: (raw & (1 << 30)) != 0,
            queued: (raw & (1 << 31)) != 0,
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
        #[expect(clippy::cast_possible_wrap, reason = "an u14 fits in an i16")]
        let biased_count = i30::from(biased.biased_count.value() as i16);
        new_word = SharedWord {
            shared_count: old_word
                .shared_count
                .checked_add(biased_count)
                .expect("refcnt overflow when merging pointers"),
            merged: true,
            ..old_word
        };
        match header.shared_word.compare_exchange_weak(
            old_word.to_raw(),
            new_word.to_raw(),
            Ordering::Relaxed,
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
                biased_count: u14::ZERO,
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
    // This always costs a TLS lookup, which can be relatively expensive
    // If queuing is rare enough it might make sense to add a global flag
    let _ = LocalThreadState::with_current(LocalThreadState::collect);
}

/// Forcibly perform the [`cleanup`] operation, regardless of internal heuristics.
#[cold]
pub fn collect_force() {
    let _ = LocalThreadState::with_current(LocalThreadState::collect_force);
}
