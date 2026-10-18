#!/usr/bin/env python3
"""Compare complete warm graph answers, latency and peak RSS on shared dependencies."""
import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
import tempfile
from pathlib import Path

from json_rendering import peak_memory
from query_latency import invoke


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs")
    binaries = {key: value.resolve() for key, value in {"before": args.before, "after": args.after}.items()}
    report = {
        "platform": platform.platform(), "load_before": os.getloadavg(), "runs": args.runs,
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "limits": "Whole warm CLI requests, including refresh, graph construction and rendering. Peak RSS measured separately. Only large unlimited scans without a trigram prefilter use workers; limited and selective queries remain serial.",
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "source_sha256": hashlib.sha256(Path("src/search.rs").read_bytes()).hexdigest(),
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-parallel-search-") as directory:
        root = Path(directory)
        wide = root / "wide"
        wide.mkdir()
        for i in range(16):
            (wide / f"file{i:02}.txt").write_text("needle needle needle needle\n" * 40_000)
        tiny = root / "tiny"
        tiny.mkdir()
        (tiny / "small.py").write_text("def example(): needle(); needle()\n")
        cases = {
            "broad_counts": (wide, ["search", "[a-z]+", "-c", "--limit", "0"], 16),
            "broad_capped_rows": (wide, ["search", "[a-z]+", "--limit", "0", "--max-per-file", "3"], 48),
            "inverted_counts": (wide, ["search", "missing", "-v", "-c", "--limit", "0"], 16),
            "limited": (wide, ["search", "[a-z]+", "--limit", "8"], 8),
            "selective": (wide, ["search", "needle", "-c", "--limit", "0"], 16),
            "tiny": (tiny, ["search", "[a-z]+", "-c", "--limit", "0"], 1),
        }
        for name, (repository, arguments, expected_rows) in cases.items():
            command = ["--json", *arguments]
            for binary in binaries.values():
                invoke(binary, repository, command)
            expected = invoke(binaries["before"], repository, command)[1]
            data = json.loads(expected[0])["data"]
            rows = data.get("counts", data.get("matches"))
            assert len(rows) == expected_rows, (name, len(rows), expected_rows)
            if name in ("broad_counts", "inverted_counts", "selective"):
                assert all(row["count"] == 40_000 for row in rows), name
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], repository, command)
                    assert output == expected, (name, key)
                    samples[key].append(elapsed)
            peak_rss = {}
            for key, binary in binaries.items():
                peak_rss[key], output = peak_memory(binary, repository, command)
                assert output == expected, (name, key)
            report["workloads"][name] = {
                "command": command, "rows": expected_rows,
                "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
                "samples_ms": samples, "peak_rss_kib": peak_rss,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            print(name, report["workloads"][name]["median_ms"], peak_rss, flush=True)
        command = ["--json", "search", "[a-z]+", "-c", "--limit", "0"]
        expected = invoke(binaries["before"], wide, command)[1]
        concurrent = {key: [] for key in binaries}
        with ThreadPoolExecutor(max_workers=4) as pool:
            for run in range(5):
                order = list(binaries) if run % 2 == 0 else list(reversed(binaries))
                for key in order:
                    started = time.perf_counter_ns()
                    results = list(pool.map(lambda _: invoke(binaries[key], wide, command), range(4)))
                    wall = (time.perf_counter_ns() - started) / 1e6
                    assert all(output == expected for _, output in results)
                    concurrent[key].append({"wall_ms": wall, "request_ms": [elapsed for elapsed, _ in results]})
        report["four_concurrent_queries"] = concurrent
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
