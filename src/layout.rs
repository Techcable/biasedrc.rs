//! Defines the underlying layout of the [`crate::Brc`] type in-memory.

use crate::Brc;
use crate::allocator_api::alloc::Allocator;
use crate::runtime::RawBrcHeader;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

const _USED_IN_DOCS: () = {
    let _ = Brc::<u32>::new;
};

/// If the weak reference count has
pub const WEAK_LOCKED_COUNT: u32 = u32::MAX;
/// If the weak reference passes this point,
/// it should be considered to have overflown.
pub const WEAK_OVERFLOW_THRESHOLD: u32 = u32::MAX / 2;

/// Combines the reference counts with additional metadata.
///
/// This is `#[repr(C)]` because [`Brc::alloc_with_in`] initializes it field-by-field.
#[repr(C)]
pub struct BrcHeader<A: Allocator> {
    pub strong: RawBrcHeader,
    /// The weak reference count.
    ///
    /// May be [`WEAK_LOCKED_COUNT`] to indicate that it is "locked",
    /// which is necessary to implement [`Brc::get_mut`] and [`Brc::make_mut`].
    pub weak_count: AtomicU32,
    /// This is stored in the header because we cannot otherwise pass a monomorphized `A`
    /// to an [`crate::runtime::ErasedDropInfo`].
    ///
    /// The erased drop info is necessary to add the `Brc` to the merge queue.
    pub alloc: ManuallyDrop<A>,
}
impl<A: Allocator> BrcHeader<A> {
    /// Drop a weak reference associated with the header.
    ///
    /// Requires passing layout information,
    /// as by now the underlying value `T` may have been destroyed
    /// and the [`Layout::for_value_raw`] method is unstable.
    #[inline]
    pub unsafe fn drop_weak(header_ptr: *mut Self, layout_info: LayoutInfo<A>) {
        // SAFETY: Caller guarantees header pointer is valid
        let weak_count = unsafe { &(*header_ptr).weak_count };
        // The reasoning in Arc::drop/Weak::drop justifies why we can weaken this to Release
        // with an acquire fence afterward.
        // The reasoning in Weak::drop explains why we don't need to check if we are locked
        if weak_count.fetch_sub(1, Ordering::Release) == 1 {
            // this is not moved into the cold path because
            // it may be possible to fold into the load/store
            atomic::fence(Ordering::Acquire);
            // SAFETY: We just verified we are the last reference,
            // caller guarantees the other requirements
            unsafe { Self::drop_weak_slow(header_ptr, layout_info.full_layout) }
        }
    }

    #[cold]
    unsafe fn drop_weak_slow(header_ptr: *mut Self, full_layout: Layout) {
        // SAFETY: Will not use the allocator after deallocation
        let alloc = unsafe { Self::take_allocator(header_ptr) };
        // SAFETY: Caller guarantees it is okay to
        unsafe {
            alloc.deallocate(NonNull::new_unchecked(header_ptr.cast::<u8>()), full_layout);
        }
    }

    /// Consume ownership of the allocator.
    ///
    /// # Safety
    /// Undefined behavior if the allocator is ever used again,
    /// just like with [`ManuallyDrop::take`].
    #[inline]
    pub unsafe fn take_allocator(header_ptr: *const Self) -> A {
        // SAFETY: Caller guarantees allocator will not be used again
        unsafe {
            header_ptr
                .byte_add(core::mem::offset_of!(Self, alloc))
                .cast::<A>()
                .read()
        }
    }
}

/// The layout information for a [`BrcHeader`].
pub struct LayoutInfo<A: Allocator> {
    pub value_offset: isize,
    pub full_layout: Layout,
    /// Our calculations depend on the layout of the allocator.
    alloc_marker: PhantomData<fn(A)>,
}
impl<A: Allocator> Copy for LayoutInfo<A> {}
impl<A: Allocator> Clone for LayoutInfo<A> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<A: Allocator> LayoutInfo<A> {
    /// The minimum alignment for the value.
    ///
    /// Necessary so that [`crate::Weak`] can have a reserved value.
    pub const MIN_VALUE_ALIGNMENT: usize = 2;

    #[inline]
    pub fn new_or_panic(layout: Layout) -> Self {
        #[cold]
        #[inline(never)]
        fn layout_overflow() -> ! {
            panic!("Layout of Brc would overflow an isize")
        }
        match Self::new(layout) {
            Ok(res) => res,
            Err(_) => layout_overflow(),
        }
    }

    #[inline]
    pub fn new(layout: Layout) -> Result<Self, core::alloc::LayoutError> {
        let (full_layout, value_offset) =
            Layout::new::<BrcHeader<A>>().extend(layout.align_to(Self::MIN_VALUE_ALIGNMENT)?)?;
        Ok(LayoutInfo {
            full_layout,
            #[expect(clippy::cast_possible_wrap, reason = "offset fits in isize")]
            value_offset: value_offset as isize,
            alloc_marker: PhantomData,
        })
    }

    /// Compute the offset (in bytes) from the value to get to the header.
    #[inline]
    pub fn header_offset(&self) -> isize {
        // SAFETY: Cannot overflow because offset is guaranteed to fit in `isize`
        unsafe { 0isize.unchecked_sub(self.value_offset) }
    }

    /// Compute the layout information for the specified value.
    ///
    /// Simply passes the result of [`core::alloc::Layout::for_value`] to [`Self::new`],
    /// then calls [`Result::unwrap_unchecked`] on the result
    ///
    /// # Safety
    /// Undefined behavior if the computed layout would overflow when passed to [`Self::new`].
    #[inline]
    pub unsafe fn for_value<T: ?Sized>(value: &T) -> Self {
        let value_layout = Layout::for_value(value);
        // SAFETY: Caller guaranteed will not overflow
        unsafe { Self::new(value_layout).unwrap_unchecked() }
    }
}
