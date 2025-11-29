_default:
  just --list


format: spellcheck
    cargo fmt --all
    taplo format

spellcheck:
    @# use pinned version to avoid breaking build
    uvx typos@1.40
