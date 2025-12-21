//! The internals of [`SupportedPointee`], using either [`ptr_meta`](ptr_meta_stable) crate or
//! the unstable `ptr_metadata` feature.

use crate::ptr_meta::{DynMetadata, Pointee};
use crate::runtime::ErasedDestructorContext;
use core::alloc::Layout;

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
/// While in theory we could use [`ptr_meta_stable::DynMetadata::layout`],
/// I ran into trait coherence issues last time I tried to  it.
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
    macro_rules! weak_pointee_str_like {
        ($($target:ty),*) => {
           $(impl SupportedWeakPointeeInternal for $target {
                #[inline]
                unsafe fn layout_for_ptr(ptr: *mut Self) -> Layout {
                    // SAFETY: Caller guarantees pointer is valid
                    unsafe { SupportedWeakPointeeInternal::layout_for_ptr(ptr as *mut [u8]) }
                }
           }
           impl SupportedWeakPointee for $target {})*
        };
    }
    weak_pointee_str_like!(str, std::ffi::OsStr, core::ffi::CStr);
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
