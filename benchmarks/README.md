# Compaction benchmark

Requires macOS/APFS, Python 3, Git, and Worktrunk (`wt`, used only to create the
disposable benchmark worktrees). Build release binaries before measuring:

```sh
cargo build --release
python3 benchmarks/compact.py target/release/cowtree
```

For a larger checkout or a before/after comparison:

```sh
python3 benchmarks/compact.py --files 100000 --rounds 3 /path/to/baseline target/release/cowtree
```

The default fixture has 20,000 distinct files of roughly 4 KiB, distributed across
directories of 100 files, and three linked worktrees. Use `--worktrees` to change
the target count and `--temp-dir` to choose a temporary directory on APFS.

Each round refreshes the fixture's target indexes outside the timed region.
Cloning changes inode/ctime, so this prevents later binaries from being penalized
by Git rehashing the previous run's clones. Every run starts with the index state
of a clean checkout.

Each round forces `compact --all --json` by removing only the fixture's receipts,
then times another invocation that should skip every target. Clone counts and
summary counts must match expectations. JSON lines report whole-command wall
times, including Git inspection and receipt writes; fixture setup and cleanup
are outside the timed region. The entire disposable repository is removed on exit.

Multiple binaries share the same fixture, with execution order reversed every
other round. These are repeated-compaction measurements with warm caches and
potentially already-shared extents, not cold-disk measurements or estimates of
physical savings. Compare medians on an otherwise idle machine and record the
filesystem, hardware, fixture size, and number of rounds with results.

## Reference measurement

On an Apple M1 Max (10 logical CPUs, 64 GiB RAM), macOS 15.4.1, APFS:

| Version | Compact 100,000 files × 3 worktrees | Skip all 3 worktrees |
| --- | ---: | ---: |
| Serial baseline (`f96c12a`) | 106.28 s | 0.144 s |
| Bounded four-worker cloning and allocation/metadata optimizations | 67.41 s | 0.152 s |

This was one round per binary with fixture indexes refreshed before each timed
run: about 37% less compaction time (1.58× throughput). It is a local measurement,
not a cross-machine guarantee. Earlier runs without index refresh were not
comparable: the later binary paid for Git rehashing the preceding run's clones.
