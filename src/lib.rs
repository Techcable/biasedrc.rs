//! An implementation of [biased reference counting] for Rust.
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195

use std::alloc::Layout;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use ptr_meta::{DynMetadata, Pointee};

mod raw;

use crate::raw::{DestructorFunc, RawBrcHeader};

fn header_offset<T>() -> Result<usize, std::alloc::LayoutError> {
    Ok(Layout::new::<RawBrcHeader>().extend(Layout::new::<T>())?.1)
}

pub struct Brc<T> {
    ptr: NonNull<T>,
    marker: PhantomData<T>,
}
impl<T> Deref for Brc<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Object lives at least as long as we do
        unsafe { self.ptr.as_ref() }
    }
}
impl<T: ?Sized + SupportedPointerTarget> Drop for Brc<T> {
    fn drop(&mut self) {
        #[derive(Copy, Clone)]
        struct DestructorFunction<T> {
            meta: <T as std::ptr::Pointee>::Metadata,
            marker: PhantomData<fn(T)>,
        }
        impl<T> DestructorFunc for DestructorFunction<T> {
            unsafe fn dealloc(ptr: NonNull<RawBrcHeader>) {
                unsafe {
                    std::ptr::drop_in_place(ptr.as_ptr());
                }
            }
        }
        // SAFETY: Our existence means we own a reference count
        unsafe { self.header().decrement_strong::<DestructorFunction<T>>() }
    }
}

