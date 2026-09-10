# cowtree

`cowtree` is a Git worktree utility for macOS that uses copy-on-write (COW) to reduce the physical storage used by worktrees. For existing worktrees, it finds
tracked files that Git says are unchanged from a clean source worktree, clones
them with `clonefile(2)`, and atomically replaces the target copies. APFS shares
the cloned data until either copy is modified. You can also use `cowtree` to create new Git worktrees that will use copy-on-write from the start.

This can reduce the space that a worktree takes up on your disk by up to 95%.

## Usage

```sh
cowtree git worktree add ../feature -b feature
cowtree git worktree add --detach ../review HEAD~3
cowtree git -C /path/to/repo -c core.autocrlf=false worktree add ../feature
cowtree git worktree list --porcelain -z

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

## Clone-first creation

Prefix a Git worktree command with `cowtree`:

```sh
git worktree add ../feature -b feature
# becomes
cowtree git worktree add ../feature -b feature
```

For `add`, cowtree asks Git to register the worktree without checking out files,
clones verified regular files from registered worktrees on the same APFS volume,
then lets Git materialize the remaining paths.

All other commands (`list`, `move`, `remove`, `lock`, `unlock`, `prune`, `repair`,
and help) run directly through Git with its output and exit status. Git global
options such as repeated `-C`, `-c`, `--config-env`, and repository selectors are
supported. Arguments and paths are passed without shell expansion or UTF-8
conversion. Unknown creation options that cannot safely be interpreted produce
an explicit error rather than an ordinary checkout.

Git handles branch naming, refs, tracking, force checks, and registration.
Cowtree supports detached donors and independently verified clean files in dirty
donors. Donors at the target commit are preferred, followed by the invoking
worktree and other registered paths. No default branch or `--source` argument is
required. Source payloads are hashed through the same open file descriptor used
for cloning, with identity checks spanning hashing and cloning; source index
trust flags cannot authorize dirty bytes.

Checkout filters, encodings, line-ending conversions, `ident`, symlinks, and
gitlinks are handled by Git or its native non-recursive checkout semantics.
Sparse omissions remain absent. Files with source-specific extended attributes
or file flags are materialized by Git; macOS's system-managed provenance
attribute is allowed. Clones use target checkout modes and umask and inherit the
destination directory's ACL, rather than the donor's ACL or privileged owner.

**No ordinary-checkout fallback:** a nonempty checkout requires at least one
verified clone on the destination APFS volume. Otherwise creation fails.
Changed and ineligible files are still legitimately materialized by Git.
Explicit `--no-checkout`, orphan worktrees, empty commits, and empty sparse
selections retain their intentionally empty Git behavior without needing a
donor or APFS. These exceptions do not create a sharing receipt.

The usual `post-checkout` hook runs once after validation, with Git's arguments
and target working directory. Hook failure retains the completed worktree and
returns the hook's status. `--quiet` suppresses creation progress, and requested
worktree locks/reasons are preserved. Intercepted creation progress is not
byte-for-byte identical to native Git.

Creation temporarily locks its registration. Early failures clean up only
verified invocation-owned state; an originally empty destination directory is
restored. Filter/checkout failures, interruption during materialization, or
external interference retain the incomplete worktree and print its path and
recovery guidance. Git-created/reset branch refs can remain after failure, just
as registration and branch creation are separate native operations. No rollback
resets branches or runs destructive checkout recovery. Avoid concurrent writers
in the destination: filesystem name checks and subsequent operations still have
unavoidable final race windows.

## Compacting existing worktrees

With `--all`, `cowtree` keeps track of which worktrees have been compacted and
skips those whose receipts are still current.

For `compact` and `estimate`, the source defaults to the checked-out branch named
by `origin/HEAD` (the remote's default branch), then `main`, then `master`. It must
be clean and its HEAD must equal the selected branch tip. Source and target must
be on the same APFS volume. `cowtree` never falls back to a full copy.

## Large checkouts

Large worktrees are created and compacted with up to four clone workers, bounded by
available CPU parallelism and the number of eligible files. Small worktrees use
the serial path to avoid thread startup overhead. `--all` processes one worktree
at a time and skips targets with current receipts.

Path parsing avoids copying Git's exclusion list, and validated parent
directories are cached during eligibility scanning. Cloning retains per-file
race checks; all workers finish before final validation and receipt creation.

See [the benchmark guide](benchmarks/README.md) for reproducible large-checkout
and receipt-skip timings.

## Compaction safety model

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

Status values are `compacted`, `created`, `stale`, `not_compacted`, `invalid`, and
`unknown`:

- `compacted`: a successful run was recorded and both commits are still current
- `created`: clone-first creation was recorded and its target commit is current
- `stale`: a valid receipt exists, but its source or target commit changed
- `not_compacted`: no receipt exists
- `invalid`: a receipt is malformed or uses an unsupported version
- `unknown`: Git or worktree state could not be inspected

With no argument, `status` checks the current worktree. It also accepts a branch
or registered worktree path; `--all` checks every registered worktree. Status is
fast because it compares receipt and commit metadata rather than scanning files.
APFS has no supported API to prove that files still share extents after a
content-preserving rewrite, so `compacted` does not guarantee current sharing.

Clone-first creation records a separate versioned JSON receipt at
`<per-worktree Git admin directory>/cowtree-creation`, containing immutable source
commit IDs, the target commit, and the count of clones still intact after the
hook. It stays meaningful after donor branch deletion or donor removal. Failed
hooks do not produce a creation receipt. Receipt-writing failures produce a
warning without changing a successfully completed checkout's exit status.
Creation receipts do not suppress a
later explicit compaction or `compact --all`: compaction can have a different
donor and candidate set. A later compaction receipt takes precedence in status.
Like `compacted`, `created` attests to the operation, not current shared extents.

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

## Release

After configuring the `CARGO_REGISTRY_TOKEN` GitHub Actions secret, run:

```sh
scripts/release
```

Choose a major, minor, or patch release when prompted. Alternatively, specify it
directly with `scripts/release patch`. The script updates the Cargo version,
runs the release checks, commits, tags, and pushes. The tag-triggered release
workflow publishes the crate and creates the GitHub release.
