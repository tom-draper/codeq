#!/usr/bin/env python3
"""Run verified differential workloads and summarize empirical tail latency."""
import argparse
import hashlib
import json
import math
import platform
import statistics
import subprocess
import sys
from pathlib import Path


SUITES = ["parallel_search", "astgrep_patterns", "caller_frontiers", "callee_cache",
          "context_lookups", "structured_json", "index_queue", "relation_inserts", "mcp_sequences", "scale_queries"]


def summarize(samples):
    if not samples or any(not math.isfinite(value) or value < 0 for value in samples):
        raise ValueError("latency samples must be nonempty, finite and nonnegative")
    values = sorted(samples)
    return {"n": len(values), "median_ms": statistics.median(values),
            "p95_ms": values[math.ceil(len(values) * .95) - 1],
            "p99_ms": values[math.ceil(len(values) * .99) - 1], "max_ms": values[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--suite", action="append", choices=SUITES)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--max-regression-percent", type=float,
                        help="Optional median gate for a stable runner; no default timing gate")
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs")
    threshold = args.max_regression_percent
    if threshold is not None and (not math.isfinite(threshold) or threshold < 0):
        parser.error("threshold must be finite and nonnegative")
    binaries = {"before": args.before.resolve(strict=True), "after": args.after.resolve(strict=True)}
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parent.parent
    selected = list(dict.fromkeys(args.suite or SUITES))
    report = {"revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
              "platform": platform.platform(), "runs": args.runs,
              "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
              "source_sha256": {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
                                for path in [Path(__file__), *(root / "bench" / f"{suite}.py" for suite in selected)]},
              "limits": "Nearest-rank empirical percentiles, including transport for MCP. "
                        "With fewer than 100 samples p99 is the observed maximum. "
                        "These are descriptive samples, not population tail estimates. "
                        "Child reports retain memory samples, row counts, parity checks and measurement limits.",
              "workloads": {}, "failures": [], "regressions": []}
    for suite in selected:
        output = directory / f"{suite}.json"
        # Never accept a previous run's report after a failed child invocation.
        output.unlink(missing_ok=True)
        command = [sys.executable, str(root / "bench" / f"{suite}.py"),
                   str(binaries["before"]), str(binaries["after"]),
                   "--runs", str(args.runs), "--output", str(output)]
        try:
            result = subprocess.run(command, cwd=root, timeout=1800)
        except (OSError, subprocess.TimeoutExpired) as error:
            report["failures"].append({"suite": suite, "error": str(error)})
            continue
        if result.returncode:
            report["failures"].append({"suite": suite, "returncode": result.returncode})
            continue
        try:
            data = json.loads(output.read_text())
            if data["errors"] != 0 or data["answer_mismatches"] != 0 or not data["workloads"]:
                raise ValueError("child reported errors, mismatches or no workloads")
            for name, row in data["workloads"].items():
                samples = row["samples_ms"]
                summary = {key: summarize(samples[key]) for key in binaries}
                report["workloads"][f"{suite}/{name}"] = summary
                if threshold is not None and summary["after"]["median_ms"] > summary["before"]["median_ms"] * (1 + threshold / 100):
                    report["regressions"].append(f"{suite}/{name}")
        except (OSError, KeyError, ValueError, TypeError) as error:
            report["failures"].append({"suite": suite, "error": str(error)})
    for path, digest in report["source_sha256"].items():
        if hashlib.sha256((root / path).read_bytes()).hexdigest() != digest:
            report["failures"].append({"source": path, "error": "source changed during measurement"})
    (directory / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"{len(report['workloads'])} workloads, {len(report['failures'])} failures, {len(report['regressions'])} regressions")
    if report["failures"] or report["regressions"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
