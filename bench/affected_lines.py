#!/usr/bin/env python3
"""Compare warm affected queries with repeated names and shared definition files."""
import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
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
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {
        "platform": platform.platform(), "runs": args.runs, "load_before": os.getloadavg(),
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "binary_sha256": {k: hashlib.sha256(p.read_bytes()).hexdigest() for k, p in binaries.items()},
        "source_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in ("src/commands/callgraph.rs", "bench/affected_lines.py")},
        "limits": "Warm whole CLI queries, including freshness checks and rendering. Filesystem caches warm. Repeated alternating runs; peak RSS measured separately. Exact stdout and stderr compared.",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-affected-lines-") as directory:
        for kind, count in [("repeated_names", 1200), ("shared_file", 1200), ("small", 3)]:
            root = Path(directory) / kind
            root.mkdir()
            (root / "leaf.py").write_text("def leaf(): return 1\n")
            if kind == "shared_file":
                (root / "parents.py").write_text("".join(f"def parent{i}(): leaf()\n" for i in range(count)))
            else:
                for i in range(count):
                    (root / f"parent{i:04}.py").write_text("def run(): leaf()\n")
            git = ["git", "-c", "user.name=bench", "-c", "user.email=bench@example.test", "-c", "commit.gpgsign=false"]
            for command in [["init", "-q", "-b", "main"], ["add", "."], ["commit", "-qm", "fixture"]]:
                subprocess.run([*git, *command], cwd=root, check=True)
            invoke(binaries["before"], root, ["index"])
            (root / "leaf.py").write_text("def leaf(): return 2\n")
            command = ["--json", "affected", "--compare-to", "HEAD", "--depth", "1", "--limit", "0"]
            for binary in binaries.values():
                invoke(binary, root, command)
            expected = invoke(binaries["before"], root, command)[1]
            data = json.loads(expected[0])["data"]
            assert len(data["changed"]) == 1 and len(data["impacted"]) == count
            assert all(row["line"] == (i + 1 if kind == "shared_file" else 1) for i, row in enumerate(data["impacted"]))
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                for key in (list(binaries) if iteration % 2 == 0 else list(reversed(binaries))):
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (kind, key)
                    samples[key].append(elapsed)
            rss = {}
            for key, binary in binaries.items():
                rss[key], output = peak_memory(binary, root, command)
                assert output == expected, (kind, key)
            report["workloads"][kind] = {
                "command": command, "impacted_rows": count, "samples_ms": samples,
                "median_ms": {k: statistics.median(v) for k, v in samples.items()}, "peak_rss_kib": rss,
                "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
            }
            print(kind, report["workloads"][kind]["median_ms"], flush=True)
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
