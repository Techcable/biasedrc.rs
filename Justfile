export RUST_BACKTRACE := "1"

test: _check && check-format
    cargo nextest run

# Due to use of header arithmeitc, we cannot pass stacked borrows yet,
# only tree borrows
export MIRIFLAGS := "-Zmiri-tree-borrows -Zmiri-env-forward=RUST_BACKTRACE"

miri *args: _check && check-format
    cargo +nightly miri nextest run {{args}}

check: _check check-format

_check:
    cargo clippy --all-targets
    cargo +stable clippy --all-targets

format: && spellcheck
    cargo fmt --all
    taplo format

check-format: && spellcheck
    cargo fmt --check --all
    taplo format --check

spellcheck:
    @# use pinned version to avoid breaking build
    uvx typos@1.40
