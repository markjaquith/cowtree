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
Use `--depth` (default 1) to vary directory nesting and `--payload-bytes`
(default 4096) to vary file size. Each file also contains a unique short header.

```sh
python3 benchmarks/compact.py --files 20000 --depth 8 --payload-bytes 128 /path/to/baseline target/release/cowtree
```

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

## Follow-up experiments: September 10, 2026

**Decision: retain the runtime implementation at `f218d0b`.** The attempted
scan/clone fusion did not demonstrate a repeatable large-checkout speedup.
These experiments used the same M1 Max/APFS setup, refreshed indexes, three
rounds per binary, and alternating binary order. Each comparison used a new
fixture; compare paired results rather than timings across separate experiments.

For 100,000 files per target, three targets, approximately 4 KiB per file:

| Experiment | Baseline median | Experimental median | Decision |
| --- | ---: | ---: | --- |
| Four-worker metadata scan, separate clone pass | 72.45 s | 71.58 s | Too small and inconsistent |
| Fused metadata scan/cloning, four workers, first version | 66.32 s | 63.25 s | Promising; required confirmation |
| Fused scan/cloning, four workers, reduced path allocations | 66.58 s | 66.44 s | Effectively tied |
| Fused scan/cloning, eight workers | 67.44 s | 91.41 s | Substantial regression |
| Fused scan/cloning, two workers | 66.52 s | 68.09 s | Mixed results, worse median |

The final four-worker prototype's individual large-checkout results were:

| Round | Baseline | Prototype |
| --- | ---: | ---: |
| 1 | 68.16 s | 66.21 s |
| 2 | 66.58 s | 67.41 s |
| 3 | 65.58 s | 66.44 s |

On a second shape (20,000 files per target, eight directory levels, 128-byte
payloads), the reduced-allocation four-worker prototype improved the median
from 13.07 s to 12.58 s. This narrower improvement was insufficient to justify
the production refactor given the larger-checkout results.

Fusion reused freshly inspected eligibility metadata as the clone's initial
identity snapshot, retaining post-clone source and target identity checks and
performing ancestor validation before any replacements. It removed two metadata
reads per eligible file, but syscall-count reduction alone did not predict
whole-command performance. Prototype tests for edits after the snapshot and
worker error propagation passed. The runtime prototypes and their specific tests
were removed rather than shipping an unproven optimization; the fixture-shape
options remain for future measurements.

These results do not establish a filesystem performance ceiling. In particular,
directory-relative clone/stat/rename operations and source-inventory reuse were
not tested in this experiment. Eight workers clearly lost in the tested fused
implementation; this does not establish an optimum for every implementation or
machine.

## Rejected directory-relative cloning experiment: September 10, 2026

**Decision: reverted `4ca4fca`. We tried directory-relative cloning and gained
no measurable performance benefit.** The additional cache management,
file-descriptor lifetimes, and libc code were not justified by the results.

The experiment cached one source/target parent-directory handle pair per worker
and used `fstatat`, `clonefileat`, `fchmodat`, `utimensat`, `renameat`, and
`unlinkat`. Final source/target identity checks still resolved the original full
paths to detect replaced parents. The worker count and eligibility scan were
unchanged.

Compared with `fa6d9a9` on the same M1 Max/APFS setup, with refreshed indexes and
three alternating rounds (roughly 4 KiB per file):

| Fixture | Baseline median | Directory-relative median |
| --- | ---: | ---: |
| 20,000 files × 3 targets, depth 8 | 13.34 s | 13.28 s |
| 100,000 files × 3 targets, depth 1 | 70.89 s | 71.00 s |

Large-fixture individual times were baseline `[70.89, 71.74, 70.44]` seconds and
directory-relative `[71.00, 71.68, 68.15]` seconds. Both comparisons were
effectively tied. Correctness tests passed, but reducing full-path resolution
did not translate into a demonstrated wall-clock improvement.

The implementation and its specific tests were removed; this record is retained
to avoid repeating the experiment without new evidence. The experimental code
remains available in commit `4ca4fca`.
