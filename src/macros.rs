//! Helper macros

/// Implements [`core::ops::Drop`] with a `#[may_dangle]` attribute.
macro_rules! drop_may_dangle {
    (unsafe impl<#[may_dangle] $primary:ident: ?Sized + $primary_bound:ident, $alloc:ident: $alloc_bound:ident> Drop for $target:path {
        $($inner:tt)*

    }) => {
        #[cfg(feature = "nightly-may-dangle")]
        // SAFETY: Guaranteed by caller
        unsafe impl<#[may_dangle] $primary: ?Sized + $primary_bound, $alloc: $alloc_bound> Drop for $target {
            $($inner)*
        }
        #[cfg(not(feature = "nightly-may-dangle"))]
        impl<$primary: ?Sized + $primary_bound, $alloc: $alloc_bound> Drop for $target {
            $($inner)*
        }
    };
}

/// Implements smart-pointer logic common to [`crate::Weak`] and [`crate::Brc`].
///
/// Use `SmartPointerBasics` if the type has no Deref to delegate to ([`crate::Weak`]),
/// and `SmartPointer` if it does have a [`Deref`] impl.
///
/// This doesn't include the following impls:
/// - `Send`
/// - `Sync`
/// - `Clone`
/// - `Deref`
/// - `Drop`
///
/// [`Deref`]: core::ops::Deref
///
/// # Safety
/// Use of `SmartPointerBasics` requires all the following:
/// - The `into_raw/from_raw` methods work properly
/// - The type can safely implement `Unpin`.
///
/// Implementing `SmartPointer` also requires:
/// - The pointer can implement [`stable_deref_trait::StableDeref`].
macro_rules! smart_pointer {
    (unsafe impl<$primary:ident: ?Sized + $primary_bound:ident, $alloc:ident: Allocator> SmartPointerBasics for $pointer:ident {}) => {
        impl<$primary: ?Sized + $primary_bound, $alloc: Allocator> core::marker::Unpin for $pointer<$primary, $alloc> {}
        // SAFETY: Preserves target and provenance in replace_ptr.
        // Uses on the caller's guarantee from_raw/into_raw work properly
        unsafe impl<$primary, U: ?Sized + $primary_bound, $alloc: Allocator> unsize::CoerciblePtr<U> for $pointer<$primary, $alloc> {
            type Pointee = $primary;
            type Output = $pointer<U, $alloc>;

            #[inline]
            fn as_sized_ptr(&mut self) -> *mut Self::Pointee {
                // The safety of this implementation for Weak is subtle.
                // If there are still strong references, everything works fine.
                // If there are no strong references,
                // the result of Weak::as_ptr is well-defined but dangling
                //
                // However, it is still safe to call
                // Weak::from_raw even with a strong count of zero
                // as long as we still own a weak reference
                $pointer::as_ptr(&*self).cast_mut()
            }

            #[inline]
            unsafe fn replace_ptr(self, new: *mut U) -> Self::Output {
                // SAFETY: Caller has guaranteed that `new` is
                // just an unsized version of the original
                //
                // Ownership is correctly transferred from `self` to result.

                // Provenance transferred into `raw` as per `into_raw`.
                let raw = $pointer::into_raw(self).cast_mut();
                // SAFETY: Provenance merged into `new` as per `replace_ptr`.
                let new: *mut U = unsafe { <*mut T as unsize::CoerciblePtr<U>>::replace_ptr(raw, new) };
                // SAFETY: Provenance transferred as per `from_raw`, originally from `into_raw`
                unsafe { $pointer::from_raw(new) }
            }
        }
        #[cfg(feature = "nightly-coerce")]
        impl<$primary: ?Sized, U: ?Sized, $alloc: Allocator> core::ops::CoerceUnsized<$pointer<U, A>> for $pointer<$primary, $alloc>
        where
            $primary: core::marker::Unsize<U> + $primary_bound,
            U: $primary_bound,
        {
        }
    };
    (unsafe impl<$primary:ident: ?Sized + $primary_bound:ident, $alloc:ident: Allocator> SmartPointer for $pointer:ident {}) => {
        smart_pointer! {
            // SAFETY: Caller guarantees the basic requirements are met as well
            unsafe impl<$primary: ?Sized + $primary_bound, $alloc: Allocator> SmartPointerBasics for $pointer {}
        }
        impl<$primary: ?Sized + $primary_bound + core::error::Error, $alloc: Allocator> core::error::Error for $pointer<$primary, $alloc> {
            #[inline]
            fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
                self.deref().source()
            }

            #[allow(deprecated, reason = "delegate")]
            #[inline]
            fn description(&self) -> &str {
                self.deref().description()
            }

            #[allow(deprecated, reason = "delegate")]
            #[inline]
            fn cause(&self) -> Option<&dyn core::error::Error> {
                self.deref().cause()
            }
        }
        impl<$primary: ?Sized + $primary_bound, $alloc: Allocator> core::borrow::Borrow<$primary> for $pointer<$primary, $alloc> {
            #[inline]
            fn borrow(&self) -> &T {
                self.deref()
            }
        }
        impl<$primary: ?Sized + $primary_bound, $alloc: Allocator> AsRef<$primary> for $pointer<$primary, $alloc> {
            #[inline]
            fn as_ref(&self) -> &T {
                self.deref()
            }
        }
        impl<$primary: ?Sized + $primary_bound + core::fmt::Debug, $alloc: Allocator> core::fmt::Debug for $pointer<$primary, $alloc> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Debug::fmt(self.deref(), f)
            }
        }
        impl<$primary: ?Sized + $primary_bound + core::fmt::Display, $alloc: Allocator> core::fmt::Display for $pointer<$primary, $alloc> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(self.deref(), f)
            }
        }
        #[warn(clippy::missing_trait_methods)]
        #[allow(clippy::partialeq_ne_impl, reason = "smart pointer should delegate")]
        impl<$primary: ?Sized + $primary_bound + PartialEq, $alloc: Allocator> PartialEq for $pointer<$primary, $alloc> {
            smart_pointer!(@delegate_cmp_ops eq, ne);
        }
        impl<$primary: ?Sized + $primary_bound + PartialOrd, $alloc: Allocator> PartialOrd for $pointer<$primary, $alloc> {
            #[inline]
            fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
                self.deref().partial_cmp(other.deref())
            }
            smart_pointer!(@delegate_cmp_ops lt, gt, le, ge);
        }
        // We don't implement min/max/clamp through delegation,
        // as as that would require allocating a new Brc
        impl<$primary: ?Sized + $primary_bound + Ord, $alloc: Allocator> Ord for $pointer<$primary, $alloc> {
            #[inline]
            fn cmp(&self, other: &Self) -> core::cmp::Ordering {
                self.deref().cmp(other.deref())
            }
        }
        impl<$primary: ?Sized + $primary_bound + core::hash::Hash, $alloc: Allocator> core::hash::Hash for $pointer<$primary, $alloc> {
            #[inline]
            fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
                self.deref().hash(state);
            }
        }
        impl<$primary: ?Sized + $primary_bound + Eq, $alloc: Allocator> Eq for $pointer<$primary, $alloc> {}
        // SAFETY: Caller guarantees it is valid to implement StableDeref
        unsafe impl<$primary: ?Sized + $primary_bound, $alloc: Allocator> stable_deref_trait::StableDeref for $pointer<$primary, $alloc> {}
    };
    (@delegate_cmp_ops $($name:ident),+ $(,)?) => {
        $(#[inline]
        fn $name(&self, other: &Self) -> bool {
            self.deref().$name(other.deref())
        })*
    };
}
