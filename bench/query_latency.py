#!/usr/bin/env python3
"""Compare complete warm CLI queries, alternating two release binaries.

No downloads or index rebuilds. Every timed invocation must succeed and return
exactly the same stdout and stderr as both untimed warmups. Includes subprocess
startup, repository discovery, freshness checking, query work and rendering.
"""
import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TASKS = [
    ("repo-totals", ROOT, ["map", "--totals"]),
    ("repo-search", ROOT, ["search", "Session", "--path", "src", "--limit", "10"]),
    ("book-totals", ROOT / "bench/.corpus/rust-book", ["map", "--totals"]),
    ("book-explore", ROOT / "bench/.corpus/rust-book", ["explore", "--at", "listings/ch06-enums-and-pattern-matching/listing-06-07/src/main.rs:10", "--max-tokens", "200"]),
    ("redis-explore", ROOT / "bench/.corpus/redis", ["explore", "dictAdd", "--path", "src/dict.c", "--max-tokens", "200"]),
    ("flask-explore", ROOT / "bench/.corpus/flask", ["explore", "run", "--path", "src/flask/app.py", "--max-tokens", "200"]),
]


def invoke(binary, root, args):
    start = time.perf_counter_ns()
    result = subprocess.run([str(binary), *args], cwd=root, capture_output=True, check=True)
    elapsed = (time.perf_counter_ns() - start) / 1e6
    return elapsed, (result.stdout, result.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, default=ROOT / "bench/.corpus")
    parser.add_argument("--runs", type=int, default=7)
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs per binary")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    load_before = os.getloadavg()
    results = []
    for name, root, command in TASKS:
        root_label = str(root.relative_to(ROOT))
        if root != ROOT:
            root = args.corpus.resolve() / root.name
        # Warm both the index and OS caches before recording samples. An initial
        # refresh note is excluded along with its untimed invocation.
        for binary in binaries.values():
            invoke(binary, root, command)
        expected = invoke(binaries["before"], root, command)[1]
        assert expected == invoke(binaries["after"], root, command)[1], name
        assert expected[0], f"{name}: empty answer"
        assert b"no symbol named" not in expected[1], name
        if command[0] == "explore":
            assert b"(focus" in expected[0], f"{name}: no focus source returned"
        samples = {label: [] for label in binaries}
        for iteration in range(args.runs):
            order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
            for label in order:
                elapsed, output = invoke(binaries[label], root, command)
                assert output == expected, f"{name}/{label}: answer changed"
                samples[label].append(elapsed)
        results.append({
            "task": name, "root": root_label, "args": command,
            "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
            "stdout_bytes": len(expected[0]), "stderr_bytes": len(expected[1]),
            "answer_sha256": hashlib.sha256(expected[0] + expected[1]).hexdigest(),
            "samples_ms": samples,
            "median_ms": {label: statistics.median(values) for label, values in samples.items()},
            "range_ms": {label: [min(values), max(values)] for label, values in samples.items()},
        })
        print(f"{name}: complete identical answers, {args.runs} runs per binary", flush=True, file=sys.stderr)
    print(json.dumps({
        "metric": "end-to-end warm CLI wall time in milliseconds",
        "limits": "sequential warmed queries on cached corpus checkouts; excludes cold indexing and model reasoning",
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "loadavg_before": load_before, "loadavg_after": os.getloadavg(),
        "binaries_sha256": {label: hashlib.sha256(binary.read_bytes()).hexdigest() for label, binary in binaries.items()},
        "errors": 0, "answer_mismatches": 0, "results": results,
        "verified_timed_invocations": sum(len(values) for row in results for values in row["samples_ms"].values()),
    }, indent=2))


if __name__ == "__main__":
    main()
