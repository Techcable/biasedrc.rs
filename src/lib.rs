//! An implementation of [biased reference counting] for Rust.
//!
//! This crate requires the standard library due to use of [`std::thread_local!`].
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195
//!
//! # Prior Art
//! - [trc](https://github.com/EricLBuehler/trc) - Requires explicit choice of either `SharedTrc` or `Trc`,
//!   avoiding need for runtime checks but preventing use as a drop-in replacement for `Arc`
//! - [hybrid_rc](https://gitlab.com/cg909/rust-hybrid-rc) - Appears to require a similar choice as `trc` between shared and local references.
#![cfg_attr(feature = "nightly-ptr-meta", feature(ptr_metadata))]
#![cfg_attr(feature = "nightly-coerce", feature(coerce_unsized, unsize))]
#![cfg_attr(feature = "nightly-ptr-layout", feature(layout_for_ptr))]
#![cfg_attr(feature = "nightly-allocator", feature(allocator_api))]
#![cfg_attr(feature = "nightly-may-dangle", feature(dropck_eyepatch))]
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
use core::cmp;
use core::error::Error;
use core::fmt::{Debug, Display, Formatter};
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::num::NonZeroUsize;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use pointee::{SupportedMetadata, SupportedPointeeInternal, SupportedWeakPointeeInternal};
use ptr_meta::Pointee;
use stable_deref_trait::{CloneStableDeref, StableDeref};

use allocator_api::alloc::{Allocator, Global};

#[cfg(feature = "arc-swap")]
mod arc_swap;
#[cfg(feature = "archery")]
mod archery;
mod runtime;
#[cfg(feature = "serde")]
mod serde;

use crate::runtime::{DropInfo, ErasedDestructorContext, RawBrcHeader};

#[cfg(feature = "archery")]
pub use self::archery::BrcK;
pub use crate::runtime::{BiasedCountError, ImpreciseRefCountError, collect, collect_force};

const WEAK_LOCKED_COUNT: u32 = u32::MAX;
/// If the weak reference passes this point,
/// it should be considered to have overflown.
const WEAK_OVERFLOW_THRESHOLD: u32 = u32::MAX / 2;

/// Combines the reference counts with additional metadata.
///
/// This is `#[repr(C)]` because [`Brc::alloc_with_in`] initializes it field-by-field.
#[repr(C)]
struct BrcHeader<A: Allocator> {
    rc: RawBrcHeader,
    /// The weak reference count.
    ///
    /// May be [`WEAK_LOCKED_COUNT`] to indicate that it is "locked",
    /// which is necessary to implement [`Brc::get_mut`] and [`Brc::make_mut`].
    weak_count: AtomicU32,
    /// This is stored in the header because we cannot otherwise pass a monomorphized `A`
    /// to an [`runtime::ErasedDropInfo`].
    ///
    /// The erased drop info is necessary to add the `Brc` to the merge queue.
    alloc: ManuallyDrop<A>,
}

impl<A: Allocator> BrcHeader<A> {
    /// Drop a weak reference associated with the header.
    ///
    /// Requires passing layout information,
    /// as by now the underlying value `T` may have been destroyed
    /// and the [`Layout::for_value_raw`] method is unstable.
    #[inline]
    unsafe fn drop_weak(header_ptr: *mut Self, layout_info: LayoutInfo<A>) {
        // SAFETY: Caller guarantees header pointer is valid
        let weak_count = unsafe { &(*header_ptr).weak_count };
        // The reasoning in Arc::drop/Weak::drop justifies why we can weaken this to Release
        // with an acquire fence afterward.
        // The reasoning in Weak::drop explains why we don't need to check if we are locked
        if weak_count.fetch_sub(1, Ordering::Acquire) == 1 {
            // this is not moved into the cold path because
            // it may be possible to fold into the load/store
            atomic::fence(Ordering::Acquire);
            // SAFETY: We just verified we are the last reference,
            // caller guarantees the other requirements
            unsafe { Self::drop_weak_slow(header_ptr, layout_info.full_layout) }
        }
    }

    #[cold]
    unsafe fn drop_weak_slow(header_ptr: *mut Self, full_layout: Layout) {
        // SAFETY: Will not use the allocator after deallocation
        let alloc = unsafe { Self::take_allocator(header_ptr) };
        // SAFETY: Caller guarantees it is okay to
        unsafe {
            alloc.deallocate(NonNull::new_unchecked(header_ptr.cast::<u8>()), full_layout);
        }
    }

    /// Consume ownership of the allocator.
    ///
    /// # Safety
    /// Undefined behavior if the allocator is ever used again,
    /// just like with [`ManuallyDrop::take`].
    #[inline]
    unsafe fn take_allocator(header_ptr: *const Self) -> A {
        // SAFETY: Caller guarantees allocator will not be used again
        unsafe {
            header_ptr
                .byte_add(core::mem::offset_of!(Self, alloc))
                .cast::<A>()
                .read()
        }
    }
}

struct LayoutInfo<A: Allocator> {
    value_offset: isize,
    full_layout: Layout,
    /// Our calculations depend on the layout of the allocator.
    alloc_marker: PhantomData<fn(A)>,
}
impl<A: Allocator> Copy for LayoutInfo<A> {}
impl<A: Allocator> Clone for LayoutInfo<A> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<A: Allocator> LayoutInfo<A> {
    /// The minimum alignment for the value.
    ///
    /// Necessary so that [`Weak`] can have a reserved value.
    pub const MIN_VALUE_ALIGNMENT: usize = 2;
    #[inline]
    pub fn new(layout: Layout) -> Result<Self, core::alloc::LayoutError> {
        let (full_layout, value_offset) =
            Layout::new::<BrcHeader<A>>().extend(layout.align_to(Self::MIN_VALUE_ALIGNMENT)?)?;
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
        const {
            assert!(core::mem::offset_of!(BrcHeader<A>, rc) == 0);
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
    /// Mirrors [`std::sync::Arc::is_unique`].
    ///
    /// Due to the nature of biased reference counting,
    /// this may have false-negatives when called on a non-biased thread.
    /// However, it will never have false positives.
    /// See [`Self::strong_count`] for details.
    #[inline]
    pub fn is_unique(this: &Self) -> bool {
        if this.header().rc.is_definitely_not_unique() {
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
            let actually_unique = this.header().rc.is_unique();
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

    /// Clone this reference count by incrementing the shared count.
    ///
    /// This function works the same regardless of the thread its called on.
    /// Even if this is the biased thread, this still increments the shared count.
    #[inline]
    pub fn clone_shared(this: &Self) -> Self {
        this.header().rc.increment_strong_shared();
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
            unsafe { RawBrcHeader::decrement_strong(&raw const (*header_ptr).rc, context) };
        if result.should_drop {
            // SAFETY: We trust the drop function to return a valid result
            unsafe {
                context.dealloc(NonNull::new_unchecked(&raw mut (*header_ptr).rc));
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
macro_rules! drop_may_dangle {
    (unsafe impl<#[may_dangle] $primary:ident: ?Sized + $primary_bound:ident, $alloc:ident: $alloc_bound:ident> Drop for $target:path {
        $($inner:tt)*

    }) => {
        #[cfg(feature = "nightly-may-dangle")]
        // SAFETY: Guaranteed by caller
        unsafe impl<#[may_dangle] $primary: ?Sized + $primary_bound, $alloc: $alloc_bound> Drop for $target {
            $($inner)*
        }
        #[cfg(not(feature = "nightly-may-dangle"))]
        impl<$primary: ?Sized + $primary_bound, $alloc: $alloc_bound> Drop for $target {
            $($inner)*
        }
    };
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
/// A [`Brc`] is just a [`NonNull`] pointer, so `Option<Brc>` can be safely zero-initialized.
///
/// This requires `T: Sized` because not all pointer metadata is safe to zero-initialize.
// SAFETY: We only wrap a NonNull (the allocator is stored in the header)
unsafe impl<T, A: Allocator> bytemuck::ZeroableInOption for Brc<T, A> {}

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
        let value: *mut T = ptr_meta::from_raw_parts_mut(
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
        // SAFETY: Valid since T has not been dropped yet.
        // However, this violates stacked borrow. It works fine for tree borrows)
        let layout = unsafe { Layout::for_value(&*value) };
        // SAFETY: Know the layout will not overflow since already allocated
        let layout_info = unsafe { LayoutInfo::<A>::new(layout).unwrap_unchecked() };
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

/// A type that can be used in a [`Brc`].
///
/// Currently, this includes all sized objects, slices, and
/// trait objects that the [`ptr_meta`] crate supports.
/// However, due to implementation limitations, it may not include all types the future
/// (in particular, extern types can never be meaningfully supported).
///
/// # Safety
/// Effectively sealed, so all implementations can be trusted.
#[doc(hidden)]
pub trait SupportedPointee: SupportedPointeeInternal {}

/// A type that can be used in a [`Weak`]
///
/// This is more not implemented for as many types as [`SupportedWeakPointee`],
/// as it is more difficult to polyfill the functionality that [`Weak`] needs.
#[doc(hidden)]
pub trait SupportedWeakPointee: SupportedPointee + SupportedWeakPointeeInternal {}

/// A [`Weak`] which is not the dummy reserved value, and so is "real."
#[derive(bytemuck::TransparentWrapper)]
#[repr(transparent)]
struct WeakReal<T: ?Sized + SupportedWeakPointee, A: Allocator>(Weak<T, A>);
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> WeakReal<T, A> {
    #[inline]
    fn value_ptr(&self) -> NonNull<T> {
        self.0.value_ptr_or_reserved
    }

    #[inline]
    fn value_layout(&self) -> Layout {
        // SAFETY: Value pointer used to be valid,
        // so we can call Layout::for_Value
        unsafe { T::layout_for_ptr(self.value_ptr().as_ptr()) }
    }

    #[inline]
    fn layout_info(&self) -> LayoutInfo<A> {
        // SAFETY: Since allocation was successful,
        // we know that this layout calculation cannot fail
        unsafe { LayoutInfo::new(self.value_layout()).unwrap_unchecked() }
    }

    #[inline]
    fn header_ptr(&self) -> NonNull<BrcHeader<A>> {
        let layout_info = self.layout_info();
        // SAFETY: We trust the `header_offset` from the layout info to be valid
        unsafe {
            self.value_ptr()
                .byte_offset(layout_info.header_offset())
                .cast::<BrcHeader<A>>()
        }
    }

    #[inline]
    fn header(&self) -> &BrcHeader<A> {
        // SAFETY: We trust the returned pointer to live for &self
        unsafe { self.header_ptr().as_ref() }
    }
}

/// A weak-reference to a [`Brc`].
///
/// Mirrors [`std::sync::Weak`].
#[repr(transparent)]
pub struct Weak<T: ?Sized + SupportedWeakPointee, A: Allocator = Global> {
    /// Either a pointer to where the value used to be,
    /// or [`usize::MAX`] if returned from [`Weak::new`].
    ///
    /// This value can never be held by a valid pointer since [`LayoutInfo::MIN_VALUE_ALIGNMENT`]
    /// is greater than one.
    ///
    /// If the value is reserved, it must use the [`Global`] allocator.
    /// This is necessary for [`Weak::allocator`] to work correctly.
    value_ptr_or_reserved: NonNull<T>,
    alloc_marker: PhantomData<A>,
}
impl<T> Default for Weak<T> {
    #[inline]
    fn default() -> Self {
        Weak::new()
    }
}
impl<T> Weak<T> {
    /// A [`Weak`] instance that points to nothing,
    /// and can never be upgraded.
    ///
    /// Mirrors [`std::sync::Weak::new`].
    #[inline]
    pub const fn new() -> Self {
        const {
            assert!(LayoutInfo::<Global>::MIN_VALUE_ALIGNMENT >= 2);
        }
        Weak {
            value_ptr_or_reserved: NonNull::without_provenance(NonZeroUsize::MAX),
            alloc_marker: PhantomData,
        }
    }
}

impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Weak<T, A> {
    /// The allocator that the weak reference originated from.
    ///
    /// Mirrors [`Weak::allocator`].
    #[inline]
    pub fn allocator(&self) -> &A {
        match self.real() {
            Some(real) => &real.header().alloc,
            None => {
                const GLOBAL_REF: &Global = &Global;
                // SAFETY: The reserved value must use the global allocator
                unsafe { &*core::ptr::from_ref(GLOBAL_REF).cast::<A>() }
            }
        }
    }

    /// Return a [`WeakReal`] corresponding to an actual allocation,
    /// or `None` if this is the reserved value from [`Self::new`].
    #[inline]
    fn real(&self) -> Option<&'_ WeakReal<T, A>> {
        if self.value_ptr_or_reserved.addr() != NonZeroUsize::MAX {
            debug_assert!(
                self.value_ptr_or_reserved.addr().get().is_multiple_of(2),
                "UB: non-reserved pointer inappropriately aligned"
            );
            // we know its not reserved
            Some(bytemuck::TransparentWrapper::wrap_ref(self))
        } else {
            None
        }
    }

    /// Upgrade to an owned reference,
    /// returning `None` if the memory has been freed.
    ///
    /// Mirrors [`std::sync::Weak::upgrade`].
    #[inline]
    pub fn upgrade(&self) -> Option<Brc<T, A>> {
        let this = self.real()?;
        let value_ptr = this.value_ptr();
        let header = this.header();
        match header.rc.increment_strong_unless_zero() {
            Ok(()) => {
                // SAFETY: Success of increment_strong means we have an owned references
                unsafe { Some(Brc::from_raw(value_ptr.as_ptr())) }
            }
            Err(runtime::ZeroReferenceCountError) => None,
        }
    }

    /// Recreate a weak references from a raw pointer.
    ///
    /// Consumes ownership of a weak reference.
    ///
    /// # Safety
    /// Pointer must have originally come from [`Brc::into_raw`],
    /// and correspond to an owned weak reference.
    ///
    /// Mirrors the requirements of [`Brc::from_raw`].
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        Weak {
            // SAFETY: Caller guarantees the pointer is valid ,
            // and into_raw never returns null
            value_ptr_or_reserved: unsafe { NonNull::new_unchecked(ptr.cast_mut()) },
            alloc_marker: PhantomData,
        }
    }

    /// Convert a weak reference into a raw pointer.
    #[inline]
    pub fn into_raw(self) -> *const T {
        let this = ManuallyDrop::new(self);
        this.as_ptr()
    }

    /// Get a raw pointer to the underlying object.
    ///
    /// The object is valid only if there are still strong references to the value.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.value_ptr_or_reserved.as_ptr()
    }
}
drop_may_dangle! {
// SAFETY: We respect the may_dangle requirements
unsafe impl<#[may_dangle] T: ?Sized + SupportedWeakPointee, A: Allocator> Drop for Weak<T, A> {
    #[inline]
    fn drop(&mut self) {
        if let Some(this) = self.real() {
            // SAFETY: Our existence implies we own a weak reference
            unsafe {
                BrcHeader::drop_weak(this.header_ptr().as_ptr(), this.layout_info());
            }
        }
    }
}
}
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Clone for Weak<T, A> {
    #[inline]
    fn clone(&self) -> Self {
        match self.real() {
            Some(real) => {
                let header = real.header();
                // cannot possibly be locked as justified by std::sync::Weak::clone
                let old_count = header.weak_count.fetch_add(1, Ordering::AcqRel);
                if old_count > WEAK_OVERFLOW_THRESHOLD {
                    runtime::fatal_errors::weak_refcnt_overflow();
                }
                Weak {
                    alloc_marker: PhantomData,
                    value_ptr_or_reserved: self.value_ptr_or_reserved,
                }
            }
            None => Weak {
                alloc_marker: PhantomData,
                value_ptr_or_reserved: self.value_ptr_or_reserved,
            },
        }
    }
}
// We might be able to be more conservative with these bounds,
// but this is what std::sync::Weak does
// SAFETY: We are careful to be thread-safe
unsafe impl<T: ?Sized + SupportedWeakPointee + Send + Sync, A: Allocator + Send + Sync> Send
    for Weak<T, A>
{
}
// SAFETY: We are careful to be thread-safe
unsafe impl<T: ?Sized + SupportedWeakPointee + Send + Sync, A: Allocator + Send + Sync> Sync
    for Weak<T, A>
{
}

impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Debug for Weak<T, A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str("(Weak)")
    }
}

/// The internals of [`SupportedPointee`], using the [`ptr_meta`] crate.
mod pointee {
    use crate::runtime::ErasedDestructorContext;
    use core::alloc::Layout;
    #[cfg(feature = "nightly-ptr-meta")]
    use core::ptr as ptr_meta;
    use ptr_meta::{DynMetadata, Pointee};
    #[cfg(not(feature = "nightly-ptr-meta"))]
    use ptr_meta_stable as ptr_meta;

    /// The sealed internals of [`super::SupportedPointee`], hidden from the public.
    ///
    /// This performs double-duty by ensuring the trait is sealed.
    pub trait SupportedPointeeInternal: Pointee<Metadata: SupportedMetadata> {}
    impl<T: ?Sized + Pointee> SupportedPointeeInternal for T where T::Metadata: SupportedMetadata {}
    impl<T: ?Sized + Pointee> super::SupportedPointee for T where T::Metadata: SupportedMetadata {}

    /// The sealed internals of [`super::SupportedWeakPointee`], hidden from the public.
    ///
    /// This is more restrictive than [`SupportedPointeeInternal`]
    /// due to the need to calculate layout information.
    pub trait SupportedWeakPointeeInternal: SupportedPointeeInternal {
        /// Determine the layout of a value behind the specified pointer.
        ///
        /// # Safety
        /// Same requirements [`Layout::for_value_raw`].
        unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout;
    }

    /// If we are permitted to use the nightly [`Layout::for_value_raw`] method,
    /// than weak references can be made to any `?Sized` type.
    #[cfg(feature = "nightly-ptr-layout")]
    mod nightly_weak_pointee {
        use super::{Layout, Pointee, SupportedMetadata, SupportedWeakPointeeInternal};
        use crate::SupportedWeakPointee;
        impl<T: ?Sized + Pointee> SupportedWeakPointeeInternal for T
        where
            T::Metadata: SupportedMetadata,
        {
            #[inline]
            unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout {
                // SAFETY: Caller promises to uphold the appropriate invariants
                unsafe { Layout::for_value_raw(ptr.cast_const()) }
            }
        }
        impl<T: ?Sized + Pointee> SupportedWeakPointee for T where T::Metadata: SupportedMetadata {}
    }
    /// If we are on stable rust, then weak references can only be made to `Sized` types,
    /// to slices, and to `str` references.
    ///
    /// We currently do not support weak references with `dyn` trait objects on stable rust.
    /// While in theory we could use [`ptr_meta::DynMetadata::layout`],
    /// I ran into trait coherence issues last time I tried to add it.
    #[cfg(not(feature = "nightly-ptr-layout"))]
    mod stable_weak_pointee {
        use super::{Layout, SupportedWeakPointeeInternal};
        use crate::SupportedWeakPointee;
        impl<T> SupportedWeakPointee for T {}
        #[cfg(not(feature = "nightly-ptr-layout"))]
        impl<T> SupportedWeakPointeeInternal for T {
            #[inline]
            unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout {
                let _ = ptr;
                Layout::new::<T>()
            }
        }
        impl<T> SupportedWeakPointeeInternal for [T] {
            #[inline]
            unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout {
                // SAFETY: If we have already been allocated,
                // then the layout cannot overflow
                unsafe { Layout::array::<T>(ptr.len()).unwrap_unchecked() }
            }
        }
        impl<T> SupportedWeakPointee for [T] {}
        impl SupportedWeakPointeeInternal for str {
            #[inline]
            unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout {
                // SAFETY: Caller guarantees pointer is valid
                unsafe { SupportedWeakPointeeInternal::layout_for_ptr(ptr as *mut [u8]) }
            }
        }
        impl SupportedWeakPointee for str {}
    }

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
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}
impl<T: ?Sized + SupportedPointee + Ord, A: Allocator> Ord for Brc<T, A> {
    #[inline]
    fn cmp(&self, other: &Self) -> cmp::Ordering {
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

#[cfg(feature = "nightly-coerce")]
impl<T: ?Sized, U: ?Sized> CoerceUnsized<Weak<U>> for Weak<T>
where
    T: Unsize<U> + SupportedWeakPointee,
    U: SupportedWeakPointee,
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

// SAFETY: Preserves target and provenance in replace_ptr
unsafe impl<T, U: ?Sized + SupportedWeakPointee> unsize::CoerciblePtr<U> for Weak<T> {
    type Pointee = T;
    type Output = Weak<U>;

    #[inline]
    fn as_sized_ptr(&mut self) -> *mut Self::Pointee {
        // Use deref to acquire pointer to self
        // NOTE: Turning this into an &mut T is UB if there is shared ownership
        Weak::as_ptr(&*self).cast_mut()
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
        unsafe { Weak::from_raw(new) }
    }
}
