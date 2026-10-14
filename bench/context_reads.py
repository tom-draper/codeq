#!/usr/bin/env python3
"""Compare complete warm context queries and optionally trace source reads."""
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


def trace_sources(binary, root, command, expected):
    sources = {str(path) for path in root.rglob("*.py")} if root != ROOT else {
        str(path) for path in (ROOT / "src").rglob("*.rs")
    }
    with tempfile.NamedTemporaryFile() as trace:
        result = subprocess.run(
            ["strace", "-qq", "-s", "512", "-e", "trace=openat,read,close", "-o", trace.name,
             str(binary), *command], cwd=root, capture_output=True, check=True,
        )
        assert (result.stdout, result.stderr) == expected, "traced output changed"
        lines = Path(trace.name).read_text().splitlines()
    descriptors = {}
    opens = 0
    body_bytes = 0
    for line in lines:
        opened = re.match(r'openat\([^,]+, "([^"]+)".*\) += (\d+)$', line)
        if opened:
            path, descriptor = opened.groups()
            descriptor = int(descriptor)
            descriptors.pop(descriptor, None)
            if path in sources:
                descriptors[descriptor] = path
                opens += 1
        read = re.match(r'read\((\d+),.*\) += (\d+)$', line)
        if read and int(read[1]) in descriptors:
            body_bytes += int(read[2])
        closed = re.match(r'close\((\d+)\) += 0$', line)
        if closed:
            descriptors.pop(int(closed[1]), None)
    return {"source_opens": opens, "source_bytes_read": body_bytes}


def measure(name, root, command, binaries, runs, trace, verify, expected_opens):
    for binary in binaries.values():
        invoke(binary, root, command)
    expected = invoke(binaries["before"], root, command)[1]
    assert expected[0], f"{name}: empty output"
    for binary in binaries.values():
        assert invoke(binary, root, command)[1] == expected, name
        answer = json.loads(invoke(binary, root, ["--json", *command])[1][0])
        assert not answer.get("index", {}).get("reindexed_files", 0), name
        verify(answer["data"])
    samples = {label: [] for label in binaries}
    for iteration in range(runs):
        order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
        for label in order:
            elapsed, output = invoke(binaries[label], root, command)
            assert output == expected, f"{name}/{label}: output changed"
            samples[label].append(elapsed)
    traces = None
    if trace:
        traces = {label: trace_sources(binary, root, command, expected)
                  for label, binary in binaries.items()}
        for label, count in expected_opens.items():
            assert traces[label]["source_opens"] == count, (name, label, traces[label])
    print(f"{name}: verified {runs} runs per binary", file=sys.stderr, flush=True)
    return {
        "task": name, "args": command, "source_trace": traces,
        "stdout_bytes": len(expected[0]),
        "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
        "samples_ms": samples,
        "median_ms": {label: statistics.median(values) for label, values in samples.items()},
        "range_ms": {label: [min(values), max(values)] for label, values in samples.items()},
    }


def verify_seeds(data, count, text):
    assert len(data["chunks"]) == count
    assert all(chunk["name"] == "seed" and chunk["reason"] == "seed" for chunk in data["chunks"])
    assert all(("text" in chunk) == text for chunk in data["chunks"])
    assert not data.get("omitted")
    assert data["budget"]["used_tokens"] <= data["budget"]["max_tokens"]


def verify_omitted(data, text):
    verify = data["chunks"]
    assert len(verify) == 1 and verify[0]["name"] == "seed"
    assert ("text" in verify[0]) == text
    assert len(data["omitted"]) == 50 and data["omitted_truncated"]
    assert all(item["name"].startswith("helper_") for item in data["omitted"])


def verify_reduced(data):
    assert len(data["chunks"]) == 1
    assert data["chunks"][0]["name"] == "Seed"
    assert data["chunks"][0]["reduced"] == "folded"
    assert "text" not in data["chunks"][0]
    assert data["budget"]["used_tokens"] <= 80


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--before-revision", required=True)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--trace", action="store_true", help="verify source opens with strace outside timed runs")
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs per binary")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    results = []
    load_before = os.getloadavg()
    with tempfile.TemporaryDirectory(prefix="codeq-context-reads-") as directory:
        base = Path(directory)
        many, small, expanded, reduced = [base / name for name in ("many", "small", "expanded", "reduced")]
        for root in (many, small, expanded, reduced):
            root.mkdir()
        for i in range(128):
            (many / f"seed_{i:03}.py").write_text("#" + "x" * (512 * 1024) + "\ndef seed():\n    return 42\n")
        (small / "seed.py").write_text("def seed():\n    return 42\n")
        (expanded / "seed.py").write_text("def seed():\n" + "".join(f"    helper_{i:03}()\n" for i in range(64)))
        for i in range(64):
            (expanded / f"helper_{i:03}.py").write_text(f"def helper_{i:03}():\n    #" + "x" * (256 * 1024) + "\n    return 1\n")
        (reduced / "seed.py").write_text("\ufeffclass Seed:\n    def work(self):\n" + "        # café 🐍\n" * 128 + "        return 1\n")
        tasks = [
            ("metadata-many-seeds", many, ["context", "--symbols", "seed", "--no-text"], lambda d: verify_seeds(d, 128, False), {"before": 128, "after": 0}),
            ("text-many-seeds", many, ["context", "--symbols", "seed"], lambda d: verify_seeds(d, 128, True), {"before": 128, "after": 128}),
            ("metadata-small-seed", small, ["context", "--symbols", "seed", "--no-text"], lambda d: verify_seeds(d, 1, False), {"before": 1, "after": 0}),
            ("omitted-neighbors-text", expanded, ["context", "--symbols", "seed", "--expand", "--max-tokens", "4000"], lambda d: verify_omitted(d, True), {"before": 65, "after": 1}),
            ("omitted-neighbors-metadata", expanded, ["context", "--symbols", "seed", "--expand", "--max-tokens", "4000", "--no-text"], lambda d: verify_omitted(d, False), {"before": 65, "after": 1}),
            ("reduced-seed-metadata", reduced, ["context", "--symbols", "Seed", "--max-tokens", "80", "--no-text"], verify_reduced, {"before": 1, "after": 1}),
        ]
        for name, root, command, verify, opens in tasks:
            result = measure(name, root, command, binaries, args.runs, args.trace, verify, opens)
            result["fixture_files"] = len(list(root.glob("*.py")))
            result["fixture_bytes"] = sum(path.stat().st_size for path in root.glob("*.py"))
            results.append(result)
    command = ["context", "--symbols", "build", "--path", "src/context.rs", "--no-text"]
    def verify_repo(data):
        assert len(data["chunks"]) == 1 and data["chunks"][0]["name"] == "build"
        assert "text" not in data["chunks"][0]
        assert "reduced" not in data["chunks"][0]
    results.append(measure("repository-metadata", ROOT, command, binaries, args.runs,
                           args.trace, verify_repo, {"before": 1, "after": 0}))
    print(json.dumps({
        "metric": "end-to-end warm CLI wall time in milliseconds",
        "limits": "sequential warmed queries; excludes cold indexing and compilation; source traces run separately without timing",
        "before_revision": args.before_revision,
        "after_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "after_worktree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "after_source_sha256": hashlib.sha256((ROOT / "src/context.rs").read_bytes()).hexdigest(),
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "loadavg_before": load_before, "loadavg_after": os.getloadavg(),
        "binaries_sha256": {label: hashlib.sha256(binary.read_bytes()).hexdigest() for label, binary in binaries.items()},
        "errors": 0, "answer_mismatches": 0,
        "verified_timed_invocations": len(results) * args.runs * 2, "results": results,
    }, indent=2))


if __name__ == "__main__":
    main()
