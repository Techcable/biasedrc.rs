//! An implementation of [biased reference counting] for Rust.
//!
//! This crate requires the standard library due to use of [`std::thread_local!`].
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195
//!
//! # Tradeoffs
//! Like many low-level optimizations, biased reference counting has tradeoffs.
//!
//! The most significant advantage is performance.
//! On the biased thread, [`Brc::clone`] and [`Brc::drop`] are 2-3x faster than the equivalent operations on [`Arc`],
//! nearing the performance of [`Rc`].
//! Except for the final drop operation, clones and drops are the same speed as [`Arc`] on non-biased threads.
//!
//! Unfortunately, there are some performance downsides too:
//! Allocating a [`Brc`] is currently 20%-100% slower than allocating an [`Arc`],
//! and dropping the last reference is about 20% slower than with [`Arc`].
//! The costs are paid once per object, so the performance difference
//! is amortized away after just 1-3 biased clone/drop operations.
//!
//! Other major downsides of using biasedrc include the need to maintain a thread-local queue
//! and in some cases delayed memory reclamation.
//!
//! [`Rc`]: std::rc::Rc
//!
//! # Panics & Unwinding
//! This library aims to unwind only when an equivalent method on [`Arc`] would unwind.
//!
//! I say unwind here rather than panic, because the library needs to deal with deferred destructors panicking
//! during implicit collections.
//! If a a panic happens during an implicit collection, it will abort the program.
//! This is because unwinding from `Brc::<u32>::clone` or `Brc::<u32>::drop` would be highly unexpected,
//! and make transitioning from [`Arc`] to [`Brc`] much more difficult.
//! Unwinding from deferred destructors makes local reasoning about program behavior impossible,
//! which is one of the main reasons Dijkstra criticised goto.
//!
//! If unwinding panics during implicit collections are truly desired behavior,
//! they can be emulated by creating a newtype wrapper for a [`Brc`]
//! which calls [`collect`] instead of [`collect_nounwind`].
//!
//! # Prior Art
//! - [trc](https://github.com/EricLBuehler/trc) - Requires explicit choice of either `SharedTrc` or `Trc`,
//!   avoiding need for runtime checks but preventing use as a drop-in replacement for `Arc`
//! - [hybrid_rc](https://gitlab.com/cg909/rust-hybrid-rc) - Appears to require a similar choice as `trc` between shared and local references.
#![cfg_attr(feature = "nightly-ptr-meta", feature(ptr_metadata))]
#![cfg_attr(feature = "nightly-coerce", feature(coerce_unsized, unsize))]
#![cfg_attr(feature = "nightly-ptr-layout", feature(layout_for_ptr))]
#![cfg_attr(feature = "nightly-allocator", feature(allocator_api))]
#![cfg_attr(feature = "nightly-may-dangle", feature(dropck_eyepatch))]
#![deny(
    missing_docs,
    clippy::std_instead_of_core,
    clippy::std_instead_of_alloc,
    clippy::alloc_instead_of_core
)]

extern crate alloc;

#[cfg(not(feature = "std"))]
compile_error!("The standard library is required to use `biasedrc`");

#[cfg(feature = "nightly-allocator")]
pub(crate) use alloc as allocator_api;
#[cfg(not(feature = "nightly-allocator"))]
pub(crate) use allocator_api2 as allocator_api;
#[cfg(feature = "nightly-ptr-meta")]
pub(crate) use core::ptr as ptr_meta;
#[cfg(not(feature = "nightly-ptr-meta"))]
pub(crate) use ptr_meta_stable as ptr_meta;

#[allow(unused_imports, clippy::disallowed_types, reason = "used for docs")]
use alloc::sync::Arc;

#[macro_use]
mod macros;
mod layout;
mod pointee;
mod runtime;
mod strong;
mod third_party;
mod weak;

pub use crate::pointee::{SupportedPointee, SupportedWeakPointee};
pub use crate::runtime::{
    BiasedCountError, ImpreciseRefCountError, collect, collect_force, collect_nounwind,
};
pub use crate::strong::Brc;
#[allow(unused_imports, reason = "may be nop if no features are enabled")]
pub use crate::third_party::*;
pub use crate::weak::Weak;
