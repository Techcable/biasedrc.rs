//! An implementation of [biased reference counting] for Rust.
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195

extern crate core;

use std::alloc::Layout;
use std::marker::PhantomData;
use std::ptr::NonNull;

mod raw;

use crate::raw::RawBrcHeader;

pub struct Brc<T: ?Sized> {
    ptr: NonNull<T>,
    marker: PhantomData<T>,
}
impl<T: ?Sized> Brc<T> {
    pub fn
    fn header(&self) -> *const {
        // SAFETY: Assume that reference is valid
        let align = unsafe {core::mem::align_of_val(self.ptr.as_ref());
        }

        unsafe {
            self.ptr.as_ptr().cast::<u8>()
                .sub(offset_of!(BrcInner::<T>, value))
                .cast::<RawBiasedHeader>()
        }
    }
}
impl Drop for Brc<T> {
    fn drop(&mut self) {

    }
}
