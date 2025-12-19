use crate::runtime::threads::{
    LocalThreadAccessError, LocalThreadState, SharedThreadInfo, ShortThreadId,
};
use arbitrary_int::prelude::*;
use cfg_if::cfg_if;
use core::ffi::c_void;
use core::fmt::{Debug, Formatter};
use core::marker::PhantomPinned;
use core::ptr::NonNull;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

mod threads;

/// An error returned by [`Brc::biased_count`],
/// either caused by being the wrong thread or not being biased at all.
///
/// This is an internal type intended only for testing,
/// just like [`Brc::biased_count`].
///
/// [`Brc::biased_count`]: crate::Brc::biased_count
#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
#[doc(hidden)]
pub enum BiasedCountError {
    #[error("Wrong thread cannot access biased count")]
    WrongThread,
    #[error("Reference is no longer biased")]
    NotBiased,
}

/// An error returned by [`Brc::strong_count`](crate::Brc::strong_count)
/// if the reference count cannot be precisely determined.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Imprecise reference count due to biased thread (lower bound is {lower_bound})")]
pub struct ImpreciseRefCountError {
    lower_bound: usize,
}

/// The result of calling [`RawBrcHeader::increment_strong_unless_zero`]
/// if the strong reference count is zero.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ZeroReferenceCountError;

/// The result of calling [`RawBrcHeader::decrement_strong`],
/// indicating whether the value needs to be dropped.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StrongDecrementResult {
    /// If the value should be dropped,
    /// as there are no more references.
    pub should_drop: bool,
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
    shared_word: AtomicUsize,
    biased_word: AtomicU32,
    marker: PhantomPinned,
}
impl RawBrcHeader {
    /// We promise that a [`RawBrcHeader`] doesn't need to execute a drop function.
    ///
    /// This is true regardless of what [`std::mem::needs_drop`] claims.
    pub const NEEDS_DROP: bool = false;

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
                LocalThreadState::init_tid()
            }
        };
        match this_id {
            None => RawBrcHeader {
                shared_word: AtomicUsize::new(
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
                shared_word: AtomicUsize::new(
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

    /// Attempt to determine the number of strong references,
    /// returning an error if it cannot be precisely determined.
    #[inline]
    pub fn strong_count(&self) -> Result<usize, ImpreciseRefCountError> {
        let this_thread_id = LocalThreadState::existing_short_id().ok();
        let shared_word = SharedWord::from_raw(self.shared_word.load(Ordering::Acquire));
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        if shared_word.merged {
            let res = shared_word.shared_count.value();
            if res < 0 {
                // SAFETY: This can only happen if there are more decrements than clones
                // This is undefined behavior on the part of the user
                unsafe {
                    if cfg!(debug_assertions) {
                        undefined_behavior::negative_refcnt_merge();
                    } else {
                        core::hint::unreachable_unchecked()
                    }
                }
            }
            #[expect(
                clippy::cast_sign_loss,
                reason = "Reference count should be nonnegative for merged threads"
            )]
            Ok(res as usize)
        } else if biased_word.owner_id == this_thread_id {
            const {
                assert!(
                    BiasedCount::BITS + 1 < usize::BITS as usize,
                    "biased count should be at least one bit smaller than usize"
                );
                assert!(
                    SharedCount::BITS + 1 < usize::BITS as usize,
                    "biased count should be at least one bit smaller than usize"
                );
            }
            let biased_count = biased_word.biased_count.value() as usize;
            let shared_count = shared_word.shared_count.value();
            // Since biased_count is 20-bits and shared_count is i30/i60,
            // the result should never overflow a usize.
            // We just statically verified this above.
            //
            // The merged count cannot underflow, for reasons described above
            let sum_count: usize = {
                if cfg!(debug_assertions) {
                    biased_count
                        .checked_add_signed(shared_count)
                        .unwrap_or_else(|| undefined_behavior::strong_count_arith_overflow())
                } else {
                    biased_count.wrapping_add_signed(shared_count)
                }
            };
            Ok(sum_count)
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

    /// Return `true` quickly if we are definitely not a unique reference.
    #[inline]
    pub fn is_definitely_not_unique(&self) -> bool {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let this_thread_id = LocalThreadState::existing_short_id().ok();
        biased_word.owner_id.is_some() && biased_word.owner_id != this_thread_id
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
    pub fn biased_count(&self) -> Result<usize, BiasedCountError> {
        let this_thread_id = LocalThreadState::existing_short_id().ok();
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        if biased_word.owner_id.is_none() {
            Err(BiasedCountError::NotBiased)
        } else if this_thread_id == biased_word.owner_id {
            Ok(biased_word.biased_count.value() as usize)
        } else {
            Err(BiasedCountError::WrongThread)
        }
    }

    #[inline]
    pub fn shared_count(&self) -> isize {
        SharedWord::from_raw(self.shared_word.load(Ordering::Acquire))
            .shared_count
            .value()
    }

    #[inline]
    fn attempt_biased_increment(&self) -> Result<(), FastIncrementFailure> {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let incremented_counter = biased_word
            .biased_count
            .checked_add(BiasedCount::new(1))
            .ok_or(FastIncrementFailure)?;
        let this_id = LocalThreadState::existing_short_id().map_err(|_| FastIncrementFailure)?;
        if biased_word.owner_id == Some(this_id) {
            // The biased count cannot be zero, unless the count has been merged.
            // If the count has been merged, we should never be marked as owned.
            if cfg!(debug_assertions) && biased_word.biased_count.value() == 0 {
                undefined_behavior::biased_count_zero_and_owned();
            }
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

    /// Increment the object's strong count.
    ///
    /// # Panic
    /// Guaranteed to never unwind,
    /// although it may abort if a fatal issue is detected.
    /// In particular, a reference count overflow will trigger an abort.
    ///
    /// # Safety
    /// This is a safe operation for the same reason that [`core::mem::forget`] is.
    #[inline]
    pub fn increment_strong(&self) {
        if self.attempt_biased_increment().is_err() {
            self.increment_strong_shared();
        }
    }

    /// Increment the object's shared strong count,
    /// not affecting the biased count even if this is the biased thread.
    ///
    /// This is exposed mainly for testing purposes.
    ///
    /// # Performance
    /// This function is extremely small (~40 bytes on aarch64).
    ///
    /// As such, we mark it `#[inline]` even though it is on the cold path.
    /// This way it is eligible for inlining even without using LTO.
    ///
    /// # Safety
    /// This is a safe operation for the same reason that [`core::mem::forget`] is.
    ///
    /// It is always safe to increment the shared count, regard
    #[cold]
    #[inline]
    pub fn increment_strong_shared(&self) {
        // safe to use a relaxed CAS here, as justified in Arc::clone
        let new_word = SharedWord::from_raw(self.shared_word.fetch_add(1, Ordering::Relaxed));
        if new_word.shared_count > SharedWord::OVERFLOW_THRESHOLD {
            fatal_errors::shared_refcnt_overflow();
        }
    }

    /// Attempt to increment the object's strong count,
    /// unless the object's count is already at zero.
    #[inline]
    pub fn increment_strong_unless_zero(&self) -> Result<(), ZeroReferenceCountError> {
        // If attempt_biased_increment succeeds,
        // we know that the overall shared count is nonzero
        //
        // This relies upon the fact that merged counts clear the owner field
        if self.attempt_biased_increment().is_ok() {
            Ok(())
        } else {
            match self
                .shared_word
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old_shared| {
                    let old_shared = SharedWord::from_raw(old_shared);
                    let is_dead = old_shared.merged && old_shared.shared_count.value() == 0;
                    if cfg!(debug_assertions)
                        && old_shared.merged
                        && old_shared.shared_count.value() < 0
                    {
                        undefined_behavior::negative_refcnt_merge();
                    }
                    if is_dead {
                        None
                    } else {
                        Some(
                            SharedWord {
                                shared_count: old_shared
                                    .shared_count
                                    .checked_add(SharedCount::new(1))
                                    .unwrap_or_else(|| fatal_errors::shared_refcnt_overflow()),
                                ..old_shared
                            }
                            .to_raw(),
                        )
                    }
                }) {
                Ok(_) => Ok(()),
                Err(_) => Err(ZeroReferenceCountError),
            }
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
    ///
    /// The pointer to the object must be valid, as if passing `&self`.
    /// We use raw pointers to comply with the requirements of Tree Borrows.
    #[inline]
    pub unsafe fn decrement_strong<D: DropInfo>(
        this: *const Self,
        drop: D,
    ) -> StrongDecrementResult {
        // SAFETY: The RC is owned
        match unsafe { (*this).decrement_biased() } {
            Ok(res) => {
                // successfully executed biased decrement,
                // return info on whether a drop is needed
                res
            }
            Err(FastDecrementFailure) => {
                // SAFETY: Caller guarantees drop function is valid and RC is owned
                unsafe { Self::decrement_shared(this, drop) }
            }
        }
    }

    #[inline]
    unsafe fn decrement_biased(&self) -> Result<StrongDecrementResult, FastDecrementFailure> {
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
                Ok(StrongDecrementResult { should_drop: false })
            } else {
                // SAFETY: We have already verified that we are the biased thread
                unsafe { Ok(self.decrement_biased_slow()) }
            }
        } else {
            Err(FastDecrementFailure)
        }
    }

    /// The slow-path for [`Self::decrement_biased`],
    /// still assuming this thread is the owner.
    ///
    /// We monomorphize this as it doesn't involve much code.
    ///
    /// # Safety
    /// Assumes that this thread is the owner of the object,
    /// and that it is still "biased".
    #[cold]
    #[inline(never)] // inlining this seems to harm performance
    unsafe fn decrement_biased_slow(&self) -> StrongDecrementResult {
        let old_shared = SharedWord::from_raw(
            self.shared_word
                .fetch_or(SharedWord::MERGED_BIT, Ordering::AcqRel),
        );
        debug_assert!(!old_shared.merged);
        // the only change is the addition of the merge bit
        let new_shared = SharedWord {
            merged: true,
            ..old_shared
        };
        debug_assert!(new_shared.shared_count.value() >= 0);
        // release ownership
        //
        // Once we merge reference counts,
        // we need to release ownership so that we are never incremented again.
        //
        // This needs to be done even if we `should_drop`,
        // as features like weak references could observe the strong count
        // even after the primary value is dropped.
        self.biased_word
            .store(BiasedWord::UNOWNED.to_raw(), Ordering::Relaxed);
        StrongDecrementResult {
            should_drop: new_shared.shared_count.value() == 0,
        }
    }

    #[cold]
    #[inline(never)]
    unsafe fn decrement_shared<D: DropInfo>(this: *const Self, drop: D) -> StrongDecrementResult {
        // SAFETY: Caller guarantees pointer is valid
        let shared_word = unsafe { &(*this).shared_word };
        let mut old = SharedWord::from_raw(shared_word.load(Ordering::Relaxed));
        let mut new: SharedWord;
        // WARNING: It would be invalid to use `fetch_sub` on the shared counter,
        // as subtraction would clobber the flags in the high-bit when the counter becomes negative.
        //
        // # Potential Optimization 1:
        // The comments in `std::sync::Arc::drop` [1] claim that the counter subtraction here
        // can use a `Release` ordering in the fast-path,
        // as long as an `Acquire` fence is done in the cold-path before deallocation.
        // I still use `AcqRel` to be conservative
        // and because we care who wins the race to set the 'queued' flag.
        //
        // # Potential Optimization 2:
        // It might be possible to split this into two CAS operations:
        // One doing the subtraction in the fast-path
        // and one another setting the queue bit in the cold path.
        // The cold-path would only trigger if `new.shared_count <= 0`.
        // I am unsure if this is profitable as it would require an additional CAS in the cold-path,
        // and this function already does very little work besides the CAS.
        // On my M1 macbook, it is just as fast as `Arc::drop` in the non-biased case
        // and only takes up 152 bytes of machine code.
        //
        // The idea of using two CAS has issues with each operation seeing different states.
        // Which counter should we consider when testing `new.merged && new.shared_count == 0`?
        // The biased reference counting paper [2] has only a single CAS operation,
        // so that is probably what we should stick with for now.
        //
        // [1]: https://github.com/rust-lang/rust/blob/1.91.0/library/alloc/src/sync.rs#L2639-L2674
        // [2]: https://dl.acm.org/doi/10.1145/3243176.3243195
        loop {
            new = SharedWord {
                shared_count: old
                    .shared_count
                    .checked_sub(SharedCount::new(1))
                    .unwrap_or_else(|| fatal_errors::shared_refcnt_underflow()),
                ..old
            };
            if new.shared_count.value() < 0 {
                new.queued = true;
            }
            match shared_word.compare_exchange_weak(
                old.to_raw(),
                new.to_raw(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => old = SharedWord::from_raw(x),
            }
        }
        let should_drop: bool;
        debug_assert!(!new.merged || new.shared_count.value() >= 0);
        if old.queued != new.queued {
            // SAFETY: We now have the exclusive right to queue the object
            unsafe { Self::decrement_shared_do_queue(this, drop) }
            should_drop = false; // drop handled by queue
        } else {
            should_drop = new.merged && new.shared_count.value() == 0;
        }
        StrongDecrementResult { should_drop }
    }

    /// The slow path for [`Self::decrement_shared`],
    /// where an object needs to be added to the queue.
    ///
    /// # Size & Speed
    /// Although this is the slowest of the slow paths, the function is monomorphized to avoid
    /// the overhead of calling [`DropInfo::erase`] in the caller.
    /// In practice, this function appears fairly small.
    /// It is around 156 bytes on aarch64, which is similar to `decrement_shared` (148 bytes).
    /// The function [`SharedThreadInfo::queue_object`] is over 300 bytes,
    /// but is marked `#[inline(never)]` so does not bloat this function.
    ///
    /// # Safety
    /// Should only be called by [`Self::decrement_shared`]
    ///
    /// Undefined behavior if called more than once.
    /// Must have set the queued flag before running this function.
    #[cold]
    #[inline(never)]
    unsafe fn decrement_shared_do_queue<D: DropInfo>(this: *const Self, drop: D) {
        // SAFETY: Caller guarantees pointer is valid
        let biased_word =
            BiasedWord::from_raw(unsafe { &(*this).biased_word }.load(Ordering::Relaxed));
        let owner_id = biased_word
            .owner_id
            .unwrap_or_else(|| undefined_behavior::negative_refcnt_no_owner());
        let drop = drop.erase();
        // SAFETY: The refcnt guarantees the header will not be dropped,
        // and the caller guarantees the drop information is valid
        //
        // We also know that this will be called at most once due to the successful CAS
        unsafe {
            SharedThreadInfo::get_by_id(owner_id)
                .unwrap_or_else(|| undefined_behavior::owner_undefined_state())
                .queue_object(QueuedObject {
                    header_ptr: NonNull::new_unchecked(this.cast_mut()),
                    drop: drop.clone(),
                });
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
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
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
/// Use a monomorphized [`DropInfo`] over an [`ErasedDropInfo`] wherever possible.
/// It not only avoids a virtual call, but can avoid passing some pointless parameters
/// like [`Self::value_offset`] (often a constant) or [`Self::erased_context`] (often a thin-pointer)
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
    /// The number of bits this value takes when packed with [`Self::to_raw`].
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

cfg_if! {
    if #[cfg(target_pointer_width = "32")] {
        type SharedCountInner = i30;
    } else if #[cfg(target_pointer_width = "64")] {
        type SharedCountInner = i62;
    } else {
        // refusing to support 16-bit targets is mainly for simplicity,
        // but also because then SharedCount::BITS < BiasedCount::BITS
        compile_error!("unsupported pointer width");
    }
}
/// Wrapper around [`SharedCountInner`] so that it uses `usize` instead of u32/u64
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SharedCount(SharedCountInner);
impl SharedCount {
    pub const BITS: usize = {
        assert!(SharedCountInner::BITS <= usize::BITS as usize);
        SharedCountInner::BITS
    };
    pub const MAX: Self = Self(SharedCountInner::MAX);
    /// Create a new [`SharedCount`] without checking it fits.
    ///
    /// # Safety
    /// Same requirements as [`SharedCountInner::new_unchecked`]
    #[inline]
    pub unsafe fn new_unchecked(val: isize) -> Self {
        // SAFETY: Guaranteed by caller
        unsafe { Self(SharedCountInner::new_unchecked(val as _)) }
    }
    /// Create a new [`SharedCount`], panicking if it doesn't fit.
    #[inline]
    pub const fn new(val: isize) -> Self {
        Self(SharedCountInner::new(val as _))
    }
    #[inline]
    #[expect(clippy::cast_possible_truncation, reason = "known to fit in isize")]
    pub const fn value(self) -> isize {
        self.0.value() as isize
    }
    #[inline]
    #[expect(clippy::cast_possible_truncation, reason = "known to fit in usize")]
    pub const fn to_bits(self) -> usize {
        self.0.to_bits() as usize
    }
    #[inline]
    pub fn masked_new(value: usize) -> Self {
        Self(SharedCountInner::masked_new(value as u64))
    }
    #[inline]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
    #[inline]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }
}

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
    /// The number of bits this value takes when packed with [`Self::to_raw`].
    ///
    /// This is not necessarily equal to `size_of::<Self>() * 8`,
    /// because that is the unpacked size,
    const BITS: usize = SharedCount::BITS + 2;
    const MERGED_BIT: usize = 1 << SharedCount::BITS;
    const QUEUED_BIT: usize = 1 << (SharedCount::BITS + 1);
    /// The threshold past which a reference count should be considered to have overflown.
    ///
    /// This is checked against the result of calling [`AtomicUsize::fetch_add`] to check for overflow.
    /// Use of `fetch_add` is noticeably faster than calling [`usize::checked_add`] in a CAS loop,
    /// but comes with the risk of the `fetch_add`
    /// overflowing and then the thread going to sleep before the panic can occur.
    /// If enough threads end up incrementing the counter, then go to sleep,
    /// the counter could silently wrap around and the final thread would not notice the overflow.
    /// Even for a 30-bit counter,
    /// it would take over 100 million threads to reach this threshold.
    /// This is safe enough we don't have to worry about it.
    const OVERFLOW_THRESHOLD: SharedCount = SharedCount::new(SharedCount::MAX.value() / 2);
    #[inline]
    fn to_raw(self) -> usize {
        const {
            assert!(usize::BITS as usize == Self::BITS);
        }
        self.shared_count.to_bits()
            | ((self.merged as usize) << SharedCount::BITS)
            | ((self.queued as usize) << (SharedCount::BITS + 1))
    }
    #[inline]
    fn from_raw(raw: usize) -> Self {
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
    if biased.owner_id != Some(biased_tid) {
        undefined_behavior::explicit_merge_bad_id();
    }
    // now update the shared word
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
                biased_count = SharedCount::new_unchecked(biased.biased_count.value() as isize);
            }
        }
        new_word = SharedWord {
            shared_count: old_word
                .shared_count
                .checked_add(biased_count)
                .unwrap_or_else(|| fatal_errors::merged_refcnt_overflow()),
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
    // must always release ownership/unbias
    // as once the counter is merged, it can never be biased again
    //
    // This is needs to be done even if we end up dropping the value,
    // as weak references can observe the strong count after we are dropped.
    header
        .biased_word
        .store(BiasedWord::UNOWNED.to_raw(), Ordering::Relaxed);
    match new_word.shared_count.value().cmp(&0) {
        core::cmp::Ordering::Less => {
            // This can only happen if there are more drops then clones, which is UB
            // check for it anyway since we are in the cold-path
            undefined_behavior::negative_refcnt_merge()
        }
        core::cmp::Ordering::Equal => {
            // SAFETY: Caller promises the drop function is valid
            unsafe { object.drop.dealloc(object.header_ptr) }
        }
        core::cmp::Ordering::Greater => {
            // still have strong references to the value, so nothing more is needed
        }
    }
}

/// Perform thread-local cleanup operations if deemed necessary,
/// potentially executing deferred destructors.
///
/// This is necessary to unbias reference counts migrating across threads.
/// Unlike traditional garbage collection, this should be needed rarely and run quickly.
///
/// This function is implicitly called by [`crate::Brc::new`],
/// [`crate::Brc::clone`], and [`crate::Brc::drop`],
/// so only it needs to be invoked implicitly if a thread goes a long time without calling these functions.
///
/// This function currently does nothing if [`std::thread::panicking`] returns true,
/// but this behavior is not guaranteed and may change in the future.
///
/// # Panics
/// Will panic only if one of the deferred destructors panics.
///
/// May abort if internal state is irreparably corrupted.
#[inline]
pub fn collect() {
    if LocalThreadState::currently_needs_collect() {
        LocalThreadState::collect_slow();
    }
}

/// Forcibly perform the [`collect`] operation, regardless of internal heuristics.
///
/// # Panics
/// Panics in the same cases that [`collect`] does.
#[cold]
pub fn collect_force() {
    let _ = LocalThreadState::with_current(LocalThreadState::collect_force);
}

/// Internal errors that should trigger aborts.
///
/// These can technically be triggered by safe code,
/// but only in highly degenerate cases.
/// The standard example is leaking a billion references,
/// which causes [`std::sync::Arc::clone`] to abort as well.
pub(crate) mod fatal_errors {
    macro_rules! fatal_error {
        ($name:ident => $fmt:expr $(, $($arg:tt)*)?) => {
            #[cold]
            #[inline(never)]
            pub(crate) fn $name() -> ! {
                nounwind::panic_nounwind!(concat!("biasedrc: ", $fmt) $(, $($arg)*)*)
            }
        };
    }

    fatal_error!(shared_refcnt_underflow => "Reference count underflow for shared counter");
    fatal_error!(shared_refcnt_overflow => "Reference count overflow for a shared counter");
    fatal_error!(merged_refcnt_overflow => "Merged reference counts overflow the shared counter");
    fatal_error!(weak_refcnt_overflow => "Weak reference counts overflowed its counter");
}

/// Encountered a situation that is undefined behavior.
///
/// Not all of these situations cause undefined behavior immediately.
/// Sometimes they indicate that internal invariants have been seriously corrupted
/// and there is no way to sensibly continue.
///
/// This does not ever call [`core::hint::unreachable_unchecked`],
/// but instead aborts with a descriptive error message.
#[cfg_attr(not(debug_assertions), allow(unused))]
mod undefined_behavior {
    macro_rules! undefined_behavior {
        ($name:ident => $fmt:expr $(, $($arg:tt)*)?) => {
            #[cold]
            #[inline(never)]
            pub(crate) fn $name() -> ! {
                nounwind::panic_nounwind!(concat!("biasedrc encountered undefined behavior: ", $fmt) $(, $($arg)*)*)
            }
        };
    }

    undefined_behavior!(negative_refcnt_merge => "Negative reference count after merging counter");
    undefined_behavior!(explicit_merge_bad_id => "The `explicit_merge` function is called with bad tid");
    undefined_behavior!(negative_refcnt_no_owner => "Negative reference count but no biased thread");
    undefined_behavior!(owner_undefined_state => "Biased thread has undefined state, but still owns objects");
    undefined_behavior!(strong_count_arith_overflow => "Computing the strong_count either overflowed (impossible) or underflowed (UB) a counter");
    undefined_behavior!(biased_count_zero_and_owned => "Reference is biased towards a particular thread, but has a zero refcount");
}
