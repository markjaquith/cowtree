# cowtree

`cowtree` creates space-efficient Git worktrees on macOS using APFS
copy-on-write clones. It can also compact existing worktrees, reducing their
physical storage by up to 95%.

> [!NOTE]
>
> Cowtree requires macOS and APFS. Linux users already have alternatives such as
> [Btrfs reflinks](https://btrfs.readthedocs.io/en/latest/Reflink.html).

## Install

```sh
brew install markjaquith/tap/cowtree
```

## Usage

```sh
# Create a copy-on-write worktree (use like `git worktree add`)
cowtree add ../feature -b feature

# Compact an existing worktree by branch name...
cowtree compact my-feature-branch

# ... or by worktree path
cowtree compact ../path/to/some-worktree

# Preview compaction
cowtree compact feature/my-branch --dry-run

# Compact every eligible worktree in the current repo
cowtree compact --all

# Show compaction status
cowtree status --all
```

Use `--json` with `compact` or `status` for machine-readable output.

## Creating worktrees

Use `cowtree add` anywhere you would use `git worktree add`. Cowtree registers
the worktree, clones verified files from existing worktrees on the same APFS
volume, then lets Git materialize the rest.

Git options, paths, sparse checkouts, filters, and `post-checkout` hooks are
preserved. If creation cannot finish safely, cowtree retains the worktree and
prints recovery instructions.

A nonempty checkout requires at least one eligible clone; cowtree does not fall
back to an ordinary checkout.

## Compacting worktrees

> [!WARNING]
>
> Run compaction only while both the source and target worktrees are idle. Do
> not edit files or run builds, formatters, or Git commands that may write to
> either worktree during compaction. Cowtree detects most concurrent changes and
> skips affected files, but a small unavoidable race window remains.

The source defaults to `origin/HEAD`, then `main`, then `master`. It must be
clean, checked out, and on the same APFS volume as the target.

`--all` skips worktrees with current compaction receipts. `--dry-run` reports
eligible files and attributed storage without modifying anything.

> [!NOTE]
>
> macOS reports each worktree as using its full size. Copy-on-write savings only
> appear in aggregate disk usage and available space.

See the [benchmark guide](benchmarks/README.md) for performance and storage
measurements.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
