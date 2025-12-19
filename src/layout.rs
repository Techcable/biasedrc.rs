//! Defines the underlying layout of the [`crate::Brc`] type in-memory.

#[allow(unused_imports, reason = "used for docs")]
use crate::Brc;
use crate::SupportedWeakPointee;
use crate::allocator_api::alloc::{AllocError, Allocator};
use crate::runtime::RawBrcHeader;
use crate::weak::WeakDropGuard;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

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
///
/// # Internal Invariants
/// The internal `value_offset` must match the `full_layout`.
/// This can only be violated using unsafe code.
pub struct LayoutInfo<A: Allocator> {
    value_offset: isize,
    full_layout: Layout,
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

    /// Create a new [`LayoutInfo`] for the specified value,
    /// panicking if there is any arithmetic overflow.
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

    /// Return the full layout of the allocated object,
    /// including both the header and the value.
    #[inline]
    pub fn full_layout(&self) -> Layout {
        self.full_layout
    }

    /// Compute the offset (in bytes) from the header to get to the value.
    #[inline]
    pub fn value_offset(&self) -> isize {
        self.value_offset
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

    /// Compute the layout information for the specified pointer.
    ///
    /// This uses [`Layout::for_value_raw`] and so requires a [`crate::SupportedWeakPointee`].
    ///
    /// # Safety
    /// Undefined behavior if the computed layout would overflow when passed to [`Self::new`].
    ///
    /// Undefined behavior if the requirements of [`Layout::for_value_raw`] are violated.
    #[inline]
    pub unsafe fn for_value_raw<T: ?Sized + SupportedWeakPointee>(value: *const T) -> Self {
        // SAFETY: Caller guarantees validity
        let value_layout = unsafe { T::layout_for_ptr(value.cast_mut()) };
        // SAFETY: Caller guaranteed will not overflow
        unsafe { Self::new(value_layout).unwrap_unchecked() }
    }
}

/// A partially allocated [`crate::Brc`].
///
/// This type has no [`Drop`] implementation,
/// and will not free memory unless [`Self::dealloc`] is explicitly called.
pub(crate) struct PartialAlloc<A: Allocator> {
    header_ptr: NonNull<BrcHeader<A>>,
    layout_info: LayoutInfo<A>,
}
impl<A: Allocator> PartialAlloc<A> {
    #[inline]
    pub fn header_ptr(&self) -> NonNull<BrcHeader<A>> {
        self.header_ptr
    }

    /// Return a pointer to the value, based on [`Self::layout_info`].
    ///
    /// # Safety
    /// This is safe, as we allocated memory for the [`LayoutInfo`]
    /// and trust its value offset.
    #[inline]
    pub fn value_ptr(&self) -> NonNull<()> {
        // SAFETY: We trust the LayoutInfo to give an accurate offset,
        // and we know allocated memory matches it
        unsafe {
            self.header_ptr()
                .byte_offset(self.layout_info.value_offset)
                .cast::<()>()
        }
    }

    /// Finish this stage of the allocation,
    /// consuming ownership of a weak reference and returning a [`WeakDropGuard`].
    ///
    /// # Safety
    /// Requires the destructor of [`WeakDropGuard`] can safely call [`BrcHeader::drop_weak`].
    /// In other words, there must be a weak reference to take ownership of..
    ///
    /// This is automatically true if the weak count has not been modified since initialization.
    ///
    /// The spatial validity requirements of [`WeakDropGuard::new`] are known to hold
    /// since the header has been properly allocated.
    #[inline]
    pub unsafe fn into_weak_guard(self) -> WeakDropGuard<A> {
        let layout_info = self.layout_info;
        let header_ptr = self.header_ptr();
        // SAFETY: Known to point to a valid allocation of self.layout_info,
        // caller guarantees we point to a weak reference
        unsafe { WeakDropGuard::new(header_ptr, layout_info) }
    }

    /// Deallocate the allocated header using [`Allocator::deallocate`].
    ///
    /// # Safety
    /// Has the same requirements as [`Allocator::deallocate`].
    /// In addition, the header pointer must not be used again.
    pub unsafe fn dealloc(&self) {
        // SAFETY: Know header_ptr is part of valid allocation
        let allocator_ptr = unsafe { allocator_ptr_for(self.header_ptr) };
        // SAFETY: The allocator is inaccessible after this, so we can steal it
        let allocator = unsafe { allocator_ptr.read() };
        // SAFETY: We know the pointer is valid in the specified allocator,
        // because that is guaranteed by construction
        unsafe { allocator.deallocate(self.header_ptr.cast(), self.layout_info.full_layout()) }
    }
}

/// Given a pointer to a [`BrcHeader`], return a pointer to the allocator.
///
/// # Safety
/// Requires that [`BrcHeader`] point to an appropriately sized allocation.
#[inline]
unsafe fn allocator_ptr_for<A: Allocator>(header_ptr: NonNull<BrcHeader<A>>) -> NonNull<A> {
    // SAFETY: Caller guarantees validity of input pointer
    unsafe {
        header_ptr
            .byte_add(core::mem::offset_of!(BrcHeader<A>, alloc))
            .cast::<A>()
    }
}

/// Begin an allocation with the specified layout.
///
/// The [`PartialAlloc`] drop implementation cleans up the allocation
///
/// # Initial Values
/// The [`BrcHeader`] is fully initialized by this function,
/// and can safely be accessed by reference through [`PartialAlloc::header_ptr`].
///
/// The raw header begins in the state [`RawBrcHeader::new_uninit`].
/// The weak reference count begins with the value `1`.
/// The allocator is placed in the appropriate part of the header.
///
/// # Errors
/// If the allocation fails, this will return an [`core::alloc::AllocError`].
#[inline]
pub(crate) fn begin_alloc_in<A: Allocator>(
    layout_info: LayoutInfo<A>,
    allocator: A,
) -> Result<PartialAlloc<A>, AllocError> {
    let header_ptr = allocator
        .allocate(layout_info.full_layout)?
        .cast::<BrcHeader<A>>();
    // Nothing beyond this point should be able to panic
    // If that assumption is false, it is still fine as we would just leak memory
    // without exposing a partially constructed header.
    let allocator = ManuallyDrop::new(allocator);
    const {
        assert!(core::mem::offset_of!(BrcHeader<A>, strong) == 0);
    }
    // SAFETY: Memory is newly allocated so it is known to be valid.
    // The RawBrcHeader is uninitialized and not yet exposed to the runtime.
    // we just verified above that the field offset is zero,
    // so we can write directly to the start
    unsafe {
        header_ptr
            .cast::<RawBrcHeader>()
            .write(const { RawBrcHeader::new_uninit() });
    }
    // SAFETY: Newly allocated memory is valid
    unsafe {
        header_ptr
            .byte_add(core::mem::offset_of!(BrcHeader<A>, weak_count))
            .cast::<AtomicU32>()
            // there is a single weak reference shared among all strong references
            .write(const { AtomicU32::new(1) });
    }
    // SAFETY: Newly allocated memory
    unsafe {
        allocator_ptr_for(header_ptr).write(ManuallyDrop::into_inner(allocator));
    }
    Ok(PartialAlloc {
        header_ptr,
        layout_info,
    })
}

/// Allocate memory for a `BrcUnique<T>` with an arbitrary layout,
/// without initializing it or allowing it to be shared.
///
/// Simply wraps the result of [`begin_alloc_in`] in a [`WeakDropGuard`],
/// while still keeping the strong count [uninitialized](crate::runtime::RawBrcHeader::new_uninit).
/// This produces the same header state as [`crate::UniqueUninitBrc::new`] does.
///
/// # Errors
/// If the allocation fails, this will return an [`core::alloc::AllocError`].
#[inline]
pub(crate) fn begin_unique_alloc_in<A: Allocator>(
    layout_info: LayoutInfo<A>,
    allocator: A,
) -> Result<WeakDropGuard<A>, AllocError> {
    let alloc = begin_alloc_in(layout_info, allocator)?;
    // SAFETY: We have not modified the weak count
    Ok(unsafe { alloc.into_weak_guard() })
}
