use crate::allocator_api::alloc::{Allocator, Global};
use crate::layout::{BrcHeader, LayoutInfo, WEAK_OVERFLOW_THRESHOLD};
use crate::{Brc, ImpreciseRefCountError, SupportedWeakPointee, runtime};
use core::alloc::Layout;
use core::fmt::{Debug, Formatter};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

/// A [`Weak`] which is not the dummy reserved value, and so is "real."
#[derive(bytemuck::TransparentWrapper)]
#[repr(transparent)]
struct WeakReal<T: ?Sized + SupportedWeakPointee, A: Allocator>(Weak<T, A>);
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> WeakReal<T, A> {
    #[inline]
    fn value_ptr(&self) -> NonNull<T> {
        self.0.value_ptr_or_reserved
    }

    #[inline]
    fn value_layout(&self) -> Layout {
        // SAFETY: Value pointer used to be valid,
        // so we can call Layout::for_Value
        unsafe { T::layout_for_ptr(self.value_ptr().as_ptr()) }
    }

    #[inline]
    fn layout_info(&self) -> LayoutInfo<A> {
        // SAFETY: Since allocation was successful,
        // we know that this layout calculation cannot fail
        unsafe { LayoutInfo::new(self.value_layout()).unwrap_unchecked() }
    }

    #[inline]
    fn header_ptr(&self) -> NonNull<BrcHeader<A>> {
        let layout_info = self.layout_info();
        // SAFETY: We trust the `header_offset` from the layout info to be valid
        unsafe {
            self.value_ptr()
                .byte_offset(layout_info.header_offset())
                .cast::<BrcHeader<A>>()
        }
    }

    #[inline]
    fn header(&self) -> &BrcHeader<A> {
        // SAFETY: We trust the returned pointer to live for &self
        unsafe { self.header_ptr().as_ref() }
    }
}

/// A weak-reference to a [`Brc`].
///
/// Mirrors [`std::sync::Weak`].
#[repr(transparent)]
pub struct Weak<T: ?Sized + SupportedWeakPointee, A: Allocator = Global> {
    /// Either a pointer to where the value used to be,
    /// or [`usize::MAX`] if returned from [`Weak::new`].
    ///
    /// This value can never be held by a valid pointer since [`LayoutInfo::MIN_VALUE_ALIGNMENT`]
    /// is greater than one.
    ///
    /// If the value is reserved, it must use the [`Global`] allocator.
    /// This is necessary for [`Weak::allocator`] to work correctly.
    value_ptr_or_reserved: NonNull<T>,
    alloc_marker: PhantomData<A>,
}
impl<T> Default for Weak<T> {
    #[inline]
    fn default() -> Self {
        Weak::new()
    }
}
impl<T> Weak<T> {
    /// A [`Weak`] instance that points to nothing,
    /// and can never be upgraded.
    ///
    /// Mirrors [`std::sync::Weak::new`].
    #[inline]
    pub const fn new() -> Self {
        const {
            assert!(LayoutInfo::<Global>::MIN_VALUE_ALIGNMENT >= 2);
        }
        Weak {
            value_ptr_or_reserved: NonNull::without_provenance(NonZeroUsize::MAX),
            alloc_marker: PhantomData,
        }
    }
}

impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Weak<T, A> {
    /// The allocator that the weak reference originated from.
    ///
    /// Mirrors [`Weak::allocator`].
    #[inline]
    pub fn allocator(&self) -> &A {
        match self.real() {
            Some(real) => &real.header().alloc,
            None => {
                const GLOBAL_REF: &Global = &Global;
                // SAFETY: The reserved value must use the global allocator
                unsafe { &*core::ptr::from_ref(GLOBAL_REF).cast::<A>() }
            }
        }
    }

    /// Get the number of strong references to the underlying object,
    /// or an error if that cannot be precisely determined.
    ///
    /// Returns 0 for the result of [`Weak::new`].
    ///
    /// # Errors
    /// Just like [`Brc::strong_count`], this can return an error
    /// if the strong count cannot be precisely determined.
    /// See that function for more details.
    #[inline]
    pub fn strong_count(&self) -> Result<usize, ImpreciseRefCountError> {
        let Some(real) = self.real() else {
            return Ok(0);
        };
        real.header().strong.strong_count(Ordering::Relaxed)
    }

    /// Get the number of weak references to the underlying object,
    /// or an error if that cannot be precisely determined.
    ///
    /// Returns 0 for the result of [`Weak::new`].
    /// Also returns zero if there are no outstanding strong references.
    ///
    /// # Accuracy
    /// This can fail to produce an exact result on a non-biased thread,
    /// which will result in an [`ImpreciseRefCountError`].
    ///
    /// Just like [`std::sync::Weak::weak_count`],
    /// the result may be off by one in either direction.
    /// This imprecision will not result in an `Err`.
    ///
    /// # Errors
    /// Just like [`Brc::weak_count`], this will return an error
    /// if the weak count cannot be precisely determined.
    /// See that function for more details.
    pub fn weak_count(&self) -> Result<usize, ImpreciseRefCountError> {
        let Some(real) = self.real() else {
            return Ok(0);
        };
        // mirrors the impl of std::sync::Weak::weak_count
        let weak = real.header().weak_count.load(Ordering::Acquire);
        match real.header().strong.strong_count(Ordering::Relaxed) {
            Ok(0) => Ok(0),
            Err(e @ ImpreciseRefCountError { lower_bound: 0 }) => {
                // cannot precisely determine whether strong count is zero,
                // so we cannot know whether we should return zero
                Err(e.clone())
            }
            Ok(1..) | Err(ImpreciseRefCountError { lower_bound: 1.. }) => {
                // a nonzero strong count means there is a nonzero weak count,
                // due to the weak pointer shared among all strong pointers.
                // This shared weak pointer is an implementation detail
                // which does not correspond to a user-visible `Weak`.
                // As such, it should be subtracted out from the total.
                #[expect(clippy::missing_panics_doc, reason = "internal error")]
                Ok(weak.checked_sub(1).expect("strong without weak") as usize)
            }
        }
    }

    /// Return a [`WeakReal`] corresponding to an actual allocation,
    /// or `None` if this is the reserved value from [`Self::new`].
    #[inline]
    fn real(&self) -> Option<&'_ WeakReal<T, A>> {
        if self.value_ptr_or_reserved.addr() != NonZeroUsize::MAX {
            debug_assert!(
                self.value_ptr_or_reserved.addr().get().is_multiple_of(2),
                "UB: non-reserved pointer inappropriately aligned"
            );
            // we know its not reserved
            Some(bytemuck::TransparentWrapper::wrap_ref(self))
        } else {
            None
        }
    }

    /// Upgrade to an owned reference,
    /// returning `None` if the memory has been freed.
    ///
    /// Mirrors [`std::sync::Weak::upgrade`].
    #[inline]
    pub fn upgrade(&self) -> Option<Brc<T, A>> {
        let this = self.real()?;
        let value_ptr = this.value_ptr();
        let header = this.header();
        match header.strong.increment_strong_unless_zero() {
            Ok(()) => {
                // SAFETY: Success of increment_strong means we have an owned references
                unsafe { Some(Brc::from_raw(value_ptr.as_ptr())) }
            }
            Err(runtime::ZeroReferenceCountError) => None,
        }
    }

    /// Recreate a weak references from a raw pointer.
    ///
    /// Consumes ownership of a weak reference.
    ///
    /// # Safety
    /// Pointer must have originally come from [`Brc::into_raw`],
    /// and correspond to an owned weak reference.
    ///
    /// Mirrors the requirements of [`Brc::from_raw`].
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        Weak {
            // SAFETY: Caller guarantees the pointer is valid ,
            // and into_raw never returns null
            value_ptr_or_reserved: unsafe { NonNull::new_unchecked(ptr.cast_mut()) },
            alloc_marker: PhantomData,
        }
    }

    /// Convert a weak reference into a raw pointer.
    #[inline]
    pub fn into_raw(self) -> *const T {
        let this = ManuallyDrop::new(self);
        this.as_ptr()
    }

    /// Get a raw pointer to the underlying object.
    ///
    /// The object is valid only if there are still strong references to the value.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.value_ptr_or_reserved.as_ptr()
    }

    /// Checks if this weak points to the same object as the other.
    ///
    /// The result of [`Weak::new()`] is equal only to itself.
    ///
    /// This does not compare pointer metadata for trait objects,
    /// just like [`std::sync::Weak::ptr_eq`] and [`core::ptr::addr_eq`].
    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        // ignores trait object metadata, just like std::sync::Weak::ptr_eq
        // slice length is irrelevant because Weak<[T]> always points to the full slice
        core::ptr::addr_eq(self.as_ptr(), other.as_ptr())
    }
}
drop_may_dangle! {
// SAFETY: We respect the may_dangle requirements
unsafe impl<#[may_dangle] T: ?Sized + SupportedWeakPointee, A: Allocator> Drop for Weak<T, A> {
    #[inline]
    fn drop(&mut self) {
        if let Some(this) = self.real() {
            // SAFETY: Our existence implies we own a weak reference
            unsafe {
                BrcHeader::drop_weak(this.header_ptr().as_ptr(), this.layout_info());
            }
        }
    }
}
}
impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Clone for Weak<T, A> {
    #[inline]
    fn clone(&self) -> Self {
        match self.real() {
            Some(real) => {
                let header = real.header();
                // cannot possibly be locked as justified by std::sync::Weak::clone
                let old_count = header.weak_count.fetch_add(1, Ordering::AcqRel);
                if old_count > WEAK_OVERFLOW_THRESHOLD {
                    runtime::fatal_errors::weak_refcnt_overflow();
                }
                Weak {
                    alloc_marker: PhantomData,
                    value_ptr_or_reserved: self.value_ptr_or_reserved,
                }
            }
            None => Weak {
                alloc_marker: PhantomData,
                value_ptr_or_reserved: self.value_ptr_or_reserved,
            },
        }
    }
}
smart_pointer! {
    unsafe impl<T: ?Sized + SupportedWeakPointee, A: Allocator> SmartPointerBasics for Weak {}
}

// We might be able to be more conservative with these bounds,
// but this is what std::sync::Weak does
// SAFETY: We are careful to be thread-safe
unsafe impl<T: ?Sized + SupportedWeakPointee + Send + Sync, A: Allocator + Send + Sync> Send
    for Weak<T, A>
{
}
// SAFETY: We are careful to be thread-safe
unsafe impl<T: ?Sized + SupportedWeakPointee + Send + Sync, A: Allocator + Send + Sync> Sync
    for Weak<T, A>
{
}

impl<T: ?Sized + SupportedWeakPointee, A: Allocator> Debug for Weak<T, A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str("(Weak)")
    }
}

/// Guard to call [`BrcHeader::drop_weak`] in the destructor.
///
/// This guard can ensure that the underlying memory is freed,
/// even if the destructor of `T` panics.
///
/// This can be better than a real [`Weak`] pointer in some situations,
/// as it doesn't require a [`SupportedWeakPointee`] bound.
/// However, it means the layout information must be passed to the constructor.
///
/// It is not necessary to depend on `T`,
/// as we already have the layout information and don't drop `T`.
pub(crate) struct WeakDropGuard<A: Allocator> {
    header_ptr: NonNull<BrcHeader<A>>,
    layout_info: LayoutInfo<A>,
}
impl<A: Allocator> WeakDropGuard<A> {
    #[inline]
    pub fn header_ptr(&self) -> NonNull<BrcHeader<A>> {
        self.header_ptr
    }

    /// Create a new [`WeakDropGuard`] from the specified header pointer and layout info.
    ///
    /// This effectively consumes ownership of a weak reference
    /// in a manner similar to [`crate::Weak::from_raw`].
    ///
    /// # Safety
    /// The header pointer must point to a valid object,
    /// with the matching layout information
    ///
    /// Must be valid to invoke [`BrcHeader::drop_weak`] when the destructor is called.
    /// In other words, there must be a weak reference to take ownership of.
    #[inline]
    pub unsafe fn new(header_ptr: NonNull<BrcHeader<A>>, layout_info: LayoutInfo<A>) -> Self {
        WeakDropGuard {
            header_ptr,
            layout_info,
        }
    }

    /// Return a pointer to the value, based on [`Self::layout_info`].
    ///
    /// # Safety
    /// This is safe, as layout information is trusted by construction.
    #[inline]
    pub fn value_ptr(&self) -> NonNull<()> {
        // SAFETY: We trust the LayoutInfo to give an accurate offset,
        // and we know allocated memory matches it
        unsafe {
            self.header_ptr()
                .byte_offset(self.layout_info.value_offset())
                .cast::<()>()
        }
    }
}
impl<A: Allocator> Drop for WeakDropGuard<A> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Safe to drop because that is guaranteed by construction
        unsafe {
            BrcHeader::<A>::drop_weak(self.header_ptr.cast().as_ptr(), self.layout_info);
        }
    }
}
