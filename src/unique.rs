use crate::allocator_api::alloc::{Allocator, Global};
use crate::layout::{BrcHeader, LayoutInfo, WEAK_LOCKED_COUNT, WEAK_OVERFLOW_THRESHOLD};
use crate::weak::WeakDropGuard;
use crate::{Brc, SupportedPointee, SupportedWeakPointee, UniqueUninitBrc, Weak};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

/// A [`Brc`] that is statically known to have no other strong references.
///
/// This means it can be safely mutated just like a [`Box`].
///
/// It is possible to create weak pointers to the object using [`UniqueBrc::downgrade`].
/// However, these will fail to upgrade until [`UniqueBrc::into_shared`] is called.
///
/// # Safety
/// It is *not* valid to convert between this type and [`crate::Brc`].
///
/// They have different invariants and the mismatched state will trigger undefined behavior.
pub struct UniqueBrc<T: ?Sized + SupportedPointee, A: Allocator = Global> {
    ptr: NonNull<T>,
    value_marker: PhantomData<T>,
    alloc_marker: PhantomData<A>,
}
impl<T: ?Sized + SupportedPointee, A: Allocator> UniqueBrc<T, A> {
    /// Retake ownership of a pointer created with [`UniqueBrc::from_raw`].
    ///
    /// Cannot be used with the result of [`Brc::into_raw`],
    /// even if the [`Brc`] is known to be unique.
    /// For this reason, the function is crate-private.
    ///
    /// # Safety
    /// Must have originated from [`UniqueBrc::into_raw`].
    ///
    /// It is undefined behavior to call this on the result of [`Brc::from_raw`],
    /// even if the [`Brc`] is known to be unique.
    #[inline]
    pub(crate) unsafe fn from_raw(ptr: *mut T) -> Self {
        UniqueBrc {
            // SAFETY: Pointer is valid and so not null
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            value_marker: PhantomData,
            alloc_marker: PhantomData,
        }
    }

    /// Give up ownership of this value,
    /// allowing use with [`Self::from_raw`] later.
    ///
    /// The result of this function cannot be used with [`Brc::into_raw`].
    /// For this reason, the function is crate-private.
    ///
    /// # Safety
    /// Calling this function is fully safe,
    /// but the result must not be used with [`Brc::from_raw`].
    #[allow(
        clippy::wrong_self_convention,
        reason = "inherent impl could conflict with deref"
    )]
    #[inline]
    #[expect(dead_code, reason = "not yet publicly exposed (for safety reasons)")]
    pub(crate) fn into_raw(this: Self) -> *mut T {
        let this = ManuallyDrop::new(this);
        this.ptr.as_ptr()
    }

    #[inline]
    fn layout_info(&self) -> LayoutInfo<A> {
        // SAFETY: Cannot overflow since the value has already been allocated
        unsafe { LayoutInfo::for_value::<T>(&**self) }
    }

    #[inline]
    fn header(&self) -> &BrcHeader<A> {
        let layout = self.layout_info();
        // SAFETY: The header pointer is valid
        unsafe { Brc::<T, A>::header_ptr_for(self.ptr, layout).as_ref() }
    }

    /// Convert this into a shared reference,
    /// giving up knowledge of uniqueness.
    ///
    /// After this is called,
    /// weak pointers previously created through [`Self::downgrade`] can be upgraded.
    #[inline]
    #[allow(clippy::wrong_self_convention, reason = "would conflict with deref")]
    pub fn into_shared(this: Self) -> Brc<T, A> {
        let this = ManuallyDrop::new(this);
        // SAFETY: We no longer care about uniqueness,
        // so it is safe to initialize the strong count
        //
        // This uses a release store just like std::sync::UniqueArc::into_arc does
        // We cannot use a relaxed update like Arc::clone,
        // because we are change the strong count from zero -> one,
        // instead of from nonzero -> nonzero + 1
        unsafe { this.header().strong.init(Ordering::Release) }
        // SAFETY: Calling `strong.init` creates a strong reference
        unsafe { Brc::from_raw(this.ptr.as_ptr()) }
    }

    /// Create a weak pointer to this object.
    ///
    /// The weak pointer will not be able to be upgraded until uniqueness is given up
    /// by calling [`UniqueBrc::into_shared`].
    #[inline]
    #[must_use]
    pub fn downgrade(this: &Self) -> Weak<T, A>
    where
        T: SupportedWeakPointee,
    {
        let layout_info = this.layout_info();
        // SAFETY: The header pointer is valid
        let header_ptr = unsafe { Brc::<T, A>::header_ptr_for(this.ptr, layout_info) };
        // SAFETY: We know we are uniquely owned and the strong count is uninit
        unsafe { Self::downgrade_for_unique_ptr(header_ptr, this.ptr) }
    }

    /// Create a new weak reference for a uniquely held reference ([`crate::UniqueBrc`]),
    /// whose strong count has not yet been initialized.
    ///
    /// Cannot be upgraded till strong count is initialized,
    /// which can only happen once uniqueness is given up.
    ///
    /// Used to implement both [`UniqueBrc::downgrade`] and [`crate::UniqueUninitBrc::downgrade`].
    ///
    /// # Safety
    /// Undefined behavior if object is not uniquely referenced,
    /// or if the strong count has not been initialized.
    ///
    /// Undefined behavior if either the header pointer or value pointer is not valid.
    ///
    /// Undefined behavior if the type of `T` does not match the allocation.
    /// The value of `T` may be uninitialized,
    #[must_use]
    #[inline]
    pub(crate) unsafe fn downgrade_for_unique_ptr(
        header_ptr: NonNull<BrcHeader<A>>,
        value_ptr: NonNull<T>,
    ) -> Weak<T, A>
    where
        A: Allocator,
        T: SupportedWeakPointee,
    {
        // SAFETY: Caller guarantees header is valid at this point
        let header = unsafe { header_ptr.as_ref() };
        if cfg!(debug_assertions) && !header.strong.is_strong_uninit(Ordering::Relaxed) {
            crate::runtime::undefined_behavior::unique_nonzero_strong();
        }
        // See std::sync::ArcUnique::downgrade for justification on the relaxed count
        // To quote "knowledge of the original reference count prevents
        //
        // Still need atomicity in case the `&UniqueBrc` is shared across threads
        let old_count = header.weak_count.fetch_add(1, Ordering::Relaxed);
        if cfg!(debug_assertions) && old_count == WEAK_LOCKED_COUNT {
            crate::runtime::undefined_behavior::unique_weak_locked();
        }
        if old_count > WEAK_OVERFLOW_THRESHOLD {
            crate::runtime::fatal_errors::weak_refcnt_overflow();
        }
        // SAFETY: Just incremented reference count.
        // The caller guarantees the pointer is both valid and of type `T`.
        // Will not be able to upgrade until strong count is initialized,
        // meaning that sharing this weak pointer will not violate uniqueness
        unsafe { Weak::from_raw(value_ptr.as_ptr().cast_const()) }
    }
}
impl<T: Default> Default for UniqueBrc<T> {
    fn default() -> Self {
        Self::new_with(T::default)
    }
}
impl<T, A: Allocator> UniqueBrc<T, A> {
    /// Allocate memory for a [`UniqueBrc`] holding the specified contents,
    /// and using the specified custom allocator..
    ///
    /// Mirrors [`Brc::new_in`].
    #[inline]
    pub fn new_in(value: T, alloc: A) -> Self {
        let uninit = UniqueUninitBrc::new_in(alloc);
        uninit.write(value)
    }

    /// Allocate memory for a [`UniqueBrc`] using the specified allocator,
    /// using a closure to initialize its contents.
    ///
    /// Mirrors [`Brc::new_with_in`].
    #[inline]
    pub fn new_with_in(func: impl FnOnce() -> T, alloc: A) -> Self {
        let uninit = UniqueUninitBrc::new_in(alloc);
        uninit.write(func())
    }
}
impl<T> UniqueBrc<T> {
    /// Create a [`UniqueBrc`] with the specified value.
    ///
    /// Mirrors [`Brc::new`].
    #[inline]
    pub fn new(value: T) -> Self {
        Self::new_in(value, Global)
    }

    /// Allocate a [`UniqueBrc`], using the specified closure to initialize its contents.
    ///
    /// Mirrors [`Brc::new_with`].
    #[inline]
    pub fn new_with(func: impl FnOnce() -> T) -> Self {
        let uninit = UniqueUninitBrc::new();
        uninit.write(func())
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> Deref for UniqueBrc<T, A> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: Pointer is valid
        unsafe { self.ptr.as_ref() }
    }
}
impl<T: ?Sized + SupportedPointee, A: Allocator> DerefMut for UniqueBrc<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Known to have unique access
        unsafe { self.ptr.as_mut() }
    }
}
drop_may_dangle! {
    // SAFETY: Doesn't access T outside of calling T::drop
    unsafe impl<#[may_dangle] T: ?Sized + SupportedPointee, A: Allocator> Drop for UniqueBrc<T, A> {
        fn drop(&mut self) {
            let layout_info = self.layout_info();
            // SAFETY: Trust allocation is in bounds and layout is accurate
            let header_ptr = unsafe { Brc::header_ptr_for(
                self.ptr,
                layout_info
            ) };
            if cfg!(debug_assertions) {
                // SAFETY: Header pointer is still valid at this point
                let header = unsafe { header_ptr.as_ref() };
                // Sanity check that strong count is truly uninitialized
                if !header.strong.is_strong_uninit(Ordering::Relaxed) {
                    crate::runtime::undefined_behavior::unique_nonzero_strong();
                }
            }
            // SAFETY: Corresponds to the weak reference that we own
            let weak = unsafe {
                WeakDropGuard::new(header_ptr, layout_info)
            };
            // SAFETY: Valid because we are the unique owner.
            unsafe { core::ptr::drop_in_place(self.ptr.as_ptr()) };
            drop(weak);
        }
    }
}
