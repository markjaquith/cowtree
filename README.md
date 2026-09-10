# cowtree

`cowtree` is a Git worktree utility for macOS that uses copy-on-write (COW) to reduce the physical storage used by worktrees. For existing worktrees, it finds
tracked files that Git says are unchanged from a clean source worktree, clones
them with `clonefile(2)`, and atomically replaces the target copies. APFS shares
the cloned data until either copy is modified. You can also use `cowtree` to create new Git worktrees that will use copy-on-write from the start.

This can reduce the space that a worktree takes up on your disk by up to 95%.

> [!NOTE]
> Cowtree is only for macOS systems using APFS. Linux users already have better
> options, such as [Btrfs reflinks](https://btrfs.readthedocs.io/en/latest/Reflink.html),
> for efficient copy-on-write file copies.

## Usage

```sh
# Just like `git worktree add`
cowtree add ../feature -b feature

# For compacting existing worktrees
cowtree compact feature/my-branch
cowtree compact /path/to/detached-worktree --source main
cowtree compact --all
cowtree compact --all --dry-run

# Estimate the space savings of compaction
cowtree estimate feature/my-branch
cowtree estimate --all --json

# Get the compaction status
cowtree status
cowtree status feature/my-branch
cowtree status --all --json
```

## Clone-first creation

Replace `git worktree add` with `cowtree add`:

```sh
git worktree add ../feature -b feature
# becomes
cowtree add ../feature -b feature
```

For `add`, cowtree asks Git to register the worktree without checking out files,
clones verified regular files from registered worktrees on the same APFS volume,
then lets Git materialize the remaining paths.

Run other worktree commands directly through Git. `cowtree add` accepts the
options and paths supported by `git worktree add`, without shell expansion or
UTF-8 conversion. Unknown creation options that cannot safely be interpreted
will produce an error.

**No ordinary-checkout fallback:** a nonempty checkout requires at least one
verified clone on the destination APFS volume.

The usual `post-checkout` hook still runs. If creation cannot finish safely,
cowtree retains the incomplete worktree and prints recovery instructions rather
than running destructive Git commands.

## Compacting existing worktrees

With `--all`, `cowtree` keeps track of which worktrees have been compacted and
skips those whose receipts are still current.

For `compact` and `estimate`, the source defaults to the checked-out branch named
by `origin/HEAD` (the remote's default branch), then `main`, then `master`. It must
be clean and its HEAD must equal the selected branch tip. Source and target must
be on the same APFS volume.

See [the benchmark guide](benchmarks/README.md) for reproducible large-checkout
and receipt-skip timings.

> [!WARNING]
> Run compaction only while both the source and target worktrees are idle. Do not
> edit files or run builds, formatters, or Git commands that may write to either
> worktree during compaction. Cowtree detects most concurrent changes and skips
> affected files, but a small unavoidable race window remains.

## Receipts and status

Status values are `compacted`, `created`, `stale`, `not_compacted`, `invalid`, and
`unknown`:

- `compacted`: a successful run was recorded and both commits are still current
- `created`: clone-first creation was recorded and its target commit is current
- `stale`: a valid receipt exists, but its source or target commit changed
- `not_compacted`: no receipt exists
- `invalid`: a receipt is malformed or uses an unsupported version
- `unknown`: Git or worktree state could not be inspected

With no argument, `status` checks the current worktree. It also accepts a branch
or registered worktree path; `--all` checks every registered worktree. Receipts
follow worktree moves and branch renames. Status records what cowtree completed,
but APFS cannot guarantee that files still share extents after later changes.

## Savings estimates

`estimate` reports eligible logical bytes and attributed allocated bytes. The
latter is an upper bound; existing shared extents, snapshots, and APFS overhead
can make actual savings differ. `du` does not report unique usage for shared
blocks.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build, test, contribution, and release
documentation.
