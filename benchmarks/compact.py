"""Benchmark release binaries against a disposable APFS Git repository."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def positive_int(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def run(cwd, *args):
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"{' '.join(map(str, args))}:\n{result.stderr}\n{result.stdout}")
    return result.stdout


def benchmark(args, root):
    repo = root / "repo"
    repo.mkdir()
    run(repo, "git", "init", "--initial-branch=main")
    for key, value in [
        ("user.name", "cowtree benchmark"),
        ("user.email", "benchmark@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ]:
        run(repo, "git", "config", key, value)
    for index in range(args.files):
        parent = repo / f"dir-{index // 100:05}"
        for level in range(args.depth - 1):
            parent /= f"level-{level}"
        path = parent / f"file-{index:06}.txt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"file {index}\n" + "x" * args.payload_bytes)
    run(repo, "git", "add", ".")
    run(repo, "git", "commit", "-m", "benchmark fixture")
    for index in range(args.worktrees):
        run(
            repo, "wt", "--config", "/dev/null", "--config-set",
            'worktree-path = "../{{ branch }}"', "switch", "--create",
            f"target-{index}", "--base", "main", "--no-cd", "--no-hooks",
        )

    for trial in range(args.rounds):
        # Alternate order to reduce bias from always running one binary first.
        binaries = args.binaries if trial % 2 == 0 else reversed(args.binaries)
        for binary in binaries:
            # A previous clone changes inode/ctime while preserving contents.
            # Restore a clean checkout's index state for every timed invocation,
            # so later binaries don't pay for rehashing the earlier one's clones.
            for index in range(args.worktrees):
                run(root / f"target-{index}", "git", "-c", "core.trustctime=true",
                    "update-index", "--refresh")
            for receipt in (repo / ".git/worktrees").glob("*/cowtree-compaction"):
                receipt.unlink()
            timings = {}
            for phase in ["compact", "skip"]:
                start = time.perf_counter()
                result = json.loads(run(repo, str(binary), "compact", "--all", "--json"))
                timings[f"{phase}_seconds"] = time.perf_counter() - start
                compacted = args.worktrees if phase == "compact" else 0
                expected = {
                    "compacted": compacted,
                    "skipped": args.worktrees - compacted,
                    "failed": 0,
                }
                results = result["results"]
                if result["summary"] != expected or len(results) != args.worktrees:
                    raise RuntimeError(f"Unexpected batch result: {result}")
                expected_files = args.files if phase == "compact" else 0
                if any(item["cloned_files"] != expected_files for item in results):
                    raise RuntimeError(f"Unexpected clone counts: {result}")
            print(json.dumps({
                "binary": str(binary), "trial": trial, "files": args.files,
                "worktrees": args.worktrees, "depth": args.depth,
                "payload_bytes": args.payload_bytes, **timings,
            }), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", type=lambda path: Path(path).resolve())
    parser.add_argument("--files", type=positive_int, default=20000)
    parser.add_argument("--worktrees", type=positive_int, default=3)
    parser.add_argument("--rounds", type=positive_int, default=3)
    parser.add_argument("--depth", type=positive_int, default=1)
    parser.add_argument("--payload-bytes", type=positive_int, default=4096)
    parser.add_argument("--temp-dir", type=Path, help="parent directory on an APFS volume")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires macOS with APFS")
    for binary in args.binaries:
        if not binary.is_file():
            parser.error(f"binary not found: {binary}")
    with tempfile.TemporaryDirectory(prefix="cowtree-bench-", dir=args.temp_dir) as directory:
        benchmark(args, Path(directory))


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError) as error:
        sys.exit(str(error))
    except KeyboardInterrupt:
        sys.exit(130)
