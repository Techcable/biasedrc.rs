use crate::raw::threads::{InvalidThreadError, ShortThreadId};
use arbitrary_int::prelude::*;
use core::marker::PhantomPinned;
use core::sync::atomic::AtomicU32;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

mod threads;

/// The object header for a [`Brc`].
///
/// Seperated from the [`Brc`] to allow more detailed control of allocation.
///
/// # Safety
/// Calling [`Self::decrement`] incorrectly can lead to use-after-free.
/// It is assumed that a header will not be
///
/// The address of the header is relied upon in some cases,
/// so it must never move in memory after it is constructed.
/// This is not statically
///
/// [`Brc`]: crate::Brc
#[repr(C)]
pub struct RawBrcHeader {
    biased_word: AtomicU32,
    shared_word: AtomicU32,
    marker: PhantomPinned,
}
impl RawBrcHeader {
    /// Initialize the header, biasing towards the current thread.
    ///
    /// # Safety
    /// The resulting header must be pinned in-memory and never moved.
    #[inline]
    pub unsafe fn init() -> Self {
        let this_id = threads::ThreadInfo::current_id().ok();
        match this_id {
            None => RawBrcHeader {
                shared_word: AtomicU32::new(
                    SharedWord {
                        shared_count: i30::new(1),
                        // mark as merged
                        merged: true,
                        queued: false,
                    }
                    .to_raw(),
                ),
                biased_word: AtomicU32::new(BiasedWord::UNOWNED.to_raw()),
                marker: PhantomPinned,
            },
            Some(this_id) => RawBrcHeader {
                biased_word: AtomicU32::new(
                    BiasedWord {
                        biased_count: u14::new(1),
                        owner_id: Some(this_id),
                    }
                    .to_raw(),
                ),
                shared_word: AtomicU32::new(
                    SharedWord {
                        queued: false,
                        merged: false,
                        shared_count: i30::new(0),
                    }
                    .to_raw(),
                ),
                marker: PhantomPinned,
            },
        }
    }

    /// Increment the object's strong count.
    ///
    /// # Safety
    /// This is a safe operation for the same reason that [`std::mem::forget`] is.
    #[inline]
    pub fn increment_strong(&self) {
        if self.attempt_fast_increment().is_err() {
            self.slow_increment()
        }
    }

    #[inline]
    fn attempt_fast_increment(&self) -> Result<(), FastIncrementFailure> {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let incremented_counter = biased_word
            .biased_count
            .checked_add(u14::new(1))
            .ok_or(FastIncrementFailure)?;
        let this_id = self::threads::ThreadInfo::current_id().map_err(|_| FastIncrementFailure)?;
        if biased_word.owner_id == Some(this_id) {
            self.biased_word.store(
                BiasedWord {
                    biased_count: incremented_counter,
                    ..biased_word
                }
                .to_raw(),
                Ordering::Relaxed,
            );
            Ok(())
        } else {
            Err(FastIncrementFailure)
        }
    }

    #[cold]
    #[inline(never)]
    fn slow_increment(&self) {
        self.shared_word
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old| {
                let old = SharedWord::from_raw(old);
                let new_count = old
                    .shared_count
                    .checked_add(i30::new(1))
                    .expect("refcnt overflow");
                Some(
                    SharedWord {
                        shared_count: new_count,
                        ..old
                    }
                    .to_raw(),
                )
            })
            .unwrap();
    }

    /// Decrement the object's strong count,
    /// calling the specified destructor function on failure.
    ///
    /// # Safety
    /// Once a header is destroyed, it should never be used again.
    ///
    /// Undefined behavior if not correctly paired with [`Self::increment_strong`].
    #[inline]
    pub unsafe fn decrement_strong<D: DestructorFunc>(&self) {}

    #[inline]
    unsafe fn fast_decrement<D: DestructorFunc>(&self) -> Result<(), FastDecrementFailure> {
        let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
        let this_id = threads::ThreadInfo::current_id().map_err(|_| FastDecrementFailure)?;
        if Some(this_id) == biased_word.owner_id {
            debug_assert_ne!(biased_word.biased_count.value(), 0);
            // SAFETY: Caller guarantees that refcnt > 0
            let biased_count =
                unsafe { u14::new_unchecked(biased_word.biased_count.value().unchecked_sub(1)) };
            if biased_count.value() > 0 {
                Ok(())
            } else {
                unsafe {
                    self.fast_decrement_slow::<D>();
                }
                Ok(())
            }
        } else {
            Err(FastDecrementFailure)
        }
    }

    #[cold]
    #[inline(never)]
    unsafe fn fast_decrement_slow<D: DestructorFunc>(&self) {
        let new = SharedWord::from_raw(
            self.shared_word
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old| {
                    let old = SharedWord::from_raw(old);
                    debug_assert!(!old.merged);
                    Some(
                        SharedWord {
                            merged: old.merged,
                            ..old
                        }
                        .to_raw(),
                    )
                })
                .unwrap(),
        );
        debug_assert!(new.shared_count.value() >= 0);
        if new.shared_count.value() == 0 {
            // SAFETY: The pointer is valid, and it is time to deallocate
            unsafe { D::dealloc(NonNull::from(self)) }
        } else {
            // release ownership
            self.biased_word
                .store(BiasedWord::UNOWNED.to_raw(), Ordering::Relaxed);
        }
    }

    #[cold]
    #[inline(never)]
    unsafe fn slow_decrement<D: DestructorFunc>(&self) {
        let mut old = SharedWord::from_raw(self.shared_word.load(Ordering::Relaxed));
        let mut new: SharedWord;
        loop {
            new = SharedWord {
                shared_count: old
                    .shared_count
                    .checked_sub(i30::new(1))
                    .expect("refcnt underflow"),
                ..old
            };
            if new.shared_count.value() < 0 {
                new.queued = true;
            }
            match self.shared_word.compare_exchange_weak(
                old.to_raw(),
                new.to_raw(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => old = SharedWord::from_raw(x),
            }
        }
        debug_assert!(!new.merged || new.shared_count.value() >= 0);
        if old.queued != new.queued {
            let biased_word = BiasedWord::from_raw(self.biased_word.load(Ordering::Relaxed));
            let owner_id = biased_word
                .owner_id
                .expect("due to negative refcnt, must have owner");
            // SAFETY: Queued object is
            match unsafe {
                threads::ThreadInfo::get_by_id(owner_id)
                    .expect("owner thread info is undefined")
                    .queue_object(QueuedObject {
                        ptr: NonNull::from(self),
                        drop: D::dealloc,
                    })
            } {
                Ok(()) => {}
                Err(InvalidThreadError::IdOverflow) => unreachable!(),
                Err(InvalidThreadError::DeadOrDying) => {
                    // SAFETY: Since thread is dead, we can do the explicit merge
                    unsafe {
                        self::explicit_merge(
                            owner_id,
                            QueuedObject {
                                ptr: NonNull::from(self),
                                drop: D::dealloc,
                            },
                        )
                    }
                }
            }
        } else if new.merged && new.shared_count.value() == 0 {
            // SAFETY: Valid to deallocate
            unsafe {
                D::dealloc(NonNull::from(self));
            }
        }
    }
}

unsafe impl Send for RawBrcHeader {}
unsafe impl Sync for RawBrcHeader {}
#[derive(Debug)]
struct FastIncrementFailure;
#[derive(Debug)]
struct FastDecrementFailure;

/// A deallocation function.
///
/// This is functionally equivalent to an `unsafe fn(*mut RawBrcHeader)`,
/// but is its own trait so that it gets monomorphized.
pub trait DestructorFunc: Copy {
    unsafe fn dealloc(&self, ptr: NonNull<RawBrcHeader>);
}

#[derive(Copy, Clone, Debug)]
struct BiasedWord {
    owner_id: Option<ShortThreadId>,
    biased_count: u14,
}
impl BiasedWord {
    const UNOWNED: BiasedWord = BiasedWord {
        owner_id: None,
        biased_count: u14::ZERO,
    };
    #[inline]
    fn to_raw(&self) -> u32 {
        (self.biased_count.value() as u32)
            | (self.owner_id.map_or(0, |value| value.value().value()) << ShortThreadId::BITS)
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        BiasedWord {
            owner_id: ShortThreadId::new(arbitrary_int::u18::masked_new(raw)),
            biased_count: arbitrary_int::u14::masked_new(raw >> ShortThreadId::BITS),
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct SharedWord {
    shared_count: i30,
    merged: bool,
    queued: bool,
}
impl SharedWord {
    #[inline]
    fn to_raw(&self) -> u32 {
        self.shared_count.to_bits() | ((self.merged as u32) << 30) | ((self.queued as u32) << 31)
    }
    #[inline]
    fn from_raw(raw: u32) -> Self {
        SharedWord {
            shared_count: i30::masked_new(raw),
            merged: (raw & (1 << 30)) != 0,
            queued: (raw & (1 << 31)) != 0,
        }
    }
}

#[derive(Copy, Clone)]
struct QueuedObject {
    ptr: NonNull<RawBrcHeader>,
    drop: unsafe fn(NonNull<RawBrcHeader>),
}
unsafe impl Send for QueuedObject {}
unsafe impl Sync for QueuedObject {}

#[cold]
pub(crate) unsafe fn explicit_merge(biased_tid: ShortThreadId, object: QueuedObject) {
    // SAFETY: Validity guaranteed by caller
    let header = unsafe { object.ptr.as_ref() };
    // we own this so don't need a fence
    let biased = BiasedWord::from_raw(header.biased_word.load(Ordering::Relaxed));
    // now update the shared word
    assert_eq!(biased.owner_id, Some(biased_tid));
    let new_word = header
        .shared_word
        .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |old_word| {
            let old_word = SharedWord::from_raw(old_word);
            assert!(!old_word.merged);
            let biased_count = i30::from(biased.biased_count.value());
            Some(
                SharedWord {
                    shared_count: old_word
                        .shared_count
                        .checked_add(biased_count)
                        .expect("refcnt overflow when merging pointers"),
                    merged: true,
                    ..old_word
                }
                .to_raw(),
            )
        })
        .map(SharedWord::from_raw)
        .unwrap();
    assert!(new_word.shared_count.value() >= 0, "{new_word:?}");
    if new_word.shared_count.value() == 0 {
        // SAFETY: Caller promises the drop function is valid
        unsafe { (object.drop)(object.ptr) }
    } else {
        // release ownership/unbias
        header.biased_word.store(
            BiasedWord {
                owner_id: None,
                biased_count: u14::ZERO,
            }
            .to_raw(),
            Ordering::Release,
        )
    }
}
