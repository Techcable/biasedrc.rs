//! An implementation of [biased reference counting] for Rust.
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195
#![cfg_attr(feature = "nightly-ptr-meta", feature(ptr_metadata))]

#[cfg(feature = "nightly-ptr-meta")]
use core::ptr as ptr_meta;
#[cfg(not(feature = "nightly-ptr-meta"))]
use ptr_meta_stable as ptr_meta;

use pointee::{SupportedMetadata, SupportedPointeeInternal};
use ptr_meta::Pointee;
use stable_deref_trait::{CloneStableDeref, StableDeref};
use std::alloc::Layout;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr::NonNull;

mod runtime;

use crate::runtime::{DropInfo, ErasedDestructorContext, RawBrcHeader};

pub use crate::runtime::{collect, collect_force};

struct LayoutInfo {
    value_offset: isize,
    full_layout: Layout,
}
impl LayoutInfo {
    #[inline]
    pub fn new(layout: Layout) -> Result<LayoutInfo, std::alloc::LayoutError> {
        let (full_layout, value_offset) = Layout::new::<RawBrcHeader>().extend(layout)?;
        Ok(LayoutInfo {
            full_layout,
            #[expect(clippy::cast_possible_wrap, reason = "offset fits in isize")]
            value_offset: value_offset as isize,
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
pub struct Brc<T: ?Sized + SupportedPointee> {
    ptr: NonNull<T>,
    marker: PhantomData<T>,
}
impl<T> Brc<T> {
    /// Construct a new [`Brc`] with the specified value.
    #[inline]
    pub fn new(value: T) -> Brc<T> {
        // SAFETY: We fully initialize the newly allocated memory
        unsafe { Self::alloc_with(Layout::new::<T>(), (), |target| target.write(value)) }
    }
}
impl<T> Brc<[T]> {
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
    pub unsafe fn from_iter_exact_trusted(
        layout: Layout,
        mut iter: impl ExactSizeIterator<Item = T>,
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
                            std::ptr::slice_from_raw_parts_mut(self.dest, self.initialized_len);
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
            std::mem::forget(guard); // finished initialization
        };
        // SAFETY: Either fully initializes the memory or panics
        // We trust the iterator to be exact.
        unsafe { Brc::alloc_with(layout, len, do_init) }
    }
}
impl<T: ?Sized + SupportedPointee> Brc<T> {
    /// Initialize the value using the specified callback.
    ///
    /// # Safety
    /// Callback must either fully initialize the memory or panic.
    #[inline(always)] // Inlining means we can potentially eliminate the guard & layout calculation
    unsafe fn alloc_with(layout: Layout, meta: T::Metadata, func: impl FnOnce(*mut T)) -> Brc<T> {
        collect();
        #[cold]
        #[inline(never)]
        fn layout_overflow() -> ! {
            panic!("Layout of Brc would overflow an isize")
        }
        let Ok(layout) = LayoutInfo::new(layout) else {
            layout_overflow()
        };
        struct CleanupGuard {
            ptr: *mut u8,
            layout: Layout,
        }
        impl Drop for CleanupGuard {
            #[inline]
            fn drop(&mut self) {
                // SAFETY: We know the pointer is valid since we just allocated it
                // We are careful to forget the guard if we are successful
                unsafe { std::alloc::dealloc(self.ptr, self.layout) }
            }
        }
        // SAFETY: Know the layout is non-empty, since it includes the header even if T is a ZST
        let allocated = unsafe { std::alloc::alloc(layout.full_layout) };
        if allocated.is_null() {
            std::alloc::handle_alloc_error(layout.full_layout);
        }
        let guard = CleanupGuard {
            ptr: allocated,
            layout: layout.full_layout,
        };
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "allocated with appropriate alignment"
        )]
        // SAFETY: Memory is newly allocated so it is known to be valid
        unsafe {
            allocated.cast::<RawBrcHeader>().write(RawBrcHeader::init());
        }
        // SAFETY: We trust the LayoutInfo to have the correct offset
        let value_ptr_addr = unsafe { allocated.byte_offset(layout.value_offset).cast::<()>() };
        let value_ptr = ptr_meta::from_raw_parts_mut(value_ptr_addr, meta);
        func(value_ptr);
        std::mem::forget(guard);
        // SAFETY: Allocated pointer is valid and never null
        unsafe { Self::from_raw(NonNull::new_unchecked(value_ptr)) }
    }
    /// Create a [`Brc`] from a raw pointer,
    /// similar to [`std::sync::Arc::from_raw`].
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
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Brc<T> {
        Brc {
            ptr,
            marker: PhantomData,
        }
    }

    /// Convert this [`Brc`] into a raw pointer,
    /// similar to [`std::sync::Arc::into_raw`].
    ///
    /// # Safety
    /// This is perfectly safe, but may leak memory.
    ///
    /// # Panics
    /// This function is infallible.
    #[allow(clippy::wrong_self_convention, reason = "could conflict with deref")]
    #[inline]
    pub fn into_raw(this: Self) -> NonNull<T> {
        let value = ManuallyDrop::new(this);
        value.ptr
    }

    #[inline]
    fn layout(&self) -> LayoutInfo {
        let this_layout = Layout::for_value(self.deref());
        // SAFETY: Cannot overflow since the value has already been allocated
        unsafe { LayoutInfo::new(this_layout).unwrap_unchecked() }
    }
    #[inline]
    fn header(&self) -> &RawBrcHeader {
        let header_offset = self.layout().header_offset();
        // SAFETY: A Brc always has a valid header, which can then be dereferenced
        unsafe {
            self.ptr
                .cast::<u8>()
                .offset(header_offset)
                .cast::<RawBrcHeader>()
                .as_ref()
        }
    }
}
impl<T: ?Sized + SupportedPointee> Deref for Brc<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Object lives at least as long as we do
        unsafe { self.ptr.as_ref() }
    }
}
impl<T: ?Sized + SupportedPointee> Drop for Brc<T> {
    #[inline]
    fn drop(&mut self) {
        if !std::thread::panicking() {
            collect();
        }
        let value: &T = self.deref();
        let context = DropContext::<T> {
            metadata: ptr_meta::metadata(value),
            value_offset: self.layout().value_offset,
            marker: PhantomData,
        };
        // SAFETY: We own a reference count and the context is valid
        unsafe { self.header().decrement_strong(context) }
    }
}
impl<T: ?Sized + SupportedPointee> Clone for Brc<T> {
    #[inline]
    fn clone(&self) -> Self {
        collect();
        self.header().increment_strong();
        // SAFETY: Just successfully incremented the refcnt
        unsafe { Brc::from_raw(self.ptr) }
    }
}

struct DropContext<T: ?Sized + SupportedPointee> {
    metadata: <T as Pointee>::Metadata,
    value_offset: isize,
    marker: PhantomData<fn(*mut T)>,
}
impl<T: ?Sized + SupportedPointee> Copy for DropContext<T> {}
impl<T: ?Sized + SupportedPointee> Clone for DropContext<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized + SupportedPointee> DropInfo for DropContext<T> {
    #[inline]
    fn value_offset(&self) -> isize {
        self.value_offset
    }

    #[inline]
    fn erased_context(&self) -> ErasedDestructorContext {
        self.metadata.to_context()
    }

    #[inline]
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
        // SAFETY: Valid since T has not been dropped yet.
        // However, this violates stacked borrow. It works fine for tree borrows)
        let layout = unsafe { Layout::for_value(&*value) };
        if std::mem::needs_drop::<T>() {
            // SAFETY: Caller guarantees this is not invoked until it is valid to drop
            unsafe { core::ptr::drop_in_place(value) }
        }
        // SAFETY: Know the layout will not overflow since already allocated
        let layout_info = unsafe { LayoutInfo::new(layout).unwrap_unchecked() };
        debug_assert_eq!(layout_info.value_offset, value_offset);
        // SAFETY: Caller guarantees it is valid to drop the header too
        unsafe { std::alloc::dealloc(header_ptr.as_ptr().cast(), layout_info.full_layout) };
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
pub trait SupportedPointee: SupportedPointeeInternal {}

/// The internals of [`SupportedPointee`], using the [`ptr_meta`] crate.
mod pointee {
    use crate::runtime::ErasedDestructorContext;
    #[cfg(feature = "nightly-ptr-meta")]
    use core::ptr as ptr_meta;
    use ptr_meta::{DynMetadata, Pointee};
    #[cfg(not(feature = "nightly-ptr-meta"))]
    use ptr_meta_stable as ptr_meta;

    /// The sealed internals of [`SupportedPointee`], hidden from the public.
    ///
    /// This performs double-duty by ensuring the trait is sealed.
    pub trait SupportedPointeeInternal: Pointee<Metadata: SupportedMetadata> {}
    impl<T: ?Sized + Pointee> SupportedPointeeInternal for T where T::Metadata: SupportedMetadata {}
    impl<T: ?Sized + Pointee> super::SupportedPointee for T where T::Metadata: SupportedMetadata {}

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
            unsafe { std::mem::transmute::<DynMetadata<Dyn>, ErasedDestructorContext>(self) }
        }
        #[inline]
        unsafe fn from_context(ctx: ErasedDestructorContext) -> Self {
            // SAFETY: DynMetadata should just be a vtable pointer, which we trust to be valid
            unsafe { std::mem::transmute::<ErasedDestructorContext, DynMetadata<Dyn>>(ctx) }
        }
    }
}

//
// smart-pointer boilerplate
//

impl<T: ?Sized + SupportedPointee + Error> Error for Brc<T> {
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
unsafe impl<T: ?Sized + SupportedPointee> CloneStableDeref for Brc<T> {}
// SAFETY: A Brc is heap allocated so the memory never moves
unsafe impl<T: ?Sized + SupportedPointee> StableDeref for Brc<T> {}
impl<T> From<T> for Brc<T> {
    #[inline]
    fn from(value: T) -> Self {
        Brc::new(value)
    }
}
impl<T: Clone> From<&[T]> for Brc<[T]> {
    fn from(src: &[T]) -> Self {
        let layout = Layout::for_value(src);
        // SAFETY: We trust the slice iterator + cloned() to have correct length
        unsafe { Self::from_iter_exact_trusted(layout, src.iter().cloned()) }
    }
}
impl<T> From<Vec<T>> for Brc<[T]> {
    fn from(value: Vec<T>) -> Self {
        let layout = Layout::for_value(value.as_slice());
        // SAFETY: We trust the Vec iterator to have the correct length
        unsafe { Self::from_iter_exact_trusted(layout, value.into_iter()) }
    }
}
impl From<&str> for Brc<str> {
    #[inline]
    fn from(value: &str) -> Self {
        let bytes = Brc::<[u8]>::from(value.as_bytes());
        // SAFETY: A str has the same repr as [u8], and we know the UTF8 is valid
        unsafe {
            Brc::from_raw(NonNull::new_unchecked(
                Brc::into_raw(bytes).as_ptr() as *mut str
            ))
        }
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
            unsafe { Self::from_iter_exact_trusted(layout, AssertExactIter { len, inner: iter }) }
        } else {
            // need to buffer
            iter.collect::<Vec<T>>().into()
        }
    }
}
impl<T: ?Sized + SupportedPointee> Borrow<T> for Brc<T> {
    #[inline]
    fn borrow(&self) -> &T {
        self.deref()
    }
}
impl<T: ?Sized + SupportedPointee> AsRef<T> for Brc<T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self.deref()
    }
}
impl<T: ?Sized + SupportedPointee + Debug> Debug for Brc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}
impl<T: ?Sized + SupportedPointee + Display> Display for Brc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self.deref(), f)
    }
}
impl<T: ?Sized + SupportedPointee + PartialEq> PartialEq for Brc<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}
impl<T: ?Sized + SupportedPointee + PartialOrd> PartialOrd for Brc<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}
impl<T: ?Sized + SupportedPointee + Ord> Ord for Brc<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.deref().cmp(other.deref())
    }
}
impl<T: ?Sized + SupportedPointee + Hash> Hash for Brc<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}
impl<T: ?Sized + SupportedPointee + Eq> Eq for Brc<T> {}
impl<T: ?Sized + SupportedPointee> Unpin for Brc<T> {}
// SAFETY: We are thread safe if T is
unsafe impl<T: ?Sized + SupportedPointee + Sync> Sync for Brc<T> {}
// SAFETY: We are thread safe if T is
unsafe impl<T: ?Sized + SupportedPointee + Sync> Send for Brc<T> {}
