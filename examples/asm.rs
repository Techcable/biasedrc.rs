//! Used with `cargo asm` to look at generated assembly.
#![allow(clippy::disallowed_types, clippy::no_mangle_with_rust_abi)]

use biasedrc::Brc;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::Arc;

#[unsafe(no_mangle)]
pub fn brc_new(x: u32) -> Brc<u32> {
    Brc::new(x)
}

#[unsafe(no_mangle)]
pub fn arc_new(x: u32) -> Arc<u32> {
    Arc::new(x)
}

#[unsafe(no_mangle)]
pub fn rc_new(x: u32) -> Arc<u32> {
    Arc::new(x)
}

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

macro_rules! pretend_used_simple {
    ($($func:ident),+ $(,)?) => {
        $(used(&($func as fn(_) -> _));)*
    };
}
pub fn main() {
    #[inline(never)]
    fn used(x: &dyn Debug) {
        println!("used: {:?}", core::hint::black_box(x));
    }
    pretend_used_simple!(
        brc_new, arc_new, rc_new, brc_clone, arc_clone, rc_clone, brc_drop, arc_drop, rc_drop
    );
    used(&(brc_collect as fn()));
}
