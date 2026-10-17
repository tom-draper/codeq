#!/usr/bin/env python3
"""Compare complete warm graph answers, latency and peak RSS on shared dependencies."""
import argparse
import hashlib
import json
import os
import platform
import statistics
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
        "limits": "Whole warm CLI requests, including refresh, graph construction and rendering. Peak RSS measured separately. SQL query reuse is checked by the separate resolver trace unit test.",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-callee-cache-") as directory:
        root = Path(directory)
        wide = root / "wide"
        wide.mkdir()
        width = 160
        leaves = "".join(f"def leaf{i:03}(): pass\n" for i in range(width))
        (wide / "duplicates.py").write_text(leaves)
        middle = "".join(
            f"def mid{i:03}():\n" + "".join(f"    leaf{j:03}()\n" for j in range(width))
            + "    external()\n" + ("    target()\n" if i == width - 1 else "")
            for i in range(width))
        (wide / "graph.py").write_text(leaves + middle + "def entry():\n"
            + "".join(f"    mid{i:03}()\n" for i in range(width))
            + "def target(): pass\ndef unreachable(): pass\n")
        (wide / "noise.py").write_text("def entry(): pass\n")
        cycle = root / "cycle"
        cycle.mkdir()
        (cycle / "graph.py").write_text(
            "def top(): a(); b()\ndef a(): shared()\ndef b(): shared()\n"
            "def shared(): top(); tail()\ndef tail(): finish()\ndef finish(): pass\n")
        capped = root / "capped"
        capped.mkdir()
        (capped / "graph.py").write_text("".join(f"def item{i:04}(): pass\n" for i in range(4200))
            + "def entry():\n" + "".join(f"    item{i:04}()\n" for i in range(4200)))
        cases = {
            "wide": (wide, ["callees", "entry", "--depth", "3", "--limit", "0"], 481),
            "early_path": (wide, ["callpath", "entry", "leaf159", "--max-depth", "2"], 3),
            "late_path": (wide, ["callpath", "entry", "target", "--max-depth", "2"], 3),
            "no_path": (wide, ["callpath", "entry", "unreachable", "--max-depth", "4"], 0),
            "scoped": (wide, ["callpath", "entry", "target", "--path", "graph.py", "--max-depth", "2"], 3),
            "limited": (wide, ["callees", "entry", "--depth", "3", "--limit", "8"], 8),
            "cycle": (cycle, ["callees", "top", "--depth", "6", "--limit", "0"], 6),
            "cycle_path": (cycle, ["callpath", "top", "finish", "--max-depth", "6"], 5),
            "hop_cap": (capped, ["callees", "entry", "--depth", "2", "--limit", "0"], 2000),
        }
        for name, (repository, arguments, expected_rows) in cases.items():
            command = ["--json", *arguments]
            for binary in binaries.values():
                invoke(binary, repository, command)
            expected = invoke(binaries["before"], repository, command)[1]
            assert len(json.loads(expected[0])["data"]) == expected_rows, name
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
