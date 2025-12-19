use crate::Brc;
use core::ffi::c_void;
use core::fmt::{Debug, Formatter};
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

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
