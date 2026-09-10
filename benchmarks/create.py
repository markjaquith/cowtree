#!/usr/bin/env python3
"""End-to-end creation comparison in a disposable, isolated Git repository."""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--files", type=int, default=20000)
    parser.add_argument("--payload-bytes", type=int, default=4096)
    parser.add_argument("--divergence", type=int, default=10, help="percentage of changed target files")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--temp-dir", type=Path)
    args = parser.parse_args()
    if args.files < 1 or args.payload_bytes < 1 or args.rounds < 1 or not 0 <= args.divergence < 100:
        parser.error("positive files/payload/rounds and divergence 0..99 required")
    binary = args.binary.resolve(strict=True)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("GIT_"):
            del env[key]
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null")

    def run(cwd, *command):
        result = subprocess.run(command, cwd=cwd, env=env, capture_output=True)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr.decode(errors='replace')}")
        return result.stdout

    with tempfile.TemporaryDirectory(prefix="cowtree-create-", dir=args.temp_dir) as directory:
        root = Path(directory)
        repo = root / "repo"
        repo.mkdir()
        run(repo, "git", "init", "-q", "--initial-branch=main")
        run(repo, "git", "config", "user.name", "cowtree benchmark")
        run(repo, "git", "config", "user.email", "benchmark@example.com")
        for i in range(args.files):
            path = repo / f"d{i // 100}" / f"file-{i}"
            path.parent.mkdir(exist_ok=True)
            path.write_bytes((f"{i}:".encode() + b"x" * args.payload_bytes)[:args.payload_bytes])
        run(repo, "git", "add", ".")
        run(repo, "git", "commit", "-qm", "source")
        run(repo, "git", "branch", "target")
        # Change main, keeping the target branch at the original commit. There
        # is one clean donor and no pre-existing target checkout in each trial.
        changed = args.files * args.divergence // 100
        if changed:
            for i in range(changed):
                (repo / f"d{i // 100}" / f"file-{i}").write_bytes(b"y" * args.payload_bytes)
            run(repo, "git", "commit", "-qam", "source divergence")
        timings = {"native": [], "native+compact": [], "clone-first": []}
        ready_timings = {method: [] for method in timings}
        for round_number in range(args.rounds):
            order = list(timings)
            if round_number % 2:
                order.reverse()
            for method in order:
                target = root / "trial"
                started = time.perf_counter()
                command = ["git", "worktree", "add", "--detach", str(target), "target"]
                if method == "clone-first":
                    command.insert(0, str(binary))
                run(repo, *command)
                if method == "native+compact":
                    run(repo, str(binary), "compact", str(target), "--source", "main", "--json")
                elapsed = time.perf_counter() - started
                timings[method].append(elapsed)
                if run(target, "git", "status", "--porcelain"):
                    raise RuntimeError("creation left a dirty worktree")
                ready_elapsed = time.perf_counter() - started
                ready_timings[method].append(ready_elapsed)
                record = {"method": method, "round": round_number + 1, "seconds": elapsed,
                          "including_first_status_seconds": ready_elapsed,
                          "files": args.files, "payload_bytes": args.payload_bytes,
                          "changed_files": changed}
                if method == "clone-first":
                    admin = Path(os.fsdecode(run(target, "git", "rev-parse", "--absolute-git-dir").rstrip(b"\n")))
                    receipt = json.loads((admin / "cowtree-creation").read_text())
                    record["cloned_files"] = receipt["cloned_files"]
                    if receipt["cloned_files"] != args.files - changed:
                        raise RuntimeError("unexpected clone count")
                print(json.dumps(record), flush=True)
                run(repo, "git", "worktree", "remove", str(target))
        print(json.dumps({"files": args.files, "payload_bytes": args.payload_bytes, "changed_files": changed,
                          "median_seconds": {key: statistics.median(values) for key, values in timings.items()},
                          "median_including_first_status_seconds": {key: statistics.median(values) for key, values in ready_timings.items()}}))


if __name__ == "__main__":
    main()
