# cowtree

`cowtree` reduces the physical storage used by Git worktrees on APFS. It finds
tracked files that Git says are unchanged from a clean source worktree, clones
them with `clonefile(2)`, and atomically replaces the target copies. APFS shares
the cloned data until either copy is modified.

`cowtree` is standalone and does not depend on Worktrunk. The installed binary
does not need a Rust or C compiler.

## Usage

```sh
cowtree compact feature/my-branch
cowtree compact /path/to/detached-worktree --source main
cowtree compact --all
cowtree compact --all --dry-run

cowtree estimate feature/my-branch
cowtree estimate --all --json

cowtree status
cowtree status feature/my-branch
cowtree status --all --json
```

With `--all`, `cowtree` keeps track of which worktrees have been compacted and
skips those whose receipts are still current.

The source defaults to the checked-out branch named by `origin/HEAD` (the
remote's default branch), then `main`, then `master`. It must be clean and its
HEAD must equal the selected branch tip. Source and target must be on the same
APFS volume. `cowtree` never falls back to a full copy.

## Large checkouts

Large worktrees are compacted with up to four parallel workers, bounded by
available CPU parallelism and the number of eligible files. Small worktrees use
the serial path to avoid thread startup overhead. `--all` processes one worktree
at a time and skips targets with current receipts.

Path parsing avoids copying Git's exclusion list, and validated parent
directories are cached during eligibility scanning. Cloning retains per-file
race checks; all workers finish before final validation and receipt creation.

See [the benchmark guide](benchmarks/README.md) for reproducible large-checkout
and receipt-skip timings.

## Safety model

Git is the authority for tracked paths and checkout state. For each target,
`cowtree` asks Git for every path that differs from the source commit, including
committed divergence, staged changes, and unstaged changes. Those paths are
excluded. Untracked and ignored files are never candidates. Symlinks,
submodules, sparse-checkout omissions, and special files are skipped.

Before replacing a file, `cowtree` checks source and target device, inode, size,
mtime, and ctime. It clones to a temporary sibling, checks the source again,
restores target mode and timestamps, checks the target again, then renames the
clone over the target. A detected race skips that path. Clone errors fail the
target; they never trigger a byte-copy fallback. `cowtree` never runs `reset`,
`clean`, or `checkout` as recovery.

There is an unavoidable final race between the target check and rename. Run
compaction on idle worktrees, especially when editors, builds, or Git commands
may be writing files. Interruptions can leave `.cowtree-clone.*` siblings for
files active at that instant, but no completion receipt is written.

## Receipts and status

The atomic, versioned receipt is stored at:

```text
<absolute per-worktree Git admin directory>/cowtree-compaction
```

It follows a linked worktree through branch rename and `git worktree move`, and
is removed naturally with the worktree administration directory. Older receipt
formats are recognized and silently upgraded when they are still current.

Status values are `compacted`, `stale`, `not_compacted`, `invalid`, and
`unknown`:

- `compacted`: a successful run was recorded and both commits are still current
- `stale`: a valid receipt exists, but its source or target commit changed
- `not_compacted`: no receipt exists
- `invalid`: a receipt is malformed or uses an unsupported version
- `unknown`: Git or worktree state could not be inspected

With no argument, `status` checks the current worktree. It also accepts a branch
or registered worktree path; `--all` checks every registered worktree. Status is
fast because it compares receipt and commit metadata rather than scanning files.
APFS has no supported API to prove that files still share extents after a
content-preserving rewrite, so `compacted` does not guarantee current sharing.

## Savings estimates

`estimate` reports eligible logical bytes and attributed allocated bytes. The
latter is an upper bound on incremental physical savings: files might already
share extents, snapshots can retain old extents, and APFS metadata has overhead.
Compare volume free space before and after for coarse validation. `du` continues
to attribute shared blocks to every clone, so it does not report unique physical
usage.

## Build and test

Install [hk](https://hk.jdx.dev/getting_started.html) and enable the Git hooks
after cloning:

```sh
hk install
```

The pre-commit hook runs `cargo fmt --check`. If it fails, run `cargo fmt`
and stage the formatting changes before committing again.

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

APFS integration tests skip on other platforms and filesystems.
