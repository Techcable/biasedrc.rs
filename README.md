# biasedrc.rs
<!-- cargo-rdme start -->

An implementation of [biased reference counting] for Rust.

This crate requires the standard library due to use of [`std::thread_local!`].

[biased reference counting]: https://dl.acm.org/doi/pdf/10.1145/3243176.3243195

## Prior Art
- [trc](https://github.com/EricLBuehler/trc) - Requires explicit choice of either `SharedTrc` or `Trc`,
  avoiding need for runtime checks but preventing use as a drop-in replacement for `Arc`
- [hybrid_rc](https://gitlab.com/cg909/rust-hybrid-rc) - Appears to require a similar choice as `trc` between shared and local references.

<!-- cargo-rdme end -->

## License
Licensed under either the [Apache 2.0 License](./LICENSE-APACHE.txt) or [MIT License](./LICENSE-MIT.txt) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you,
as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
