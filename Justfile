export RUST_BACKTRACE := "1"

test: _check && check-format
    cargo +nightly nextest run --all-features
    cargo +stable nextest run

coverage: _check
    cargo llvm-cov nextest --all-features

# Due to header arithmetic, we had problems with stacked borrows in the past.
# Right now it seems to be working. If problems come up switch back to -Zmiri-tree-borrows.
export MIRIFLAGS := "-Zmiri-strict-provenance -Zmiri-env-forward=RUST_BACKTRACE"

miri *args: _check && check-format
    cargo +nightly miri nextest run --all-features {{args}}

check: _check check-format

_check:
    cargo +nightly clippy --all-targets --all-features
    cargo +stable clippy --all-targets
    cargo doc --document-private-items --no-deps

format: && spellcheck
    cargo fmt --all
    taplo format
    cargo sort --grouped --workspace .

check-format: && spellcheck
    cargo reedme --check
    cargo fmt --check --all
    taplo format --check
    cargo sort --grouped --workspace --check .

spellcheck:
    @# use pinned version to avoid breaking build
    uvx typos@1.46
