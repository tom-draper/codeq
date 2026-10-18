#!/usr/bin/env python3
"""Compare complete warm graph answers, latency and peak RSS on shared dependencies."""
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
    binaries = {key: value.resolve() for key, value in {"before": args.before, "after": args.after}.items()}
    report = {
        "platform": platform.platform(), "load_before": os.getloadavg(), "runs": args.runs,
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "limits": "Whole warm CLI requests, including refresh, graph construction and rendering. Peak RSS measured separately. Batches share file bodies, definitions, line indexes and syntax spans; aliases keep the existing lookup path.",
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "source_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in
                          ("src/search.rs", "src/commands/callgraph.rs", "src/commands/testing.rs")},
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-caller-frontiers-") as directory:
        root = Path(directory)
        wide = root / "wide"
        wide.mkdir()
        width = 120
        middle = "".join(f"def mid{i:03}(): leaf()\n" for i in range(width))
        (wide / "graph.py").write_text("def leaf(): return 1\n" + middle)
        for i in range(80):
            (wide / f"parent{i:03}.py").write_text(f"def parent{i:03}():\n"
                + "".join(f"    mid{j:03}()\n" for j in range(width))
                + "    # mid000 mid001\n    text = 'mid002'\n")
        (wide / "test_alias.py").write_text("from graph import mid000 as chosen\ndef test_alias(): chosen()\n")
        (wide / "notes.txt").write_text("mid000 mid001 mid002\n")
        git = ["git", "-c", "user.name=bench", "-c", "user.email=bench@example.test", "-c", "commit.gpgsign=false"]
        subprocess.run([*git, "init", "-q", "-b", "main"], cwd=wide, check=True)
        subprocess.run([*git, "add", "."], cwd=wide, check=True)
        subprocess.run([*git, "commit", "-qm", "fixture"], cwd=wide, check=True)
        invoke(binaries["before"], wide, ["index"])
        (wide / "graph.py").write_text("def leaf(): return 2\n" + middle)
        cycle = root / "cycle"
        cycle.mkdir()
        (cycle / "graph.py").write_text(
            "def leaf(): top()\ndef a(): leaf()\ndef b(): leaf()\n"
            "def top(): a(); b()\n")
        capped = root / "capped"
        capped.mkdir()
        (capped / "graph.py").write_text("def leaf(): pass\n"
            + "".join(f"def item{i:04}(): leaf()\n" for i in range(2100)))
        cases = {
            "wide": (wide, ["callers", "leaf", "--depth", "3", "--limit", "0"], 202),
            "direct": (wide, ["callers", "leaf", "--limit", "0"], 120),
            "scoped": (wide, ["callers", "leaf", "--depth", "3", "--path", "graph.py", "--limit", "0"], 120),
            "limited": (wide, ["callers", "leaf", "--depth", "3", "--limit", "8"], 8),
            "affected": (wide, ["affected", "--compare-to", "HEAD", "--depth", "3"], 201),
            "cycle": (cycle, ["callers", "leaf", "--depth", "6", "--limit", "0"], 4),
            "hop_cap": (capped, ["callers", "leaf", "--depth", "3", "--limit", "0"], 2000),
        }
        for name, (repository, arguments, expected_rows) in cases.items():
            command = ["--json", *arguments]
            for binary in binaries.values():
                invoke(binary, repository, command)
            expected = invoke(binaries["before"], repository, command)[1]
            data = json.loads(expected[0])["data"]
            rows = data["impacted"] if name == "affected" else data
            assert len(rows) == expected_rows, (name, len(rows), expected_rows)
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
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
