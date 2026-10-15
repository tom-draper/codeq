#!/usr/bin/env python3
"""Compare warm API diffs and verify Git subprocess counts outside timing."""
import argparse
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

from query_latency import invoke

ROOT = Path(__file__).resolve().parent.parent


def trace_git(binary, root, command, expected):
    with tempfile.NamedTemporaryFile() as trace:
        result = subprocess.run(
            ["strace", "-f", "-qq", "-s", "4096", "-e", "trace=execve", "-o", trace.name,
             str(binary), *command], cwd=root, capture_output=True, check=True)
        assert (result.stdout, result.stderr) == expected
        lines = Path(trace.name).read_text().splitlines()
    calls = [line for line in lines if re.search(r'execve\("[^" ]*/git",', line)
             and line.endswith("= 0")]
    return {name: sum(f'"{name}"' in line for line in calls)
            for name in ("merge-base", "show", "cat-file")}


def measure(name, root, command, binaries, runs, trace, count):
    for binary in binaries.values():
        invoke(binary, root, command)
    expected = invoke(binaries["before"], root, command)[1]
    for binary in binaries.values():
        assert invoke(binary, root, command)[1] == expected
        answer = json.loads(invoke(binary, root, ["--json", *command])[1][0])
        assert not answer.get("index", {}).get("reindexed_files", 0)
        data = answer["data"]
        assert len(data["changed"]) == count, data
        assert {r["name"] for r in data["changed"]} == ({"shape"} if count else set())
        assert all("a: u32" in r["was"] and "a: u64" in r["signature"] for r in data["changed"])
        assert not data["added"] and not data["removed"], data
    samples = {label: [] for label in binaries}
    for iteration in range(runs):
        order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
        for label in order:
            elapsed, output = invoke(binaries[label], root, command)
            assert output == expected, (name, label)
            samples[label].append(elapsed)
    traces = None
    if trace:
        traces = {label: trace_git(binary, root, command, expected)
                  for label, binary in binaries.items()}
        assert traces["before"] == {"merge-base": count + 1, "show": count, "cat-file": 0}, traces
        assert traces["after"] == {"merge-base": 1, "show": 0, "cat-file": int(count > 0)}, traces
    print(f"{name}: verified {runs} runs per binary", file=sys.stderr, flush=True)
    return {"task": name, "args": command, "changed_exports": count,
            "git_trace": traces, "samples_ms": samples,
            "median_ms": {k: statistics.median(v) for k, v in samples.items()},
            "range_ms": {k: [min(v), max(v)] for k, v in samples.items()},
            "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--before-revision", required=True)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--trace", action="store_true")
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs per binary")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    load = os.getloadavg()
    results = []
    with tempfile.TemporaryDirectory(prefix="codeq-api-git-") as directory:
        for size in (1, 128):
            root = Path(directory) / str(size)
            root.mkdir()
            def git(*command):
                subprocess.run(["git", "-C", str(root), "-c", "user.name=test",
                                "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false",
                                *command], check=True, capture_output=True)
            git("init", "-q")
            for i in range(size):
                (root / f"file_{i:03}.rs").write_text("pub fn shape(a: u32) {}\n")
            git("add", "-A")
            git("commit", "-qm", "initial")
            for path in root.glob("*.rs"):
                path.write_text("pub fn shape(a: u64) {}\n")
            tasks = [(f"changed-{size}", [], size)]
            if size > 1:
                tasks.extend([("scoped-one", ["file_000.rs"], 1),
                              ("empty-scope", ["missing"], 0)])
            for name, scope, count in tasks:
                results.append(measure(name, root, ["api", *scope, "--compare-to", "HEAD", "--limit", "0"],
                                       binaries, args.runs, args.trace, count))
    print(json.dumps({
        "metric": "end-to-end warm CLI wall time in milliseconds",
        "limits": "sequential synthetic Rust queries with warm index and filesystem; excludes compilation and cold indexing; traces are untimed",
        "before_revision": args.before_revision,
        "after_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "after_worktree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "source_sha256": {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest()
                          for p in ("src/repo.rs", "src/commands/symbols.rs", "src/commands/project.rs")},
        "binaries_sha256": {k: hashlib.sha256(v.read_bytes()).hexdigest() for k, v in binaries.items()},
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "loadavg_before": load, "loadavg_after": os.getloadavg(),
        "errors": 0, "answer_mismatches": 0,
        "verified_timed_invocations": len(results) * args.runs * 2, "results": results,
    }, indent=2))


if __name__ == "__main__":
    main()
