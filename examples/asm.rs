//! Used with `cargo asm` to look at generated assembly.
#![allow(
    missing_docs,
    clippy::disallowed_types,
    clippy::no_mangle_with_rust_abi
)]

use biasedrc::Brc;
use std::rc::Rc;
use std::sync::Arc;

#[unsafe(no_mangle)]
pub fn brc_collect() {
    biasedrc::collect();
}

#[unsafe(no_mangle)]
pub fn brc_clone(x: &Brc<u32>) -> Brc<u32> {
    x.clone()
}

#[unsafe(no_mangle)]
pub fn arc_clone(x: &Arc<u32>) -> Arc<u32> {
    x.clone()
}

#[unsafe(no_mangle)]
pub fn rc_clone(x: &Rc<u32>) -> Rc<u32> {
    x.clone()
}

#[unsafe(no_mangle)]
pub fn brc_drop(x: Brc<u32>) {
    drop(x);
}

#[unsafe(no_mangle)]
pub fn arc_drop(x: Arc<u32>) {
    drop(x);
}

#[unsafe(no_mangle)]
pub fn rc_drop(x: Rc<u32>) {
    drop(x);
}

pub fn main() {
    unimplemented!()
}
