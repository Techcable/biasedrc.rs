test: _check && check-format
    cargo nextest run

check: _check && check-format

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
