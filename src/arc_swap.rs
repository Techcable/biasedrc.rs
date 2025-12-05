//! Support for the [`arc_swap`] crate.

use crate::Brc;

// SAFETY: Implemented correctly
unsafe impl<T> arc_swap::RefCnt for Brc<T> {
    type Base = T;
    #[inline]
    fn into_ptr(me: Self) -> *mut T {
        Brc::into_raw(me).cast_mut()
    }
    #[inline]
    fn as_ptr(me: &Self) -> *mut T {
        core::ptr::from_ref::<T>(&**me).cast_mut()
    }
    #[inline]
    unsafe fn from_ptr(ptr: *const Self::Base) -> Self {
        // SAFETY: Validity guaranteed by the caller
        unsafe { Brc::from_raw(ptr) }
    }
    #[inline]
    fn inc(me: &Self) -> *mut Self::Base {
        Brc::into_raw(Brc::clone(me)).cast_mut()
    }
    #[inline]
    unsafe fn dec(ptr: *const Self::Base) {
        // SAFETY: Caller guarantees this is valid
        unsafe { Brc::decrement_strong_count(ptr) }
    }
}
