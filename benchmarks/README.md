# Compaction benchmark

## Clone-first creation benchmark

On macOS/APFS with Python 3 and Git, build a release binary and run:

```sh
cargo build --release
python3 benchmarks/create.py target/release/cowtree --rounds 3
python3 benchmarks/create.py target/release/cowtree --divergence 0 --files 100000
python3 benchmarks/create.py target/release/cowtree --files 1000 --payload-bytes 1048576
python3 benchmarks/create.py candidate/cowtree --baseline baseline/cowtree
```

This separate benchmark compares native `git worktree add`, native add followed
by compaction, and `cowtree add`. It uses disposable native Git fixtures,
isolated Git configuration, a clean same-volume donor, and a fresh destination
for every trial. All creation, inventory, payload verification, cloning,
checkout, final index validation, and receipt work is timed. The script also
reports time including the first `git status`: compaction replaces files without
refreshing the index, so command time alone defers some work to the next Git
invocation. Repository setup and cleanup are outside the timed region. Trial
order alternates between rounds; output includes individual timings, medians,
and verified clone counts.

`--divergence` is the percentage of files requiring Git materialization (default
10). Use `--temp-dir` to choose the APFS volume. These are warm-cache
measurements and do not measure device writes or prove unique physical savings.
In particular, `du` attributes shared blocks to both clones. Record hardware,
filesystem, Git version, and repeated timings; source hashing and filesystem
metadata operations can outweigh avoided writes on some checkout shapes.

For opt-in phase timings from a real creation, set `COWTREE_TIMING=1`:

```sh
COWTREE_TIMING=1 cowtree add ../feature -b feature
```

Timing lines go to stderr and include worktree discovery and registration, index
preparation, attribute and donor inventories, clone planning, clone-phase wall
time, aggregate donor-hashing worker time, Git materialization, index refresh,
validation, hooks, and receipt finalization. Aggregate hashing time is summed
across clone workers and can therefore exceed clone-phase wall time.

### Creation reference measurements: September 10, 2026

Apple M1 Max (10 logical CPUs, 64 GiB RAM), macOS 15.4.1, APFS, Git 2.45.0;
three rounds per shape, alternating trial order. All expected clone counts were
verified. The creation path uses up to four clone workers.

Median whole-command times:

| Fixture                          | Cloned files | Native add | Add + compact | Clone-first add |
| -------------------------------- | -----------: | ---------: | ------------: | --------------: |
| 20,000 × 4 KiB, 10% divergent    |       18,000 |     1.83 s |        5.66 s |          4.30 s |
| 20,000 × 4 KiB, identical commit |       20,000 |     1.88 s |        5.98 s |          4.52 s |
| 1,000 × 1 MiB, 10% divergent     |          900 |     0.76 s |        3.46 s |          4.61 s |

Median elapsed times including the first `git status`:

| Fixture                          | Native add | Add + compact | Clone-first add |
| -------------------------------- | ---------: | ------------: | --------------: |
| 20,000 × 4 KiB, 10% divergent    |     2.13 s |        6.74 s |          4.36 s |
| 20,000 × 4 KiB, identical commit |     2.04 s |        7.21 s |          4.58 s |
| 1,000 × 1 MiB, 10% divergent     |     2.24 s |        7.34 s |          4.62 s |

Clone-first creation was about 24% faster than add-plus-compact for the
small-file command timings. For the large-file shape its command took about 33%
longer, but including the first status it was about 37% faster. Compaction
changes file identities without refreshing the target index; clone-first
creation performs that validation before returning. Native Git can also rehash
newly checked-out files when their index timestamps are racy. Both timings
matter when comparing the workflows. Native add alone was fastest in these
fixtures, but provides no COW sharing guarantee.

These local measurements establish neither a universal speedup nor physical
bytes saved. Donor verification reads file contents, and all methods pay Git and
filesystem metadata costs. The architectural difference is that cloned paths
avoid an initial ordinary checkout write and a later replacement pass.

## Existing-worktree compaction benchmark

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

The default fixture has 20,000 distinct files of roughly 4 KiB, distributed
across directories of 100 files, and three linked worktrees. Use `--worktrees`
to change the target count and `--temp-dir` to choose a temporary directory on
APFS. Use `--depth` (default 1) to vary directory nesting and `--payload-bytes`
(default 4096) to vary file size. Each file also contains a unique short header.

```sh
python3 benchmarks/compact.py --files 20000 --depth 8 --payload-bytes 128 /path/to/baseline target/release/cowtree
```

Each round refreshes the fixture's target indexes outside the timed region.
Cloning changes inode/ctime, so this prevents later binaries from being
penalized by Git rehashing the previous run's clones. Every run starts with the
index state of a clean checkout.

Each round forces `compact --all --json` by removing only the fixture's
receipts, then times another invocation that should skip every target. Clone
counts and summary counts must match expectations. JSON lines report
whole-command wall times, including Git inspection and receipt writes; fixture
setup and cleanup are outside the timed region. The entire disposable repository
is removed on exit.

Multiple binaries share the same fixture, with execution order reversed every
other round. These are repeated-compaction measurements with warm caches and
potentially already-shared extents, not cold-disk measurements or estimates of
physical savings. Compare medians on an otherwise idle machine and record the
filesystem, hardware, fixture size, and number of rounds with results.

## Reference measurement

On an Apple M1 Max (10 logical CPUs, 64 GiB RAM), macOS 15.4.1, APFS:

| Version                                                           | Compact 100,000 files × 3 worktrees | Skip all 3 worktrees |
| ----------------------------------------------------------------- | ----------------------------------: | -------------------: |
| Serial baseline (`f96c12a`)                                       |                            106.28 s |              0.144 s |
| Bounded four-worker cloning and allocation/metadata optimizations |                             67.41 s |              0.152 s |

This was one round per binary with fixture indexes refreshed before each timed
run: about 37% less compaction time (1.58× throughput). It is a local
measurement, not a cross-machine guarantee. Earlier runs without index refresh
were not comparable: the later binary paid for Git rehashing the preceding run's
clones.

## Follow-up experiments: September 10, 2026

**Decision: retain the runtime implementation at `f218d0b`.** The attempted
scan/clone fusion did not demonstrate a repeatable large-checkout speedup. These
experiments used the same M1 Max/APFS setup, refreshed indexes, three rounds per
binary, and alternating binary order. Each comparison used a new fixture;
compare paired results rather than timings across separate experiments.

For 100,000 files per target, three targets, approximately 4 KiB per file:

| Experiment                                                 | Baseline median | Experimental median | Decision                         |
| ---------------------------------------------------------- | --------------: | ------------------: | -------------------------------- |
| Four-worker metadata scan, separate clone pass             |         72.45 s |             71.58 s | Too small and inconsistent       |
| Fused metadata scan/cloning, four workers, first version   |         66.32 s |             63.25 s | Promising; required confirmation |
| Fused scan/cloning, four workers, reduced path allocations |         66.58 s |             66.44 s | Effectively tied                 |
| Fused scan/cloning, eight workers                          |         67.44 s |             91.41 s | Substantial regression           |
| Fused scan/cloning, two workers                            |         66.52 s |             68.09 s | Mixed results, worse median      |

The final four-worker prototype's individual large-checkout results were:

| Round | Baseline | Prototype |
| ----- | -------: | --------: |
| 1     |  68.16 s |   66.21 s |
| 2     |  66.58 s |   67.41 s |
| 3     |  65.58 s |   66.44 s |

On a second shape (20,000 files per target, eight directory levels, 128-byte
payloads), the reduced-allocation four-worker prototype improved the median from
13.07 s to 12.58 s. This narrower improvement was insufficient to justify the
production refactor given the larger-checkout results.

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

Compared with `fa6d9a9` on the same M1 Max/APFS setup, with refreshed indexes
and three alternating rounds (roughly 4 KiB per file):

| Fixture                            | Baseline median | Directory-relative median |
| ---------------------------------- | --------------: | ------------------------: |
| 20,000 files × 3 targets, depth 8  |         13.34 s |                   13.28 s |
| 100,000 files × 3 targets, depth 1 |         70.89 s |                   71.00 s |

Large-fixture individual times were baseline `[70.89, 71.74, 70.44]` seconds and
directory-relative `[71.00, 71.68, 68.15]` seconds. Both comparisons were
effectively tied. Correctness tests passed, but reducing full-path resolution
did not translate into a demonstrated wall-clock improvement.

The implementation and its specific tests were removed; this record is retained
to avoid repeating the experiment without new evidence. The experimental code
remains available in commit `4ca4fca`.

## Rejected recursive clone-and-reset experiment: September 10, 2026

**Decision: do not replace clone-first creation with recursive cloning followed
by `git reset --hard`.** It was slower on the representative small-file shape,
even when the donor and target commits were identical.

The prototype registered a no-checkout worktree, used macOS `cp -cR` to clone
the donor hierarchy except `.git`, then ran `git reset --hard` in the target.
One version used a single recursive clone; another split the top-level entries
across four concurrent recursive clones. Both produced clean worktrees.

On an Apple M4 Pro, macOS 26.6.2, APFS, and Git 2.55.0, with 20,000 identical
4 KiB files and three alternating rounds:

| Method                              | Median command time |
| ----------------------------------- | ------------------: |
| Existing four-worker clone-first add |              5.13 s |
| Four concurrent recursive clones + reset |          6.14 s |

The concurrent recursive approach was about 20% slower. A separate serial run
measured 7.47 s versus 5.05 s for clone-first. With 10% commit divergence, the
serial approach measured 7.57 s versus 4.88 s. A 1,000-file, two-round trial
favored recursive clone-and-reset (0.40 s versus 0.54 s), but that narrower
shape did not justify a more complex fast path that also needs special handling
for untracked files, ignored files, metadata, sparse checkouts, filters, and
submodules.

Recursive cloning still traverses every entry, does so serially within each
hierarchy, and then makes Git traverse the result again during reset. Splitting
top-level directories recovered some time but did not outperform the existing
parallel per-file implementation.

## Rejected clone-then-verify experiment: September 10, 2026

**Decision: retain pre-clone donor hashing.** Removing it produced only a
negligible small-file improvement and a modest large-file improvement, which
did not justify the additional mismatch-recovery path.

The prototype cloned candidate files without first hashing their contents, kept
the existing source and destination identity checks, and relied on
`git update-index --refresh` to verify the resulting clones. If refresh found a
mismatch, Cowtree identified the changed clones, removed them, materialized
those paths from Git, refreshed the index again, and excluded the replacements
from the creation receipt. Existing dirty-donor, race, sparse-checkout, filter,
SHA-256, and receipt tests passed, as did Clippy.

On an Apple M4 Pro, macOS 26.6.2, APFS, and Git 2.55.0, candidate and baseline
binaries ran against the same disposable fixture in alternating order for
three rounds:

| Fixture                          | Baseline median | Clone-then-verify median | Improvement |
| -------------------------------- | --------------: | -----------------------: | ----------: |
| 20,000 × 4 KiB, 10% divergent    |          4.94 s |                   4.88 s |        1.2% |
| 1,000 × 1 MiB, 10% divergent     |          2.98 s |                   2.73 s |        8.2% |
| 1,000 × 1 MiB, identical commit  |          3.09 s |                   2.90 s |        6.2% |

Git's mandatory destination verification remains the dominant content pass.
Avoiding the donor hash helps more for large payloads, but it does not address
the metadata and per-file cloning costs that dominate large file-count
worktrees. The prototype was removed.

## Unsafe creation performance ceiling: September 10, 2026

An intentionally unsafe prototype established the available optimization
ceiling by removing donor hashing, descriptor pinning, metadata screening,
permission and timestamp normalization, and source/target race checks. It used
direct path-based `clonefile`, while retaining Cowtree's inventories, four clone
workers, Git index refresh, final status validation, and receipt work. This path
was benchmark-only and was removed after measurement.

On an Apple M4 Pro, macOS 26.6.2, APFS, and Git 2.55.0, unsafe and baseline
binaries ran against the same disposable fixture in alternating order for
three rounds:

| Fixture                       | Baseline median | Unsafe ceiling median | Maximum improvement |
| ----------------------------- | --------------: | --------------------: | ------------------: |
| 20,000 × 4 KiB, 10% divergent |          5.05 s |                3.71 s |               26.6% |
| 1,000 × 1 MiB, 10% divergent  |          3.00 s |                2.68 s |               10.6% |

The ceiling is meaningful for high file counts but not for large payloads.
Future work should target the per-file safety and metadata operations as a
group. Hash elimination alone cannot approach the small-file ceiling, and the
unsafe implementation is not suitable for production.
