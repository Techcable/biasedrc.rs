//! Implements conversions to/from [`Brc`].
//!
//! These usually require allocation.

use crate::allocator_api::alloc::Global;
use crate::ptr_meta;
use crate::strong::{MayPanic, NeverPanic};
use crate::{Brc, SupportedPointee};
use core::alloc::Layout;
use core::mem::ManuallyDrop;

impl<T> Brc<[T]> {
    /// Create a new [`Brc`], cloning each element from the specified slice.
    ///
    /// Equivalent to `From<[T]>`, but potentially clearer.
    /// Prefer using [`Brc::copy_from_slice`] wherever possible,
    /// as copying is much faster than cloning.
    ///
    /// # Panics
    /// Will panic if [`T::clone`](Clone::clone) does,
    /// in addition to the cases that [`Self::new`] does.
    #[inline]
    pub fn clone_from_slice(slice: &[T]) -> Self
    where
        T: Clone,
    {
        Self::from(slice)
    }
    /// Create a new [`Brc`], using a `memcpy` of the specified slice
    ///
    /// This is more efficient than [`Self::clone_from_slice`] or `From<&[T]>`,
    /// by taking advantage of the `T: Copy` bound.
    /// Even on nightly, this library avoids specialization as it is an "incomplete feature"
    /// with soundness issues.
    ///
    /// # Panics
    /// This function should only panic in the cases [`Self::new`] does.
    #[inline]
    pub fn copy_from_slice(slice: &[T]) -> Self
    where
        T: Copy,
    {
        let layout = Layout::for_value::<[T]>(slice);
        // SAFETY: We know layout is correct for [T],
        // and the memcpy ensures the result is fully initialized
        unsafe {
            Self::alloc_with_in::<NeverPanic>(
                layout,
                slice.len(),
                |dest| {
                    // SAFETY: This is fine because T: Copy
                    dest.cast::<T>()
                        .copy_from_nonoverlapping(slice.as_ptr(), slice.len());
                },
                Global,
            )
        }
    }
}

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
            Self::alloc_with_in::<NeverPanic>(
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
/// Create a new `Brc<[T]>` by cloning the contents of the specified slice.
///
/// Equivalent to calling [`Brc::clone_from_slice`].
/// Prefer using [`Brc::copy_from_slice`] wherever possible,
/// as copying is much faster than cloning.
impl<T: Clone> From<&[T]> for Brc<[T]> {
    fn from(src: &[T]) -> Self {
        let layout = Layout::for_value(src);
        // SAFETY: We trust the slice iterator + cloned() to have correct length or panic
        // It would be nice if we could ask `is_copy::<T>`,
        // but we unfortunately cannot without specialization.
        unsafe { Self::from_iter_exact_trusted_in::<MayPanic>(layout, src.iter().cloned(), Global) }
    }
}
/// Implement something like `From<Vec<T>>`,
/// directly consuming ownership of the elements without needing [`Clone`].
///
/// # Safety (soundness)
/// Must implement `take_ownership` correctly:
/// After completion, the source must no longer drop the elements.
///
/// # Correctness (but not soundness)
/// Taking ownership should not consume the source allocation, only the source elements.
/// The `take_ownership` function should not panic.
macro_rules! from_vec_like {
    (unsafe impl<$element:ident $(, const $cap:ident: usize)?> From<$src_ty:ty> for Brc {
        fn take_ownership($src:ident) -> $ownership_taken:ty $take_ownership:block
    }) => {
        impl<$element $(, const $cap: usize)?> From<$src_ty> for crate::Brc<[$element]> {
            fn from(src: $src_ty) -> Self {
                use crate::allocator_api::alloc::{Layout, Global};
                // SAFETY: We either transfer ownership from the source (on success) or drop it (on panic)
                // The closure fully initializes the result once it is called
                unsafe {
                    crate::Brc::<[$element]>::alloc_with_in::<crate::strong::NeverPanic>(
                        Layout::for_value::<[T]>(src.as_slice()),
                        src.len(),
                        |dest| {
                            #[allow(unused_mut, reason = "macro flexibility")]
                            let mut $src: $src_ty = src;
                            let (src_ptr, src_len): (*mut $element, usize) = ($src.as_mut_ptr(), $src.len());
                            // Nothing past here should panic
                            let _transferred: $ownership_taken = $take_ownership;
                            dest.cast::<$element>().copy_from_nonoverlapping(src_ptr, src_len);
                        },
                        Global,
                    )
                }
            }
        }
    };
}
from_vec_like! {
    // SAFETY: Fully transfers ownership using Vec::set_len
    unsafe impl<T> From<Vec<T>> for Brc {
        fn take_ownership(src) -> () {
            src.set_len(0);
        }
    }
}
from_vec_like! {
    // SAFETY: Fully transfers ownership using ManuallyDrop::new
    unsafe impl<T, const N: usize> From<[T; N]> for Brc {
        fn take_ownership(src) -> ManuallyDrop<[T; N]> {
            ManuallyDrop::new(src)
        }
    }
}
#[cfg(feature = "arrayvec")]
mod arrayvec {
    use crate::Brc;

    from_vec_like! {
        // SAFETY: Fully transfers ownership using ArrayVec::set_len
        unsafe impl<T, const CAP: usize> From<arrayvec::ArrayVec<T, CAP>> for Brc {
            fn take_ownership(src) -> () {
                arrayvec::ArrayVec::set_len(&mut src, 0);
            }
        }
    }
    impl<const CAP: usize> From<arrayvec::ArrayString<CAP>> for Brc<str> {
        #[inline]
        fn from(src: arrayvec::ArrayString<CAP>) -> Brc<str> {
            src.as_str().into()
        }
    }
}
impl From<&str> for Brc<str> {
    #[inline]
    fn from(value: &str) -> Self {
        let bytes = Brc::<[u8]>::copy_from_slice(value.as_bytes());
        // SAFETY: A str has the same repr as [u8], and we know the UTF8 is valid
        unsafe { Brc::from_raw(Brc::into_raw(bytes) as *mut str) }
    }
}
impl<T> FromIterator<T> for Brc<[T]> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, upper) = iter.size_hint();
        if Some(lower) == upper {
            let len = lower;
            let layout = Layout::array::<T>(len).expect("Layout overflow");
            // SAFETY: The AssertExactIter verifies the length is correct
            // The Layout is correct
            unsafe {
                Self::from_iter_exact_trusted_in::<MayPanic>(
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

/// Verifies that the iterator has exactly the claimed length,
/// panicking if it yields more or fewer elements.
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
