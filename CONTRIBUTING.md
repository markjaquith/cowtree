# Contributing

## Setup

Install [hk](https://hk.jdx.dev/getting_started.html) and enable the Git hooks
after cloning:

```sh
hk install
```

The pre-commit hook runs `cargo fmt --check`. If it fails, run `cargo fmt` and
stage the formatting changes before committing again.

## Build and test

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

APFS integration tests skip on other platforms and filesystems.

## Release

After configuring the `CARGO_REGISTRY_TOKEN` GitHub Actions secret, run:

```sh
scripts/release
```

Choose a major, minor, or patch release when prompted. Alternatively, specify it
directly with `scripts/release patch`. The script updates the Cargo version,
runs the release checks, commits, tags, and pushes. The tag-triggered release
workflow publishes the crate and creates the GitHub release.
