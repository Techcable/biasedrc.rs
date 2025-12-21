#[allow(clippy::disallowed_types, unused_imports, reason = "used for docs")]
use alloc::sync::Arc;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use stable_deref_trait::CloneStableDeref;

use crate::allocator_api::alloc::{Allocator, Global};
use crate::layout::{BrcHeader, LayoutInfo, WEAK_LOCKED_COUNT, WEAK_OVERFLOW_THRESHOLD};
use crate::pointee::SupportedMetadata;
use crate::ptr_meta::{self, Pointee};
use crate::runtime::{DropInfo, ErasedDestructorContext, RawBrcHeader};
use crate::{
    BiasedCountError, ImpreciseRefCountError, SupportedPointee, SupportedWeakPointee, Weak,
    collect, runtime,
};

mod conversions;

/// A thread-safe reference counted object,
/// biased towards a particular thread.
///
/// # Differences from [`Arc`]
/// Most differences come from the fact that [`Self::strong_count`] is not exact.
/// This means that [`Brc::get_mut`] and [`Brc::try_unwrap`] can fail spuriously
/// even if the [`Arc`] is logically unique.
/// It also means that [`Brc::into_inner`] cannot provide the same guarantees as [`Arc::into_inner`].
///
/// ## Allocator
/// The [`Brc`] is parameterized by an [`Allocator`] just like [`Arc`].
/// However, the allocator is stored in the object header and so is shared across
/// all references to a particular object.
/// Although this is mainly due to design limitations,
/// it obliviates the need for an `A: Clone` bound in some places that [`Arc`] requires that.
/// However, it does add an `A: Clone` bound to [`Brc::into_raw_with_allocator`].
///
/// ## Deferred Destruction & Deferred Panics
/// Destruction is not guaranteed to occur immediately when the last reference is [dropped](`drop`).
/// It may be deferred until [`collect`] is called on another thread.
///
/// Since [`collect`] is called implicitly by [`Brc::clone`], [`Brc::clone`] and [`Brc::new`],
/// any of these functions could panic while executing the deferred destructor of an unrelated object.
/// If this is unacceptable,
/// either use the [`Brc::drop_no_collect`] and [`Brc::clone_no_collect`] functions
/// or the [`nounwind`] crate.
#[repr(transparent)] // can be transmuted into a pointer
pub struct Brc<T: ?Sized + SupportedPointee, A: Allocator = Global> {
    ptr: NonNull<T>,
    value_marker: PhantomData<T>,
    alloc_marker: PhantomData<A>,
}
impl<T> Brc<T> {
    /// Construct a new [`Brc`] with the specified value.
    ///
    /// # Panics
    /// This may panic if [`collect`] does.
    ///
    /// The behavior on out of memory is determined by [`Box::new`],
    /// which may either panic or abort.
    #[inline]
    pub fn new(value: T) -> Brc<T> {
        Brc::new_in(value, Global)
    }

    /// Construct a new [`Brc`], using a closure to initialize the specified value.
    ///
    /// This can potentially improve performance by allowing values to be constructed in place.
    ///
    /// # Panics
    /// May panic in the same cases that [`Self::new`] does.
    #[inline]
    pub fn new_with(func: impl FnOnce() -> T) -> Self {
        // SAFETY: Either we fully initialize the newly allocated memory,
        // or the initialization function panics
        unsafe {
            Self::alloc_with_in::<MayPanic>(
                Layout::new::<T>(),
                (),
                |target| target.write(func()),
                Global,
            )
        }
    }

    /// Constructs a `Pin<Brc<T>>`.
    #[inline]
    pub fn pin(value: T) -> Pin<Self> {
        Brc::pin_in(value, Global)
    }
}
impl<T, A: Allocator> Brc<T, A> {
    /// Construct a new [`Brc`] with the specified value,
    /// using a particular allocator.
    #[inline]
    pub fn new_in(value: T, alloc: A) -> Self {
        // There is no advantage to waiting for BrcRawHeader::init to initialize the thread state.
        // This is because if the thread state is uninitialized,
        // the queue is empty and collection would be a no-op anyway.
        #[cfg(not(biasedrc_no_implicit_collect))]
        collect();
        // This function used to be implemented as Self::new_with(|| value).
        // While correct, this caused code bloat and was noticeably slower than Arc::new.
        //
        // The main problem is that the compiler was unable to eliminate the drop guard,
        // as it doesn't seem to realize the closure can't panic.
        // Even worse, the drop guard wasn't inlined so the compiler thought the cleanup code
        // could itself panic.
        // This required generating a second landing pad calling core::panicking::panic_in_cleanup()
        // This generated a ton of code bloat, which had to be inlined into the caller.s
        //
        // Avoiding the panic guard improves Brc::new performance by 24% on my M1 macbook.
        //
        // When I first implemented this, I reimplemented the allocation code here.
        // However, it works just as well to pass a `NeverPanic` flag to `new_with_in`.
        // This means that we don't have to reimplement the allocation code and other constructors
        // like `From<Box<T>>` can take advantage of the panic .
        //
        // I previously tried to implement this in terms of Box::new
        // on the suspicion that Box::new was more efficient than manual allocation.
        // It turns out that after inlining, they are the same thing.
        // This is a good thing as it means we can use manual `LayoutInfo` calculations
        // in both cases without needing to define a `BrcInner` type.
        //
        // SAFETY: Closure fully initializes the memory and never panics.
        // Layout information and metadata `()` are correct for T.
        unsafe {
            Self::alloc_with_in::<NeverPanic>(
                Layout::new::<T>(),
                (),
                |dest| dest.write(value),
                alloc,
            )
        }
    }

    /// Construct a new [`Brc`], using a closure to initialize the specified value,
    /// along with using a particular allocator.
    ///
    /// This can potentially improve performance by allowing values to be constructed in place.
    #[inline]
    pub fn new_with_in(func: impl FnOnce() -> T, alloc: A) -> Self {
        // SAFETY: Either we fully initialize the newly allocated memory,
        // or the initialization function panics
        unsafe {
            Self::alloc_with_in::<MayPanic>(
                Layout::new::<T>(),
                (),
                |target| target.write(func()),
                alloc,
            )
        }
    }

    /// If the [`Brc`] is uniquely owned,
    /// return the inner value.
    ///
    /// Like [`Self::get_mut`], this may fail spuriously
    /// (as [`Self::is_unique`] can have false positive).
    ///
    /// This suffers from the same race condition described in [`Arc::try_unwrap`].
    /// In particular, this means that if all threads call [`Arc::try_unwrap`],
    /// it is possible that the value is dropped and no thread gets the value.
    /// Unfortunately, there is no solution
    ///
    /// # Errors
    /// Returns an `Err` holding the original value,
    /// if the value is not known to be unique.
    #[inline]
    pub fn try_unwrap(this: Self) -> Result<T, Self>
    where
        T: Sized,
    {
        let this = ManuallyDrop::new(this);
        if Self::is_unique(&*this) {
            // SAFETY: We are unique, so can move out of the value
            Ok(unsafe { core::ptr::from_ref::<T>(&**this).read() })
        } else {
            Err(ManuallyDrop::into_inner(this))
        }
    }

    /// If the [`Brc`] is uniquely owned,
    /// return the inner value.
    ///
    /// Like [`Self::get_mut`], this may fail spuriously
    /// (as [`Self::is_unique`] can have false positive).
    ///
    /// Unlike [`Arc::into_inner`],
    /// this does not currently avoid the race condition present in [`Self::try_unwrap`].
    /// This means that if all threads call [`Arc::try_unwrap`],
    /// it is possible that the value is dropped and no thread gets the value.
    ///
    /// This is because [`Self::strong_count`] is not always exact,
    /// and dropping from the non-biased thread sometimes requires placing
    /// objects in the internal queue.
    /// In particular, if many refcounts are incremented on the biased threads,
    /// but then have [`Self::into_inner`] called on a non-biased thread,
    /// then the non-biased thread will not know when the value actually becomes unique,
    /// and will have to wait until the object is placed in the queue and later processed.
    #[allow(
        clippy::wrong_self_convention,
        reason = "don't want to conflict with inherent methods"
    )]
    #[inline]
    pub fn into_inner(this: Self) -> Option<T> {
        Self::try_unwrap(this).ok()
    }

    /// Constructs a `Pin<Brc<T>>` using the specified allocator.
    #[inline]
    pub fn pin_in(data: T, alloc: A) -> Pin<Self> {
        let res = Brc::new_in(data, alloc);
        // SAFETY: We have appropriate Deref & Drop impls
        unsafe { Pin::new_unchecked(res) }
    }
}
impl<T, A: Allocator> Brc<[T], A> {
    /// Allocate a slice of memory rom a [`Layout`] and an iterator,
    /// whose length is trusted to be exact.
    ///
    /// The iterator is permitted to panic, both in [`Iterator::next`] and [`Drop`].
    /// Passing an incorrect [`PanicPolicy`] is perfectly safe,
    /// for the same reasons as it is in [`Self::alloc_with_in`].
    ///
    /// # Safety
    /// The layout must match the result of [`Layout::array`].
    /// The calculation is moved to the caller to potentially avoid a panic.
    ///
    /// The iterator must either panic or yield precisely as many elements as its length.
    #[deny(clippy::multiple_unsafe_ops_per_block)]
    #[track_caller] // want to propagate iterator panics
    #[inline]
    pub(crate) unsafe fn from_iter_exact_trusted_in<P: PanicPolicy>(
        layout: Layout,
        mut iter: impl ExactSizeIterator<Item = T>,
        alloc: A,
    ) -> Self {
        let len = iter.len();
        debug_assert_eq!(Ok(layout), Layout::array::<T>(len));
        let do_init = |dest| {
            let dest = dest as *mut T;
            struct PartialDropGuard<T> {
                dest: *mut T,
                initialized_len: usize,
            }
            impl<T> Drop for PartialDropGuard<T> {
                fn drop(&mut self) {
                    let initialized =
                        core::ptr::slice_from_raw_parts_mut(self.dest, self.initialized_len);
                    // SAFETY: Trust that `len` items have been initialized
                    unsafe {
                        core::ptr::drop_in_place(initialized);
                    }
                }
            }
            let mut guard = if P::MAY_PANIC && core::mem::needs_drop::<T>() {
                Some(PartialDropGuard {
                    dest,
                    initialized_len: 0,
                })
            } else {
                None
            };
            for index in 0..len {
                if let Some(ref mut guard) = guard {
                    guard.initialized_len = index;
                }
                // SAFETY: We trust the length to be exact, so unwrap_unchecked is sound
                // The next() call is always allowed to panic (PanicPolicy is allowed to lie)
                let item = unsafe { iter.next().unwrap_unchecked() };
                // SAFETY: Index is in bounds
                let slot = unsafe { dest.add(index) };
                // SAFETY: Newly allocated memory is known to be valid
                unsafe { slot.write(item) };
            }
            // call next() function one more time, to trigger panic in AssertExactIter
            // We don't want to do this in the drop function as that could trigger a double-panic
            // This is zero-cost if the iterator has no side effects
            let _ = iter.next();
            drop(iter); // this is always permitted to panic, as PanicPolicy is allowed to lie
            core::mem::forget(guard); // finished initialization
        };
        // SAFETY: Either fully initializes the memory or panics
        // We trust the iterator to be exact.
        unsafe { Brc::alloc_with_in::<P>(layout, len, do_init, alloc) }
    }
}

/// Indicates whether the closure in [`Brc::alloc_with_in`] can panic.
///
/// Using [`NeverPanic`] can noticeably improve performance,
/// as discussed in [`Brc::new_in`].
///
/// # Safety
/// The [`Self::MAY_PANIC`] flag is intended as a hint,
/// and should not be relied upon for memory safety.
pub(crate) trait PanicPolicy {
    const MAY_PANIC: bool;
}
pub(crate) struct MayPanic;
impl PanicPolicy for MayPanic {
    const MAY_PANIC: bool = true;
}
pub(crate) struct NeverPanic;
impl PanicPolicy for NeverPanic {
    const MAY_PANIC: bool = false;
}

impl<T: ?Sized + SupportedPointee, A: Allocator> Brc<T, A> {
    /// Return a reference to the underlying allocator.
    ///
    /// Mirrors [`Arc::allocator`].
    #[inline]
    pub fn allocator(this: &Self) -> &A {
        &this.header().alloc
    }

    /// Initialize the value using the specified callback.
    ///
    /// # Safety
    /// Callback must either fully initialize the memory or panic.
    ///
    /// It is perfectly safe for [`PanicPolicy::MAY_PANIC`] to be wrong.
    /// In the case of a false positive, we simply include unnecessary cleanup code.
    /// In the case of a false negative, we simply leak memory.
    #[inline(always)] // Inlining means we can potentially eliminate the guard & layout calculation
    unsafe fn alloc_with_in<P: PanicPolicy>(
        layout: Layout,
        meta: T::Metadata,
        func: impl FnOnce(*mut T),
        alloc: A,
    ) -> Self {
        #[cfg(not(biasedrc_no_implicit_collect))]
        collect();
        let layout = LayoutInfo::<A>::new_or_panic(layout);
        struct CleanupGuard<'a, A: Allocator> {
            ptr: NonNull<u8>,
            layout: Layout,
            alloc: &'a A,
        }
        impl<A: Allocator> Drop for CleanupGuard<'_, A> {
            #[inline(always)] // Unfortunately, core::ptr::drop_in_place still might not be inlined
            fn drop(&mut self) {
                // SAFETY: We know the pointer is valid since we just allocated it
                // We are careful to forget the guard if we are successful
                unsafe { self.alloc.deallocate(self.ptr, self.layout) }
            }
        }
        let Ok(allocated) = alloc.allocate(layout.full_layout) else {
            alloc::alloc::handle_alloc_error(layout.full_layout);
        };
        // SAFETY: It is perfectly safe for P::MAY_PANIC to be wrong.
        // In that case, we simply leak memory.
        let guard = if P::MAY_PANIC {
            Some(CleanupGuard {
                ptr: allocated.cast(),
                layout: layout.full_layout,
                alloc: &alloc,
            })
        } else {
            None
        };
        const {
            assert!(core::mem::offset_of!(BrcHeader<A>, strong) == 0);
        }
        // SAFETY: Memory is newly allocated so it is known to be valid
        // The RawBrcHeader is pinned immediately after it is created
        // we just verified above that the field offset is zero
        unsafe {
            allocated.cast::<RawBrcHeader>().write(RawBrcHeader::init());
        }
        // SAFETY: Newly allocated memory is valid
        unsafe {
            allocated
                .byte_add(core::mem::offset_of!(BrcHeader<A>, weak_count))
                .cast::<AtomicU32>()
                // there is a single weak reference shared among all strong references
                .write(AtomicU32::new(1));
        }
        // SAFETY: We trust the LayoutInfo to have the correct offset
        let value_ptr_addr = unsafe {
            allocated
                .as_ptr()
                .byte_offset(layout.value_offset)
                .cast::<()>()
        };
        let value_ptr = ptr_meta::from_raw_parts_mut(value_ptr_addr, meta);
        func(value_ptr);
        core::mem::forget(guard);
        // The guard no longer borrows the allocator, so now we can move it to the header.
        // SAFETY: Know that the allocated memory starts with a BrcHeader
        unsafe {
            allocated
                .byte_add(core::mem::offset_of!(BrcHeader<A>, alloc))
                .cast::<A>()
                .write(alloc);
        }
        // SAFETY: Allocated pointer is valid and never null
        // and the header is fully initialized
        unsafe { Self::from_raw(value_ptr) }
    }

    /// Return the number of strong references to the object,
    /// or an error if that value cannot be pricelessly determined.
    ///
    /// If this thread is the biased thread,
    /// then it can always determine the true reference count.
    /// If it is not the biased thread,
    /// then it can only approximate the value.
    ///
    /// # Errors
    /// Gives an [`ImpreciseRefCountError`] if not on the biased thread,
    /// and the true reference count cannot be determined.
    #[inline]
    pub fn strong_count(this: &Self) -> Result<usize, ImpreciseRefCountError> {
        // Arc::strong_count uses a Relaxed ordering here
        this.header().strong.strong_count(Ordering::Relaxed)
    }

    /// Returns the number of weak pointers to the object,
    /// or an error if that value cannot be precisely determined.
    ///
    /// # Errors
    /// Gives an [`ImpreciseRefCountError`] if not on the biased thread,
    /// and the true reference count cannot be determined.
    #[inline]
    pub fn weak_count(this: &Self) -> Result<usize, ImpreciseRefCountError> {
        let weak_count = this.header().weak_count.load(Ordering::Relaxed);
        if weak_count == WEAK_LOCKED_COUNT {
            Ok(0) // can only happen if there was one weak reference
        } else {
            // exclude the strong count shared by all weak references
            Ok((weak_count - 1) as usize)
        }
    }

    /// Determine if this [`Brc`] is uniquely owned.
    ///
    /// Mirrors [`std::sync::Arc::is_unique`].
    ///
    /// Due to the nature of biased reference counting,
    /// this may have false-negatives when called on a non-biased thread.
    /// However, it will never have false positives.
    /// See [`Self::strong_count`] for details.
    #[inline]
    pub fn is_unique(this: &Self) -> bool {
        if this.header().strong.is_definitely_not_unique() {
            // return false quickly if we are definitely not the biased thread
            return false;
        }
        // if there is only one weak count, then we may be unique
        // We still need to lock the reference count while we are checking.
        if this
            .header()
            .weak_count
            .compare_exchange(1, WEAK_LOCKED_COUNT, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let actually_unique = this.header().strong.is_unique();
            // now release the lock on the weak reference count
            this.header().weak_count.store(1, Ordering::Release);
            actually_unique
        } else {
            false // multiple weak references
        }
    }

    /// Return the biased reference count,
    /// or an error if that is not possible or sensible.
    ///
    /// This method is intended only for testing.
    #[doc(hidden)]
    pub fn biased_count(this: &Self) -> Result<usize, BiasedCountError> {
        this.header().strong.biased_count()
    }

    /// Return the shared reference count.
    ///
    /// This method is intended only for testing.
    #[doc(hidden)]
    pub fn shared_count(this: &Self) -> isize {
        this.header().strong.shared_count()
    }

    /// Return the biased and shared reference counts.
    ///
    /// This is a utility method equivalent to `(Self::biased_count(this), Self::shared_count(this))`.
    /// It is intended only for testing.
    #[doc(hidden)]
    pub fn biased_and_shared_counts(this: &Self) -> (Result<usize, BiasedCountError>, isize) {
        (Self::biased_count(this), Self::shared_count(this))
    }

    /// Return a mutable reference to the value in this [`Brc`],
    /// unsafely assuming if it is uniquely owned.
    ///
    /// See also [`Self::get_mut`] which clones the value instead of returning `None`.
    ///
    /// # Safety
    /// This is safe if [`Self::is_unique`] returns true,
    /// but due to false negatives from this function may be true in other cases.
    ///
    /// Trigger immediate undefined behavior if there are any other references to the inner value,
    /// as a `&mut T` reference must always be unique.
    #[inline]
    pub unsafe fn get_mut_unchecked(this: &mut Self) -> &mut T {
        // SAFETY: Caller guarantees this is valid
        unsafe { this.ptr.as_mut() }
    }

    /// Return a mutable reference to the value in this [`Brc`],
    /// or `None` if it is not uniquely owned.
    ///
    /// This may fail spuriously on a non-biased thread,
    /// due to inability to determine the true value of the reference count.
    /// In other words, [`Self::is_unique`] has false negatives.
    ///
    /// See also [`Self::make_mut`] which clones the value instead of returning `None`.
    #[inline]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        if Self::is_unique(this) {
            // SAFETY: Uniqueness makes this is safed
            unsafe { Some(Self::get_mut_unchecked(this)) }
        } else {
            None
        }
    }

    /// Return a mutable reference to this [`Brc`] if it is uniquely owned,
    /// or `Clone` it to make it unique otherwise.
    ///
    /// May `Clone` the value unnecessarily if uniqueness
    /// can not be guaranteed by [`Self::is_unique`].
    ///
    /// See also [`Self::get_mut`] which returns `None` instead of cloning the value.
    /// This mirrors the [`Arc::make_mut`] method, but may involve m
    #[inline]
    pub fn make_mut(this: &mut Self) -> &mut T
    where
        T: Clone,
        A: Clone,
    {
        // TODO: Implement the optimization that std::sync::Arc::make_mut does?
        // It allows this method to be slightly better than `is_unique`
        if Self::is_unique(this) {
            // SAFETY: Uniqueness makes this is safe
            unsafe { Self::get_mut_unchecked(this) }
        } else {
            let alloc = Self::allocator(this).clone();
            let value = &**this;
            *this = Self::new_with_in(|| T::clone(value), alloc);
            // SAFETY: We have ensured the reference is unique
            unsafe { Self::get_mut_unchecked(this) }
        }
    }

    /// Downgrade into a new weak reference.
    ///
    /// Mirrors [`Arc::downgrade`].
    #[must_use]
    pub fn downgrade(this: &Self) -> Weak<T, A>
    where
        T: SupportedWeakPointee,
    {
        let mut weak_count = this.header().weak_count.load(Ordering::Relaxed);
        loop {
            // spin if the weak count is locked (Arc::downgrade does this too)
            // the lock should only be held very briefly
            if weak_count == WEAK_LOCKED_COUNT {
                core::hint::spin_loop();
                weak_count = this.header().weak_count.load(Ordering::Relaxed);
                continue;
            }
            if weak_count > WEAK_OVERFLOW_THRESHOLD {
                runtime::fatal_errors::weak_refcnt_overflow();
            }
            match this.header().weak_count.compare_exchange_weak(
                weak_count,
                weak_count + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // SAFETY: Just incremented the weak reference count
                    unsafe {
                        return Weak::from_raw(Self::as_ptr(this));
                    }
                }
                Err(new_count) => {
                    weak_count = new_count;
                }
            }
        }
    }

    /// Create a [`Brc`] from a raw pointer,
    /// originating from [`Brc::into_raw`].
    ///
    /// Mirrors [`Arc::from_raw`].
    ///
    /// See also [`Brc::decrement_strong_count`], which directly mutates the reference count
    /// without creating a owned value.
    /// This is subject to roughly the same safety requirements.
    ///
    /// Because the [`Brc`] stores the allocator in the header (as discussed in the type docs),
    /// this function works fine for any allocator.
    /// However, a [`Brc::from_raw_in`] function is added for completeness.
    ///
    /// # Safety
    /// This must correspond exactly to an owned reference count from [`Brc::into_raw`],
    /// and is vulnerable to double-free if called multiple times on the same pointer.
    ///
    /// This is only valid for the result of [`Brc::into_raw`], not for any other piece of memory.
    ///
    /// # Panics
    /// This function is infallible.
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        Brc {
            // SAFETY: Cannot be null as it comes from `into_raw`
            ptr: unsafe { NonNull::new_unchecked(ptr.cast_mut()) },
            value_marker: PhantomData,
            alloc_marker: PhantomData,
        }
    }

    /// Create a [`Brc`] from a raw pointer along with its allocator,
    /// originating from [`Brc::into_raw_with_allocator`].
    ///
    /// Mirrors [`Arc::from_raw_in`].
    ///
    /// This function currently discards the allocator and calls [`Self::into_raw`].
    /// For future-proofing and compatibility with [`Arc`],
    /// it is required that the allocator is
    /// equivalent to calling [`Self::allocator`] on the original reference.
    ///
    /// # Safety
    /// Same requirements as [`Brc::from_raw`].
    ///
    /// # Panics
    /// This function is infallible unless the allocator's drop function panics.
    #[inline]
    pub unsafe fn from_raw_in(ptr: *const T, alloc: A) -> Self {
        drop(alloc);
        // SAFETY: Caller guarantees validity
        unsafe { Self::from_raw(ptr) }
    }

    /// Consumes this [`Brc`], converting it into a raw pointer.
    ///
    /// Mirrors [`Arc::into_raw`].
    /// Because the allocator is stored in the header,
    /// this works well with any allocator.
    ///
    /// # Safety
    /// This is perfectly safe, but may leak memory.
    ///
    /// # Panics
    /// This function is infallible.
    #[allow(clippy::wrong_self_convention, reason = "could conflict with deref")]
    #[inline]
    pub fn into_raw(this: Self) -> *const T {
        let value = ManuallyDrop::new(this);
        value.ptr.as_ptr().cast_const()
    }

    /// Consumes this [`Brc`], converting it into a raw pointer
    /// along with a copy of the underlying allocator.
    ///
    /// Mirrors [`Arc::into_raw_with_allocator`],
    /// but needs to add a `A: Clone` bound because the allocator is stored in the header
    /// (see type-level docs for details)
    /// If you just want to call [`Brc::from_raw_in`] later,
    /// prefer [`Brc::into_raw`] and [`Brc::from_raw`] which avoid cloning the allocator.
    /// Because the allocator is stored in the header,
    /// these functions work for any allocator (unlike [`Arc`]).
    ///
    /// # Safety
    /// This is perfectly safe, but may leak memory.
    ///
    /// # Panics
    /// This function is infallible unless the allocator clone function panics.
    #[allow(clippy::wrong_self_convention, reason = "could conflict with deref")]
    #[inline]
    pub fn into_raw_with_allocator(this: Self) -> (*const T, A)
    where
        A: Clone,
    {
        let allocator = Self::allocator(&this).clone();
        let value = ManuallyDrop::new(this);
        (value.ptr.as_ptr().cast_const(), allocator)
    }

    /// Convert this [`Brc`] into a raw pointer,
    /// without affecting the reference count.
    ///
    /// Mirrors [`Arc::as_ptr`].
    ///
    /// This will give the same result as [`Self::into_raw`] would,
    /// but does not consume ownership.
    ///
    /// # Panics
    /// This function is infallible.
    #[allow(
        clippy::wrong_self_convention,
        reason = "inherent method could conflict with deref"
    )]
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        this.ptr.as_ptr().cast_const()
    }

    /// Increments the strong reference count on the [`Brc`] associated with the specified pointer.
    ///
    /// Similarly to [`Self::clone_no_collect`], this does not implicitly call [`collect`].
    /// This is a low-level function which leaves that choice to the user.
    ///
    /// Mirrors [`Arc::increment_strong_count`]
    ///
    /// # Panics
    /// See [`Self::clone_no_collect`] for details.
    ///
    /// # Safety
    /// The pointer must have been obtained through [`Brc::into_raw`] or [`Brc::as_ptr`],
    /// have the correct type, and still point to valid memory (not dropped).
    #[inline]
    pub unsafe fn increment_strong_count(ptr: *const T) {
        // SAFETY: Caller guarantees the pointer is valid
        let this = ManuallyDrop::new(unsafe { Self::from_raw(ptr) });
        core::mem::forget(Self::clone_no_collect(&*this));
    }

    /// Decrements the strong reference count on the [`Brc`] associated with the specified pointer.
    ///
    /// Similarly to [`Self::drop_no_collect`], this does not implicitly call [`collect`].
    /// This is a low-level function which leaves that choice to the user.
    ///
    /// Mirrors [`Arc::decrement_strong_count`].
    ///
    /// # Panics
    /// See [`Self::drop_no_collect`] for details.
    ///
    /// # Safety
    /// The pointer must have been obtained through [`Brc::into_raw`] or [`Brc::as_ptr`],
    /// have the correct type, and still point to valid memory (not dropped).
    ///
    /// Each decrement must match a corresponding increment,
    /// or else use after free must occur.
    /// Must not decrement the last reference count while other [`Brc`] references are active.
    #[inline]
    pub unsafe fn decrement_strong_count(ptr: *const T) {
        // SAFETY: Caller guarantees pointer is active, corresponds to a real Brc
        Self::drop_no_collect(unsafe { Brc::from_raw(ptr) });
    }

    /// Checks if this points to the same object as the other.
    ///
    /// This does not compare pointer metadata for trait objects,
    /// just like [`std::sync::Arc::ptr_eq`] and [`core::ptr::addr_eq`].
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        // ignores trait object metadata, just like std::sync::Weak::ptr_eq
        // slice length is irrelevant because Weak<[T]> always points to the full slice
        core::ptr::addr_eq(Self::as_ptr(this), Self::as_ptr(other))
    }

    #[inline]
    fn layout(&self) -> LayoutInfo<A> {
        let this_layout = Layout::for_value(self.deref());
        // SAFETY: Cannot overflow since the value has already been allocated
        unsafe { LayoutInfo::new(this_layout).unwrap_unchecked() }
    }

    #[inline]
    unsafe fn header_ptr_for(ptr: NonNull<T>, layout: LayoutInfo<A>) -> NonNull<BrcHeader<A>> {
        let header_offset = layout.header_offset();
        // SAFETY: Caller guarantees pointer is spatially valid
        unsafe {
            ptr.cast::<u8>()
                .offset(header_offset)
                .cast::<BrcHeader<A>>()
        }
    }

    #[inline]
    fn header(&self) -> &BrcHeader<A> {
        let layout = self.layout();
        // SAFETY: The header pointer is valid
        unsafe { Self::header_ptr_for(self.ptr, layout).as_ref() }
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> Deref for Brc<T, A> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Object lives at least as long as we do
        unsafe { self.ptr.as_ref() }
    }
}

smart_pointer! {
    unsafe impl<T: ?Sized + SupportedPointee, A: Allocator> SmartPointer for Brc {}
}

// SAFETY: We are thread safe if T is
// We need to require T: Send to safely drop from other threads
unsafe impl<T: ?Sized + SupportedPointee + Sync + Send, A: Allocator + Send + Sync> Sync
    for Brc<T, A>
{
}
// SAFETY: We are thread safe if T is
unsafe impl<T: ?Sized + SupportedPointee + Sync + Send, A: Allocator + Send + Sync> Send
    for Brc<T, A>
{
}

// SAFETY: A Cloned Brc just increments the RC, so memory location is the same
unsafe impl<T: ?Sized + SupportedPointee, A: Allocator> CloneStableDeref for Brc<T, A> {}

//
// drop & clone logic
//

impl<T: ?Sized + SupportedPointee, A: Allocator> Brc<T, A> {
    /// Clone this reference without invoking [`collect`].
    ///
    /// # Panics
    /// Unlike [`Clone`], this function does not call [`collect`] and so will never panic.
    /// However, it may abort if the reference count overflows or internal state appears corrupted.
    #[inline]
    pub fn clone_no_collect(this: &Self) -> Self {
        this.header().strong.increment_strong();
        // SAFETY: Just successfully incremented the refcnt
        unsafe { Brc::from_raw(this.ptr.as_ptr()) }
    }

    /// Drop this reference without invoking [`collect`].
    ///
    /// # Panics
    /// Unlike [`drop`] which can panic due to [`collect`],
    /// this function only panics if the underlying destructor does.
    /// Similarly to [`drop`] the destructor may be deferred,
    /// meaning a panicking destructor may not happen right away.
    ///
    /// This function may abort if internal state appears corrupted.
    #[inline]
    pub fn drop_no_collect(this: Self) {
        let mut this = ManuallyDrop::new(this);
        // SAFETY: Default Drop impl not executed due to ManuallyDrop
        unsafe {
            Self::drop_no_collect_in_place(&mut this);
        }
    }

    /// Clone this reference count by incrementing the shared count.
    ///
    /// This function works the same regardless of the thread its called on.
    /// Even if this is the biased thread, this still increments the shared count.
    #[inline]
    pub fn clone_shared(this: &Self) -> Self {
        this.header().strong.increment_strong_shared();
        // SAFETY: Just incremented the reference count
        unsafe { Brc::from_raw(this.ptr.as_ptr()) }
    }

    /// Shared code for [`Self::drop_no_collect`] and [`drop`].
    ///
    /// # Safety
    /// Must be semantically owned, just like when calling [`core::ptr::drop_in_place`].
    #[inline]
    unsafe fn drop_no_collect_in_place(this: &mut Self) {
        let value: &T = this.deref();
        let context = DropContext::<T, A> {
            metadata: ptr_meta::metadata(value),
            value_offset: this.layout().value_offset,
            marker: PhantomData,
        };
        // SAFETY: Pointer is spatially valid
        let header_ptr = unsafe { Self::header_ptr_for(this.ptr, this.layout()).as_ptr() };
        // SAFETY: We own a reference count and the context is valid
        let result =
            unsafe { RawBrcHeader::decrement_strong(&raw const (*header_ptr).strong, context) };
        if result.should_drop {
            // SAFETY: We trust the drop function to return a valid result
            unsafe {
                context.dealloc(NonNull::new_unchecked(&raw mut (*header_ptr).strong));
            }
        }
    }
}

drop_may_dangle! {
// SAFETY: We respect the #[may_dangle] requirements
unsafe impl<#[may_dangle] T: ?Sized + SupportedPointee, A: Allocator> Drop for Brc<T, A> {
    /// Drops a reference to the underlying object,
    /// potentially freeing it if there are otherwise no references.
    ///
    /// This implicitly calls [`collect`] to help cleanup garbage from other threads.
    /// Use [`Self::drop_no_collect`] to avoid this.
    ///
    /// Due to the nature of biased reference counting,
    /// there are some cases where destruction may be deferred.
    ///
    /// # Panics
    /// This may panic if the underlying destructor panics,
    /// or if [`collect`] panics while executing a deferred destructor.
    ///
    /// This may abort if internal state appears corrupted.
    #[inline]
    fn drop(&mut self) {
        #[cfg(not(any(biasedrc_no_implicit_collect, biasedrc_no_implicit_collect_drop)))]
        collect();
        // SAFETY: Drop function is executed at most once
        // and Brc cannot be used once it completes.
        unsafe {
            Self::drop_no_collect_in_place(self);
        }
    }
}
}
impl<T: ?Sized + SupportedPointee, A: Allocator> Clone for Brc<T, A> {
    /// Create a new reference to the underlying object.
    ///
    /// This implicitly calls [`collect`] to help cleanup garbage from other threads.
    /// Use [`Self::clone_no_collect`] to avoid this.
    ///
    /// # Panics
    /// This function will panic only if [`collect`] panics.
    ///
    /// This function may abort if internal state appears corrupted,
    /// or if a reference count overflows.
    #[inline]
    fn clone(&self) -> Self {
        #[cfg(not(any(biasedrc_no_implicit_collect, biasedrc_no_implicit_collect_clone)))]
        collect();
        Self::clone_no_collect(self)
    }
}

struct DropContext<T: ?Sized + SupportedPointee, A: Allocator> {
    metadata: <T as Pointee>::Metadata,
    value_offset: isize,
    marker: PhantomData<fn(*mut T, A)>,
}
impl<T: ?Sized + SupportedPointee, A: Allocator> Copy for DropContext<T, A> {}
impl<T: ?Sized + SupportedPointee, A: Allocator> Clone for DropContext<T, A> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> DropInfo for DropContext<T, A> {
    #[inline]
    fn value_offset(&self) -> isize {
        self.value_offset
    }

    #[inline]
    fn erased_context(&self) -> ErasedDestructorContext {
        self.metadata.to_context()
    }

    #[inline] // marked inline because sometimes called from a generic context
    unsafe fn erased_dealloc(
        header_ptr: NonNull<RawBrcHeader>,
        ctx: ErasedDestructorContext,
        value_offset: isize,
    ) {
        let value: *mut T = crate::ptr_meta::from_raw_parts_mut(
            // SAFETY: Caller guarantees that the value_offset is valid
            unsafe { header_ptr.as_ptr().byte_offset(value_offset).cast::<()>() },
            // SAFETY: We know that the context is valid
            unsafe { <T::Metadata as SupportedMetadata>::from_context(ctx) },
        );
        /// Guard to drop the weak reference shared by all strong references.
        ///
        /// This guard ensures that the underlying memory is freed,
        /// even if the destructor of `T` panics.
        ///
        /// We can not use a real [`Weak`] pointer directly as that has stricter bounds on `T`
        /// in order to do the layout calculation.
        struct WeakDropGuard<A: Allocator> {
            header_ptr: NonNull<BrcHeader<A>>,
            layout_info: LayoutInfo<A>,
        }
        impl<A: Allocator> Drop for WeakDropGuard<A> {
            #[inline]
            fn drop(&mut self) {
                // SAFETY: Safe to drop because we are last strong reference
                unsafe {
                    BrcHeader::<A>::drop_weak(self.header_ptr.cast().as_ptr(), self.layout_info);
                }
            }
        }
        // SAFETY: Valid to get a &T since the valuehas not been dropped yet.
        // We also know that the layout will not overflow as allocation already succeeded.
        // This violates stacked borrow but works fine for tree borrows.
        let layout_info = unsafe { LayoutInfo::<A>::for_value(&*value) };
        debug_assert_eq!(layout_info.value_offset, value_offset);
        let weak_guard = WeakDropGuard {
            header_ptr: header_ptr.cast(),
            layout_info,
        };
        if core::mem::needs_drop::<T>() {
            // SAFETY: Caller guarantees this is not invoked until it is valid to drop
            unsafe { core::ptr::drop_in_place(value) }
        }
        const {
            assert!(!RawBrcHeader::NEEDS_DROP);
        }
        // Explicitly drop the weak reference shared by all the strong references
        drop(weak_guard);
    }
}
