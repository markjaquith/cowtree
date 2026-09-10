# Contributing

## Setup

Install [mise](https://mise.jdx.dev/getting-started.html), then install the
project tools and enable the Git hooks:

```sh
mise install
mise exec -- hk install --mise
```

The pre-commit hook checks Rust and Markdown formatting. To format Markdown
files, run:

```sh
mise exec -- hk fix --all --step prettier
```

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
