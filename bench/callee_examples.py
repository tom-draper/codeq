#!/usr/bin/env python3
"""Compare complete warm callee-example queries with exact output checks.

Usage: python3 bench/callee_examples.py --before /tmp/codeq-before \
    --after target/release/codeq --before-revision <commit>
"""
import argparse
import hashlib
import json
import os
import platform
import sqlite3
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

from query_latency import invoke

ROOT = Path(__file__).resolve().parent.parent


def fixture(root, distinct, calls, padding, early=False, split=False):
    root.mkdir()
    lines = []
    for i in range(distinct):
        lines.extend([f"def helper_{i}():", "    pass", ""])
    lines.append("def caller():")
    comments = ["    # café " + "x" * 120] * padding
    if not early:
        lines.extend(comments)
    expected = {}
    prefix_bytes = 0
    indexed_bytes = 0
    offset = sum(len(line.encode()) + 2 for line in lines)
    for i in range(calls):
        if split and i == calls // 2:
            lines.extend(["", "def caller():"])
            offset += len("\r\ndef caller():\r\n".encode())
        name = f"helper_{i % distinct}"
        if name not in expected:
            indexed_bytes = max(indexed_bytes, offset)
        expected.setdefault(name, len(lines) + 1)
        prefix_bytes += offset
        call = f"    {name}()"
        lines.append(call)
        offset += len(call.encode()) + 2
    if early:
        lines.extend(comments)
    body = "\r\n".join(lines) + "\r\n"
    source = root / "source.py"
    source.write_bytes(b"\xef\xbb\xbf" + body.encode())
    return expected, {
        "source_bytes": source.stat().st_size,
        "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "call_sites": calls,
        "distinct_callees": distinct,
        "same_named_definitions": 2 if split else 1,
        "old_newline_scan_bytes": prefix_bytes,
        "line_index_scan_bytes": indexed_bytes,
    }


def measure(name, root, command, binaries, runs, expected_lines=None, dimensions=None):
    for binary in binaries.values():
        invoke(binary, root, command)
    expected_output = invoke(binaries["before"], root, command)[1]
    assert expected_output[0], f"{name}: empty output"
    assert expected_output == invoke(binaries["after"], root, command)[1], name
    for binary in binaries.values():
        answer = json.loads(invoke(binary, root, ["--json", *command])[1][0])
        assert answer["data"], f"{name}: no callees"
        assert not answer.get("index", {}).get("reindexed_files", 0), name
        if expected_lines is not None:
            assert len(answer["data"]) == len(expected_lines), name
            observed = {row["name"]: row["call_line"] for row in answer["data"]}
            assert observed == expected_lines, (name, observed)
            assert all(row["call"] == row["name"] for row in answer["data"]), name
    if dimensions is not None:
        connection = sqlite3.connect(f"file:{root / '.codeq/index.db'}?mode=ro", uri=True)
        indexed_calls = connection.execute("SELECT COUNT(*) FROM calls").fetchone()[0]
        connection.close()
        assert indexed_calls == dimensions["call_sites"], (name, indexed_calls)
        dimensions["verified_indexed_calls"] = indexed_calls
    samples = {label: [] for label in binaries}
    for iteration in range(runs):
        order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
        for label in order:
            elapsed, output = invoke(binaries[label], root, command)
            assert output == expected_output, f"{name}/{label}: output changed"
            samples[label].append(elapsed)
    print(f"{name}: verified {runs} runs per binary", file=sys.stderr, flush=True)
    return {
        "task": name, "args": command, "dimensions": dimensions,
        "stdout_bytes": len(expected_output[0]),
        "answer_sha256": hashlib.sha256(b"\0".join(expected_output)).hexdigest(),
        "samples_ms": samples,
        "median_ms": {label: statistics.median(values) for label, values in samples.items()},
        "range_ms": {label: [min(values), max(values)] for label, values in samples.items()},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--before-revision", required=True)
    parser.add_argument("--runs", type=int, default=7)
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs per binary")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    load_before = os.getloadavg()
    results = []
    with tempfile.TemporaryDirectory(prefix="codeq-callee-examples-") as directory:
        for name, distinct, calls, padding, early, split in [
            ("repeated-callee", 1, 5000, 4096, False, False),
            ("distinct-callees", 1000, 1000, 4096, False, False),
            ("early-sparse-call", 1, 1, 4096, True, False),
            ("small-file", 1, 3, 0, False, False),
            ("multiple-definitions", 2, 2, 8, False, True),
        ]:
            root = Path(directory) / name
            expected, dimensions = fixture(root, distinct, calls, padding, early, split)
            results.append(measure(name, root, ["callees", "caller", "--examples", "--limit", "0"],
                                   binaries, args.runs, expected, dimensions))
            if calls >= 1000:
                results.append(measure(name + "-without-examples", root,
                                       ["callees", "caller", "--limit", "0"], binaries, args.runs))
    results.append(measure("repository-callees", ROOT,
                           ["callees", "scan_called_names", "--path", "src/commands/callgraph.rs", "--examples", "--limit", "0"],
                           binaries, args.runs))
    print(json.dumps({
        "metric": "end-to-end warm CLI wall time in milliseconds",
        "limits": "sequential warmed queries; synthetic call-heavy cases are not representative of every repository; excludes cold indexing and compilation",
        "before_revision": args.before_revision,
        "after_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "after_worktree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "after_source_sha256": hashlib.sha256((ROOT / "src/commands/callgraph.rs").read_bytes()).hexdigest(),
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "loadavg_before": load_before, "loadavg_after": os.getloadavg(),
        "binaries_sha256": {label: hashlib.sha256(binary.read_bytes()).hexdigest() for label, binary in binaries.items()},
        "errors": 0, "answer_mismatches": 0,
        "verified_timed_invocations": len(results) * args.runs * 2,
        "results": results,
    }, indent=2))


if __name__ == "__main__":
    main()
