//! Integration with third-party crates.
//!
//! All public items from this module should be re-exported at the crate root.
//! This module itself should be private.

#[cfg(feature = "arc-swap")]
mod arc_swap;
#[cfg(feature = "archery")]
mod archery;
#[cfg(feature = "serde")]
mod serde;

#[cfg(feature = "archery")]
pub use archery::BrcK;
