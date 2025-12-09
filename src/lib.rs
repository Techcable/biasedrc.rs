//! An implementation of [biased reference counting] for Rust.
//!
//! This crate requires the standard library due to use of [`std::thread_local!`].
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195
#![cfg_attr(feature = "nightly-ptr-meta", feature(ptr_metadata))]
#![cfg_attr(feature = "nightly-coerce", feature(coerce_unsized, unsize))]
#![cfg_attr(feature = "nightly-allocator", feature(allocator_api))]
#![deny(
    missing_docs,
    clippy::std_instead_of_core,
    clippy::std_instead_of_alloc,
    clippy::alloc_instead_of_core
)]

extern crate alloc;

#[cfg(feature = "nightly-allocator")]
use alloc as allocator_api;
#[cfg(not(feature = "nightly-allocator"))]
use allocator_api2 as allocator_api;
#[cfg(feature = "nightly-ptr-meta")]
use core::ptr as ptr_meta;
#[cfg(feature = "nightly-coerce")]
use core::{marker::Unsize, ops::CoerceUnsized};
#[cfg(not(feature = "nightly-ptr-meta"))]
use ptr_meta_stable as ptr_meta;

#[allow(unused_imports, clippy::disallowed_types, reason = "used for docs")]
use alloc::sync::Arc;
use core::alloc::Layout;
use core::borrow::Borrow;
use core::cmp::Ordering;
use core::error::Error;
use core::ffi::c_void;
use core::fmt::{Debug, Display, Formatter};
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr::NonNull;
use pointee::{SupportedMetadata, SupportedPointeeInternal};
use ptr_meta::Pointee;
use stable_deref_trait::{CloneStableDeref, StableDeref};

use allocator_api::alloc::{Allocator, Global};

#[cfg(feature = "arc-swap")]
mod arc_swap;
mod runtime;
#[cfg(feature = "serde")]
mod serde;

use crate::runtime::{DropInfo, ErasedDestructorContext, RawBrcHeader};

pub use crate::runtime::{BiasedCountError, ImpreciseRefCountError, collect, collect_force};

/// Combines the reference counts with additional metadata.
///
/// This is `#[repr(C)]` because [`Brc::alloc_with_in`] initializes it field-by-field.
#[repr(C)]
struct BrcHeader<A: Allocator> {
    rc: RawBrcHeader,
    /// This is stored in the header because we cannot otherwise pass a monomorphized `A`
    /// to an [`runtime::ErasedDropInfo`].
    ///
    /// The erased drop info is necessary to add the `Brc` to the merge queue.
    alloc: ManuallyDrop<A>,
}

struct LayoutInfo<A: Allocator> {
    value_offset: isize,
    full_layout: Layout,
    /// Our calculations depend on the layout of the allocator.
    alloc_marker: PhantomData<fn(A)>,
}
impl<A: Allocator> LayoutInfo<A> {
    #[inline]
    pub fn new(layout: Layout) -> Result<Self, core::alloc::LayoutError> {
        let (full_layout, value_offset) = Layout::new::<BrcHeader<A>>().extend(layout)?;
        Ok(LayoutInfo {
            full_layout,
            #[expect(clippy::cast_possible_wrap, reason = "offset fits in isize")]
            value_offset: value_offset as isize,
            alloc_marker: PhantomData,
        })
    }
    /// Compute the offset (in bytes) from the value to get to the header.
    #[inline]
    fn header_offset(&self) -> isize {
        // SAFETY: Cannot overflow because offset is guaranteed to fit in `isize`
        unsafe { 0isize.unchecked_sub(self.value_offset) }
    }
}

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
    /// The behavior on out of memory is determined by [`alloc::alloc::handle_alloc_error`],
    /// which may involve a panic or an abort.
    #[inline]
    pub fn new(value: T) -> Brc<T> {
        Self::new_with_in(|| value, Global)
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
            Self::alloc_with_in(
                Layout::new::<T>(),
                (),
                |target| target.write(func()),
                Global,
            )
        }
    }
}
impl<T, A: Allocator> Brc<T, A> {
    /// Construct a new [`Brc`] with the specified value,
    /// using a particular allocator.
    #[inline]
    pub fn new_in(value: T, alloc: A) -> Self {
        Self::new_with_in(|| value, alloc)
    }

    /// Construct a new [`Brc`], using a closure to initialize the specified value,
    /// along with using a particular allocator.
    ///
    /// This can potentially improve performance by allowing values to be constructed in place.
    #[inline]
    pub fn new_with_in(func: impl FnOnce() -> T, alloc: A) -> Self {
        // SAFETY: Either we fully initialize the newly allocated memory,
        // or the initialization function panics
        unsafe { Self::alloc_with_in(Layout::new::<T>(), (), |target| target.write(func()), alloc) }
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
}
impl<T, A: Allocator> Brc<[T], A> {
    /// Allocate a slice of memory rom a [`Layout`] and a,
    /// whose length is trusted to be exact.
    ///
    /// The iterator is permitted to panic, both in [`Iterator::next`] and [`Drop`].
    ///
    /// # Safety
    /// The layout must match the result of [`Layout::array`].
    /// The calculation is moved to the caller to potentially avoid a panic.
    ///
    /// The iterator must either panic or yield precisely as many elements as its length.
    #[deny(clippy::multiple_unsafe_ops_per_block)]
    pub(crate) unsafe fn from_iter_exact_trusted_in(
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
                    if core::mem::needs_drop::<T>() {
                        let initialized =
                            core::ptr::slice_from_raw_parts_mut(self.dest, self.initialized_len);
                        // SAFETY: Trust that `len` items have been initialized
                        unsafe {
                            core::ptr::drop_in_place(initialized);
                        }
                    }
                }
            }
            let mut guard = PartialDropGuard {
                dest,
                initialized_len: 0,
            };
            for index in 0..len {
                guard.initialized_len = index;
                // SAFETY: We trust the length to be exact
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
            drop(iter); // this is permitted to panic
            core::mem::forget(guard); // finished initialization
        };
        // SAFETY: Either fully initializes the memory or panics
        // We trust the iterator to be exact.
        unsafe { Brc::alloc_with_in(layout, len, do_init, alloc) }
    }
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
    #[inline(always)] // Inlining means we can potentially eliminate the guard & layout calculation
    unsafe fn alloc_with_in(
        layout: Layout,
        meta: T::Metadata,
        func: impl FnOnce(*mut T),
        alloc: A,
    ) -> Self {
        #[cfg(not(biasedrc_no_implicit_collect))]
        collect();
        #[cold]
        #[inline(never)]
        fn layout_overflow() -> ! {
            panic!("Layout of Brc would overflow an isize")
        }
        let Ok(layout) = LayoutInfo::<A>::new(layout) else {
            layout_overflow()
        };
        struct CleanupGuard<A: Allocator> {
            ptr: NonNull<u8>,
            layout: Layout,
            alloc: Option<A>,
        }
        impl<A: Allocator> Drop for CleanupGuard<A> {
            #[inline]
            fn drop(&mut self) {
                let alloc = self.alloc.take().unwrap();
                // SAFETY: We know the pointer is valid since we just allocated it
                // We are careful to forget the guard if we are successful
                unsafe { alloc.deallocate(self.ptr, self.layout) }
            }
        }
        let Ok(allocated) = alloc.allocate(layout.full_layout) else {
            alloc::alloc::handle_alloc_error(layout.full_layout);
        };
        let mut guard = CleanupGuard {
            ptr: allocated.cast(),
            layout: layout.full_layout,
            alloc: Some(alloc),
        };
        // SAFETY: Memory is newly allocated so it is known to be valid
        // The RawBrcHeader is pinned immediately after it is created
        unsafe {
            allocated.cast::<RawBrcHeader>().write(RawBrcHeader::init());
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
        let alloc = guard.alloc.take().unwrap();
        core::mem::forget(guard);
        // Now we have the allocator, we can initialize the rest of the header
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
        this.header().rc.strong_count()
    }

    /// Determine if this [`Brc`] is uniquely owned.
    ///
    /// Due to the nature of biased reference counting,
    /// this may have false-negatives when called on a non-biased thread.
    /// However, it will never have false positives.
    /// See [`Self::strong_count`] for details.
    #[inline]
    pub fn is_unique(this: &Self) -> bool {
        this.header().rc.is_unique()
    }

    /// Return the biased reference count,
    /// or an error if that is not possible or sensible.
    ///
    /// This method is intended only for testing.
    #[doc(hidden)]
    pub fn biased_count(this: &Self) -> Result<usize, BiasedCountError> {
        this.header().rc.biased_count()
    }

    /// Return the shared reference count.
    ///
    /// This method is intended only for testing.
    #[doc(hidden)]
    pub fn shared_count(this: &Self) -> isize {
        this.header().rc.shared_count()
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

    /// Clone this reference without invoking [`collect`].
    ///
    /// # Panics
    /// Unlike [`Clone`], this function does not call [`collect`] and so will never panic.
    /// However, it may abort if the reference count overflows or internal state appears corrupted.
    #[inline]
    pub fn clone_no_collect(this: &Self) -> Self {
        this.header().rc.increment_strong();
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
        // SAFETY: We own a reference count and the context is valid
        let result = unsafe { this.header().rc.decrement_strong(context) };
        if result.should_drop {
            // SAFETY: We trust the drop function to return a valid result
            unsafe {
                context.dealloc(NonNull::new_unchecked(
                    (&raw const this.header().rc).cast_mut(),
                ));
            }
        }
    }

    #[inline]
    fn layout(&self) -> LayoutInfo<A> {
        let this_layout = Layout::for_value(self.deref());
        // SAFETY: Cannot overflow since the value has already been allocated
        unsafe { LayoutInfo::new(this_layout).unwrap_unchecked() }
    }

    #[inline]
    fn header(&self) -> &BrcHeader<A> {
        let header_offset = self.layout().header_offset();
        // SAFETY: A Brc always has a valid header, which can then be dereferenced
        unsafe {
            self.ptr
                .cast::<u8>()
                .offset(header_offset)
                .cast::<BrcHeader<A>>()
                .as_ref()
        }
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
impl<T: ?Sized + SupportedPointee, A: Allocator> Drop for Brc<T, A> {
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
        #[cfg(not(biasedrc_no_implicit_collect))]
        collect();
        // SAFETY: Drop function is executed at most once
        // and Brc cannot be used once it completes.
        unsafe {
            Self::drop_no_collect_in_place(self);
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
        #[cfg(not(biasedrc_no_implicit_collect))]
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

    #[inline]
    unsafe fn erased_dealloc(
        header_ptr: NonNull<RawBrcHeader>,
        ctx: ErasedDestructorContext,
        value_offset: isize,
    ) {
        let value: *mut T = ptr_meta::from_raw_parts_mut(
            // SAFETY: Caller guarantees that the value_offset is valid
            unsafe { header_ptr.as_ptr().byte_offset(value_offset).cast::<()>() },
            // SAFETY: We know that the context is valid
            unsafe { <T::Metadata as SupportedMetadata>::from_context(ctx) },
        );
        // SAFETY: Valid since T has not been dropped yet.
        // However, this violates stacked borrow. It works fine for tree borrows)
        let layout = unsafe { Layout::for_value(&*value) };
        if core::mem::needs_drop::<T>() {
            // SAFETY: Caller guarantees this is not invoked until it is valid to drop
            unsafe { core::ptr::drop_in_place(value) }
        }
        const {
            assert!(!RawBrcHeader::NEEDS_DROP);
        }
        // Move the allocator out of the header
        // SAFETY: Will never be used again after dealloc
        let alloc: A = unsafe {
            header_ptr
                .as_ptr()
                .cast::<BrcHeader<A>>()
                .byte_add(core::mem::offset_of!(BrcHeader<A>, alloc))
                .cast::<A>()
                .read()
        };
        // SAFETY: Know the layout will not overflow since already allocated
        let layout_info = unsafe { LayoutInfo::<A>::new(layout).unwrap_unchecked() };
        debug_assert_eq!(layout_info.value_offset, value_offset);
        // SAFETY: Caller guarantees it is valid to drop the header too
        unsafe { alloc.deallocate(header_ptr.cast(), layout_info.full_layout) };
    }
}

/// A type that can be used in a [`Brc`].
///
/// Currently, this includes all sized objects, slices, and
/// trait objects that the [`ptr_meta`] crate supports.
/// However, due to implementation limitations, it may not include all types the future
/// (in particular, extern types can never be meaningfully supported).
///
/// # Safety
/// Effectively sealed, so all implementations can be trusted.
pub trait SupportedPointee: SupportedPointeeInternal {}

/// The internals of [`SupportedPointee`], using the [`ptr_meta`] crate.
mod pointee {
    use crate::runtime::ErasedDestructorContext;
    #[cfg(feature = "nightly-ptr-meta")]
    use core::ptr as ptr_meta;
    use ptr_meta::{DynMetadata, Pointee};
    #[cfg(not(feature = "nightly-ptr-meta"))]
    use ptr_meta_stable as ptr_meta;

    /// The sealed internals of [`SupportedPointee`], hidden from the public.
    ///
    /// This performs double-duty by ensuring the trait is sealed.
    pub trait SupportedPointeeInternal: Pointee<Metadata: SupportedMetadata> {}
    impl<T: ?Sized + Pointee> SupportedPointeeInternal for T where T::Metadata: SupportedMetadata {}
    impl<T: ?Sized + Pointee> super::SupportedPointee for T where T::Metadata: SupportedMetadata {}

    /// Indicates that the metadata is supported, meaning it is at most pointer sized.
    ///
    /// # Safety
    /// This trait is effectively sealed, so it can be trusted to work correctly.
    pub trait SupportedMetadata: Copy {
        fn to_context(self) -> ErasedDestructorContext;
        unsafe fn from_context(ctx: ErasedDestructorContext) -> Self;
    }
    impl SupportedMetadata for usize {
        #[inline]
        fn to_context(self) -> ErasedDestructorContext {
            ErasedDestructorContext(core::ptr::without_provenance_mut(self))
        }

        #[inline]
        unsafe fn from_context(ctx: ErasedDestructorContext) -> Self {
            ctx.0.addr()
        }
    }
    impl SupportedMetadata for () {
        #[inline]
        fn to_context(self) -> ErasedDestructorContext {
            ErasedDestructorContext(core::ptr::null_mut())
        }

        #[inline(always)]
        unsafe fn from_context(_ctx: ErasedDestructorContext) -> Self {
            /* nothing to do */
        }
    }
    impl<Dyn: ?Sized> SupportedMetadata for DynMetadata<Dyn> {
        #[inline]
        fn to_context(self) -> ErasedDestructorContext {
            // SAFETY: DynMetadata should just be a vtable pointer
            unsafe { core::mem::transmute::<DynMetadata<Dyn>, ErasedDestructorContext>(self) }
        }
        #[inline]
        unsafe fn from_context(ctx: ErasedDestructorContext) -> Self {
            // SAFETY: DynMetadata should just be a vtable pointer, which we trust to be valid
            unsafe { core::mem::transmute::<ErasedDestructorContext, DynMetadata<Dyn>>(ctx) }
        }
    }
}

//
// smart-pointer boilerplate
//

/// An [`archery::SharedPointerKind`] for [`Brc`].
#[repr(transparent)]
pub struct BrcK {
    value_ptr: NonNull<c_void>,
}
impl BrcK {
    #[inline]
    fn from_brc<T>(brc: Brc<T>) -> Self {
        let brc = ManuallyDrop::new(brc);
        BrcK {
            value_ptr: brc.ptr.cast(),
        }
    }
    #[inline]
    unsafe fn into_brc<T>(self) -> Brc<T> {
        // SAFETY: Type guaranteed by the caller
        unsafe { Brc::from_raw(self.value_ptr.as_ptr().cast::<T>()) }
    }
    /// Convert this into a mutable reference to a [`Brc`].
    ///
    /// Works because both types are `#[repr(transparent)]` wrappers around a pointer.
    ///
    /// # Safety
    /// Must be the correct type.
    #[inline]
    unsafe fn as_brc_mut<T>(&mut self) -> &mut Brc<T> {
        // SAFETY: Caller guarantees correct type
        unsafe { &mut *core::ptr::from_mut::<Self>(self).cast::<Brc<T>>() }
    }
    /// Convert this into a reference to a [`Brc`].
    ///
    /// Works because both types are `#[repr(transparent)]` wrappers around a pointer.
    ///
    /// # Safety
    /// Must be the correct type.
    #[inline]
    unsafe fn as_brc<T>(&self) -> &Brc<T> {
        // SAFETY: Caller guarantees correct type
        unsafe { &*core::ptr::from_ref::<Self>(self).cast::<Brc<T>>() }
    }
}
// SAFETY: We work like a normal Arc
unsafe impl archery::SharedPointerKind for BrcK {
    #[inline]
    fn new<T>(v: T) -> Self {
        Self::from_brc(Brc::new(v))
    }

    #[inline]
    fn from_box<T>(v: Box<T>) -> Self {
        Self::from_brc::<T>(Brc::from(v))
    }

    #[inline]
    unsafe fn as_ptr<T>(&self) -> *const T {
        self.value_ptr.as_ptr().cast()
    }

    #[inline]
    unsafe fn deref<T>(&self) -> &T {
        // SAFETY: Caller guarantees `T` is correct type
        unsafe { self.value_ptr.cast::<T>().as_ref() }
    }

    #[inline]
    unsafe fn try_unwrap<T>(self) -> Result<T, Self> {
        // SAFETY: Caller guarantees `T` is correct type
        unsafe { Brc::try_unwrap(Self::into_brc(self)).map_err(Self::from_brc) }
    }

    #[inline]
    unsafe fn get_mut<T>(&mut self) -> Option<&mut T> {
        // SAFETY: Caller guarantees `T` is correct type
        Brc::get_mut(unsafe { self.as_brc_mut() })
    }

    #[inline]
    unsafe fn make_mut<T: Clone>(&mut self) -> &mut T {
        // SAFETY: Caller guarantees `T` is correct type
        Brc::make_mut(unsafe { self.as_brc_mut() })
    }

    unsafe fn strong_count<T>(&self) -> usize {
        unimplemented!("A Brc cannot always give an accurate strong count")
    }

    #[inline]
    unsafe fn clone<T>(&self) -> Self {
        // SAFETY: Caller guarantees the correct type
        Self::from_brc(unsafe { Brc::clone(self.as_brc::<T>()) })
    }

    #[inline]
    unsafe fn drop<T>(&mut self) {
        // SAFETY: Caller guarantees `T` is correct type, and it is safe to drop
        unsafe { core::ptr::drop_in_place::<Brc<T>>(self.as_brc_mut()) }
    }
}
impl Debug for BrcK {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BrcK").finish_non_exhaustive()
    }
}

impl<T: ?Sized + SupportedPointee + Error, A: Allocator> Error for Brc<T, A> {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.deref().source()
    }

    #[allow(deprecated, reason = "delegate")]
    #[inline]
    fn description(&self) -> &str {
        self.deref().description()
    }

    #[allow(deprecated, reason = "delegate")]
    #[inline]
    fn cause(&self) -> Option<&dyn Error> {
        self.deref().cause()
    }
}
// SAFETY: A Cloned Brc just increments the RC, so memory location is the same
unsafe impl<T: ?Sized + SupportedPointee, A: Allocator> CloneStableDeref for Brc<T, A> {}
// SAFETY: A Brc is heap allocated so the memory never moves
unsafe impl<T: ?Sized + SupportedPointee, A: Allocator> StableDeref for Brc<T, A> {}
impl<T> From<T> for Brc<T> {
    #[inline]
    fn from(value: T) -> Self {
        Brc::new(value)
    }
}
/// Convert from a [`Box`] to a [`Brc`].
///
/// This conversion is guaranteed not to copy values to the stack,
/// which means large values cannot trigger stack overflow.
///
/// However, this cannot reuse the allocation as a [`Box`] has no room to hold the reference count.
impl<T: ?Sized + SupportedPointee> From<Box<T>> for Brc<T> {
    #[inline]
    fn from(value: Box<T>) -> Self {
        let meta = ptr_meta::metadata(&raw const *value);
        let layout = Layout::for_value::<T>(&*value);
        // SAFETY: Fully initializes the value by copying from the Box.
        // Can only fail if the allocation does
        unsafe {
            Self::alloc_with_in(
                layout,
                meta,
                move |dest| {
                    let value = ManuallyDrop::new(value);
                    dest.cast::<u8>().copy_from_nonoverlapping(
                        core::ptr::from_ref::<T>(&**value).cast::<u8>(),
                        layout.size(),
                    );
                    drop(ManuallyDrop::into_inner(value));
                },
                Global,
            )
        }
    }
}
impl<T: Clone> From<&[T]> for Brc<[T]> {
    fn from(src: &[T]) -> Self {
        let layout = Layout::for_value(src);
        // SAFETY: We trust the slice iterator + cloned() to have correct length or panic
        unsafe { Self::from_iter_exact_trusted_in(layout, src.iter().cloned(), Global) }
    }
}
impl<T> From<Vec<T>> for Brc<[T]> {
    fn from(value: Vec<T>) -> Self {
        let layout = Layout::for_value(value.as_slice());
        // SAFETY: We trust the Vec iterator to have the correct length
        unsafe { Self::from_iter_exact_trusted_in(layout, value.into_iter(), Global) }
    }
}
impl From<&str> for Brc<str> {
    #[inline]
    fn from(value: &str) -> Self {
        let bytes = Brc::<[u8]>::from(value.as_bytes());
        // SAFETY: A str has the same repr as [u8], and we know the UTF8 is valid
        unsafe { Brc::from_raw(Brc::into_raw(bytes) as *mut str) }
    }
}
impl<T> FromIterator<T> for Brc<[T]> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, upper) = iter.size_hint();
        if Some(lower) == upper {
            /// Verifies that the iterator has the claimed length,
            /// and panics if it doesn't.
            struct AssertExactIter<I: Iterator<Item = T>, T> {
                inner: I,
                len: usize,
            }
            impl<I: Iterator<Item = T>, T> Iterator for AssertExactIter<I, T> {
                type Item = T;
                #[inline]
                #[track_caller]
                fn next(&mut self) -> Option<Self::Item> {
                    match (self.inner.next(), self.len) {
                        (None, 0) => None,
                        (Some(_), 0) => panic!("Iterator yielded more items than claimed length"),
                        (Some(item), _) => {
                            self.len -= 1;
                            Some(item)
                        }
                        (None, _) => panic!("Iterator yielded fewer items than claimed length"),
                    }
                }
                #[inline]
                fn size_hint(&self) -> (usize, Option<usize>) {
                    (self.len, Some(self.len))
                }
            }
            impl<I: Iterator<Item = T>, T> ExactSizeIterator for AssertExactIter<I, T> {}
            let len = lower;
            let layout = Layout::array::<T>(len).expect("Layout overflow");
            // SAFETY: The AssertExactIter verifies the length is correct
            // The Layout is correct
            unsafe {
                Self::from_iter_exact_trusted_in(
                    layout,
                    AssertExactIter { len, inner: iter },
                    Global,
                )
            }
        } else {
            // need to buffer
            iter.collect::<Vec<T>>().into()
        }
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> Borrow<T> for Brc<T, A> {
    #[inline]
    fn borrow(&self) -> &T {
        self.deref()
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> AsRef<T> for Brc<T, A> {
    #[inline]
    fn as_ref(&self) -> &T {
        self.deref()
    }
}
impl<T: ?Sized + SupportedPointee + Debug, A: Allocator> Debug for Brc<T, A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}
impl<T: ?Sized + SupportedPointee + Display, A: Allocator> Display for Brc<T, A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        Display::fmt(self.deref(), f)
    }
}
impl<T: ?Sized + SupportedPointee + PartialEq, A: Allocator> PartialEq for Brc<T, A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}
impl<T: ?Sized + SupportedPointee + PartialOrd, A: Allocator> PartialOrd for Brc<T, A> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}
impl<T: ?Sized + SupportedPointee + Ord, A: Allocator> Ord for Brc<T, A> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.deref().cmp(other.deref())
    }
}
impl<T: ?Sized + SupportedPointee + Hash, A: Allocator> Hash for Brc<T, A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}
impl<T: ?Sized + SupportedPointee + Eq, A: Allocator> Eq for Brc<T, A> {}
impl<T: ?Sized + SupportedPointee, A: Allocator> Unpin for Brc<T, A> {}
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

#[cfg(feature = "nightly-coerce")]
impl<T: ?Sized, U: ?Sized> CoerceUnsized<Brc<U>> for Brc<T>
where
    T: Unsize<U> + SupportedPointee,
    U: SupportedPointee,
{
}

// SAFETY: Preserves target and provenance in replace_ptr
unsafe impl<T, U: ?Sized + SupportedPointee> unsize::CoerciblePtr<U> for Brc<T> {
    type Pointee = T;
    type Output = Brc<U>;

    #[inline]
    fn as_sized_ptr(&mut self) -> *mut Self::Pointee {
        // Use deref to acquire pointer to self
        // NOTE: Turning this into an &mut T is UB if there is shared ownership
        core::ptr::from_ref(&**self).cast_mut()
    }

    #[inline]
    unsafe fn replace_ptr(self, new: *mut U) -> Self::Output {
        // SAFETY: Caller has guaranteed that `new` is
        // just an unsized version of the original
        //
        // Ownership is correctly transferred from `self` to result.

        // Provenance transferred into `raw` as per `into_raw`.
        let raw = Self::into_raw(self).cast_mut();
        // SAFETY: Provenance merged into `new` as per `replace_ptr`.
        let new: *mut U = unsafe { <*mut T as unsize::CoerciblePtr<U>>::replace_ptr(raw, new) };
        // SAFETY: Provenance transferred as per `from_raw`, originally from `into_raw`
        unsafe { Brc::from_raw(new) }
    }
}
