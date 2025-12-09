export RUST_BACKTRACE := "1"

test: _check && check-format
    cargo nextest run --all-features
    cargo +stable nextest run

coverage: _check
    cargo llvm-cov nextest --all-features

# Due to use of header arithmeitc, we cannot pass stacked borrows yet,
# only tree borrows
export MIRIFLAGS := "-Zmiri-tree-borrows -Zmiri-env-forward=RUST_BACKTRACE"

miri *args: _check && check-format
    cargo +nightly miri nextest run {{args}}

check: _check check-format

_check:
    cargo clippy --all-targets --all-features
    cargo +stable clippy --all-targets
    cargo doc --no-deps

format: && spellcheck
    cargo fmt --all
    taplo format
    cargo sort --grouped --workspace .

check-format: && spellcheck
    cargo fmt --check --all
    taplo format --check
    cargo sort --grouped --workspace --check .

spellcheck:
    @# use pinned version to avoid breaking build
    uvx typos@1.40
