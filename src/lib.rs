//! An implementation of [biased reference counting] for Rust.
//!
//! This crate requires the standard library due to use of [`std::thread_local!`].
//!
//! [biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195
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
pub use crate::runtime::{BiasedCountError, ImpreciseRefCountError, collect, collect_force};
pub use crate::strong::Brc;
#[allow(unused_imports, reason = "may be nop if no features are enabled")]
pub use crate::third_party::*;
pub use crate::weak::Weak;
