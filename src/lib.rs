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
impl Drop for Brc<T> {
    fn drop(&mut self) {}
}
