use crate::allocator_api;
use crate::allocator_api::alloc::{Allocator, Global};
use crate::layout::{BrcHeader, LayoutInfo};
use crate::unique::UniqueBrc;
use crate::{Brc, SupportedWeakPointee, Weak};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

/// A [`UniqueBrc`] whose contents may not have been initialized.
///
/// Even before the value is fully initialized,
/// weak references can be safely created with [`Self::downgrade`].
/// Just like with [`UniqueBrc`] these references will fail to upgrade
/// until the value is fully uninitialized.
///
/// This type cannot be directly converted into a [`Brc`].
/// It must first be converted into a [`UniqueBrc`] via [`Self::assume_init`]
/// (or some other init method like [`Self::write`]).
///
/// Dropping this struct will free the allocated memory, but will not drop the contents of `T`.
pub struct UniqueUninitBrc<T: ?Sized + SupportedWeakPointee, A: Allocator = Global> {
    ptr: NonNull<T>,
    alloc_marker: PhantomData<A>,
}
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> UniqueUninitBrc<T, A> {
    /// Get a pointer to the contained value.
    ///
    /// # Safety
    /// Must not be used with [`Brc::from_raw`],
    /// as it doesn't have the expected state.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.ptr.as_ptr().cast_const()
    }

    /// Get the layout information.
    #[inline]
    fn layout_info(&self) -> LayoutInfo<A> {
        // SAFETY: Pointer refers to valid Brc allocation.
        // It is not necessarily initialized so `for_value` would be unsafe
        unsafe { LayoutInfo::for_value_raw(self.as_ptr()) }
    }

    /// Get a mutable pointer to the contained value.
    ///
    /// # Safety
    /// Must not be used with [`Brc::from_raw`],
    /// as it doesn't have the expected state.
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Get a reference to the possibly initialized value.
    ///
    /// # Safety
    /// This is fully safe due to wrapping in a [`MaybeUninit`].
    #[inline]
    pub fn as_uninit_ref(&self) -> &MaybeUninit<T>
    where
        T: Sized,
    {
        // SAFETY: Memory is valid, other than maybe being uninitialized
        unsafe { &*self.as_ptr().cast::<MaybeUninit<T>>() }
    }
    /// Get a mutable reference to the possibly initialized value.
    ///
    /// # Safety
    /// This is fully safe due to wrapping in a [`MaybeUninit`].
    #[inline]
    pub fn as_uninit_mut(&self) -> &MaybeUninit<T>
    where
        T: Sized,
    {
        // SAFETY: Memory is valid, other than maybe being uninitialized
        unsafe { &*self.as_ptr().cast::<MaybeUninit<T>>() }
    }

    /// Assume the value is fully initialized,
    /// converting it into a [`UniqueBrc`].
    ///
    /// # Safety
    /// Undefined behavior if the value is not fully initialized,
    /// just like with [`MaybeUninit::assume_init`].
    #[inline]
    pub unsafe fn assume_init(self) -> UniqueBrc<T, A> {
        let this = ManuallyDrop::new(self);
        // SAFETY: We meet the special requirements
        unsafe { UniqueBrc::from_raw(this.ptr.as_ptr()) }
    }

    /// Assume the value is fully initialized,
    /// returning a reference to the value.
    ///
    /// # Safety
    /// Undefined behavior if the value is not fully initialized,
    /// just like with [`MaybeUninit::assume_init_ref`].
    #[inline]
    pub unsafe fn assume_init_ref(self) -> UniqueBrc<T, A> {
        let this = ManuallyDrop::new(self);
        // SAFETY: We meet the special requirements
        unsafe { UniqueBrc::from_raw(this.ptr.as_ptr()) }
    }

    /// Assume the value is fully initialized,
    /// returning a mutable reference to the value.
    ///
    /// This takes advantage of the fact that [`Self`] is unique.
    ///
    /// # Safety
    /// Undefined behavior if the value is not fully initialized,
    /// just like with [`MaybeUninit::assume_init_ref`].
    #[inline]
    pub unsafe fn assume_init_mut(&mut self) -> UniqueBrc<T, A> {
        let this = ManuallyDrop::new(self);
        // SAFETY: We meet the special requirements
        unsafe { UniqueBrc::from_raw(this.ptr.as_ptr()) }
    }

    /// Run a closure with a `&Weak` reference created by [`Self::downgrade`].
    ///
    /// If the closure doesn't clone the  `&Weak` reference,
    /// this avoids modifying any reference counts and so is faster than [`Self::downgrade`].
    /// Prefer calling [`Self::downgrade`] directly whenever you want a weak reference,
    /// as it can take advantage of uniqueness to be
    /// (slightly) more efficient than [`Weak::clone`].
    ///
    /// This function is mainly included so that [`Brc::new_cyclic`]
    /// can be implemented using entirely public APIs,
    /// while keeping the same performance characteristics as [`std::sync::Arc::new_cyclic`].
    pub fn with_weak_ref<R>(&self, func: impl FnOnce(&Weak<T, A>) -> R) -> R {
        // SAFETY: We know that `self` owns a weak reference,
        // and ManuallyDrop ensures it is never actually dropped,
        let weak = unsafe { ManuallyDrop::new(Weak::from_raw(self.ptr.as_ptr())) };
        func(&*weak)
    }

    /// Drops the contained value in place.
    ///
    /// This does not drop the allocated memory.
    /// That is handled by the destructor of [`UniqueUninitBrc`].
    ///
    /// Mirrors [`MaybeUninit::assume_init_drop`].
    ///
    /// # Safety
    /// Assumes that the type is fully initialized and valid.
    ///
    /// All the other requirements of [`core::ptr::drop_in_place`] apply.
    /// In particular, you must not double drop the underlying value.
    #[inline]
    pub unsafe fn assume_init_drop(&mut self) {
        // SAFETY: Responsibility of the caller
        unsafe { core::ptr::drop_in_place(self.as_mut_ptr()) }
    }

    /// Create a weak pointer to this object.
    ///
    /// The weak pointer will not be able to be upgraded until the object is both fully initialized
    /// and uniqueness is given up by calling [`UniqueBrc::into_shared`].
    #[inline]
    #[must_use]
    pub fn downgrade(this: &Self) -> Weak<T, A> {
        let layout_info = this.layout_info();
        // SAFETY: The header pointer is valid
        let header_ptr = unsafe { Brc::<T, A>::header_ptr_for(this.ptr, layout_info) };
        // SAFETY: We know we are uniquely owned and the strong count is uninit
        unsafe { UniqueBrc::downgrade_for_unique_ptr(header_ptr, this.ptr) }
    }
}
impl<T, A: Allocator> UniqueUninitBrc<T, A> {
    /// Allocate memory for a [`UniqueBrc`] using the specified allocator,
    /// without initializing its contents.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        let layout_info = LayoutInfo::new_or_panic(Layout::new::<T>());
        let Ok(weak_guard) = crate::layout::begin_unique_alloc_in(layout_info, alloc) else {
            allocator_api::alloc::handle_alloc_error(layout_info.full_layout())
        };
        let value_ptr = weak_guard.value_ptr().cast::<T>();
        core::mem::forget(weak_guard);
        UniqueUninitBrc {
            ptr: value_ptr,
            alloc_marker: PhantomData,
        }
    }

    /// Initialize the specified value,
    /// returning a [`UniqueBrc`].
    #[inline]
    pub fn write(self, value: T) -> UniqueBrc<T, A> {
        // SAFETY: Pointer is known to have space for a T
        unsafe { self.as_mut_ptr().write(value) };
        // SAFETY: We are initialized because we just wrote to memory
        unsafe { self.assume_init() }
    }
}
impl<T> Default for UniqueUninitBrc<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> UniqueUninitBrc<T> {
    /// Allocate memory for a [`UniqueBrc`]
    /// without initializing its contents.
    #[inline]
    pub fn new() -> Self {
        Self::new_in(Global)
    }
}
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> UniqueUninitBrc<T, A> {}
drop_may_dangle! {
    // SAFETY: Don't access T at all
    unsafe impl<#[may_dangle] T: ?Sized + SupportedWeakPointee, A: Allocator> Drop for UniqueUninitBrc<T, A> {
        fn drop(&mut self) {
            let layout_info = self.layout_info();
            // SAFETY: Trust allocation is in bounds and layout is accurate
            let header_ptr = unsafe { Brc::header_ptr_for(
                self.ptr,
                layout_info
            ) };
            if cfg!(debug_assertions) {
                // SAFETY: The header is still valid at this point
                let header = unsafe { header_ptr.as_ref() };
                // Sanity check that our strong count is still uninitialized
                if header.strong.is_strong_uninit(Ordering::Relaxed) {
                    crate::runtime::undefined_behavior::unique_nonzero_strong();
                }
            }
            // SAFETY: Corresponds to the weak reference that we own
            unsafe {
                BrcHeader::drop_weak(
                    header_ptr.as_ptr(),
                    layout_info
                );
            }
        }
    }
}
