#!/usr/bin/env python3
"""Compare concurrent edited and fresh queries, tracing directory walks separately."""
import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path


def batch(binary, root, workers, expected, changed, traces=None):
    gate = threading.Barrier(workers)
    command = [str(binary), "--json", "--meta", "--root", str(root), "definitions", "edited"]

    def query(index):
        args = command
        if traces is not None:
            args = ["strace", "-qq", "-yy", "-s", "512", "-e", "getdents64,newfstatat,statx",
                    "-o", str(traces / f"{index}.trace"), *command]
        gate.wait()
        start = time.perf_counter_ns()
        result = subprocess.run(args, capture_output=True, check=True)
        elapsed = (time.perf_counter_ns() - start) / 1e6
        assert not result.stderr, result.stderr
        answer = json.loads(result.stdout)
        assert answer["data"] == expected and answer["index"]["fresh"]
        return elapsed, answer["index"].get("reindexed_files", 0)

    start = time.perf_counter_ns()
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        answers = list(pool.map(query, range(workers)))
    elapsed = (time.perf_counter_ns() - start) / 1e6
    assert sum(reindexed for _, reindexed in answers) == int(changed), answers
    return elapsed, [latency for latency, _ in answers]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.runs >= 5 and args.workers >= 2
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {"platform": platform.platform(), "cpus": os.cpu_count(), "load_before": os.getloadavg(),
              "runs": args.runs, "workers": args.workers,
              "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                                for key, path in binaries.items()}, "workloads": {}}
    with tempfile.TemporaryDirectory(prefix="codeq-concurrent-refresh-") as directory:
        base = Path(directory)
        root = base / "repo"
        root.mkdir()
        source_dirs = set()
        for index in range(32):
            folder = root / "src" / f"group{index:02}"
            folder.mkdir(parents=True)
            source_dirs.add(str(folder))
            for file in range(128):
                (folder / f"file{file:03}.py").write_text(f"def item{index}_{file}(): pass\n")
        revision = 0
        padding = "#" + "x" * (2 * 1024 * 1024) + "\n"

        def edit():
            nonlocal revision
            revision ^= 1
            (root / "change.py").write_text(padding + f"def edited(): return {revision}\n")

        edit()
        expected = None
        for binary in binaries.values():
            result = subprocess.run([str(binary), "--json", "--root", str(root), "definitions", "edited"], capture_output=True, check=True)
            data = json.loads(result.stdout)["data"]
            assert len(data) == 1 and data[0]["name"] == "edited"
            if expected is None:
                expected = data
            assert data == expected
        for changed in [True, False]:
            samples = {key: [] for key in binaries}
            queries = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    if changed:
                        edit()
                    elapsed, latencies = batch(binaries[key], root, args.workers, expected, changed)
                    samples[key].append(elapsed)
                    queries[key].extend(latencies)
            counters = {}
            for key, binary in binaries.items():
                traces = base / (key + str(changed))
                traces.mkdir()
                if changed:
                    edit()
                batch(binary, root, args.workers, expected, changed, traces)
                reads = stats = 0
                for trace in traces.glob("*.trace"):
                    for line in trace.read_text().splitlines():
                        match = re.match(r'getdents64\(\d+<([^>]+)>.*= (\d+)$', line)
                        if match and match[1] in source_dirs and int(match[2]) > 0:
                            reads += 1
                        if (line.startswith("statx(") or line.startswith("newfstatat(")) and str(root) in line:
                            stats += 1
                assert reads % len(source_dirs) == 0
                counters[key] = {"full_walks": reads // len(source_dirs), "source_directory_reads": reads,
                                 "repository_metadata_calls": stats}
            assert counters["after"]["full_walks"] == int(changed), counters
            if changed:
                assert counters["before"]["full_walks"] >= 2, counters
            else:
                assert counters["before"]["full_walks"] == 0, counters
            name = "edited" if changed else "fresh"
            report["workloads"][name] = {
                "batch_samples_ms": samples, "query_samples_ms": queries, "traces_untimed": counters,
                "median_batch_ms": {key: statistics.median(values) for key, values in samples.items()},
                "range_batch_ms": {key: [min(values), max(values)] for key, values in samples.items()},
            }
            print(f"{name}: {report['workloads'][name]['median_batch_ms']}; {counters}", flush=True)
        report["fixture"] = {"files": 4097, "source_directories": 32}
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
