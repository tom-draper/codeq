#!/usr/bin/env python3
"""Verify astgrep output parity, warm latency, and candidate source opens."""
import argparse
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import tempfile
from pathlib import Path

from query_latency import invoke


def source_opens(binary, root, command, expected):
    sources = {str(path) for path in root.glob("*.rs")}
    with tempfile.NamedTemporaryFile() as trace:
        result = subprocess.run(
            ["strace", "-f", "-qq", "-s", "512", "-e", "openat", "-o", trace.name,
             str(binary), *command], cwd=root, capture_output=True, check=True,
        )
        assert (result.stdout, result.stderr) == expected
        opened = re.findall(r'openat\([^,]+, "([^"]+)"[^\n]*= \d+', Path(trace.name).read_text())
    return sum(path in sources for path in opened)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.runs >= 5
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {
        "platform": platform.platform(), "cpus": os.cpu_count(), "load_before": os.getloadavg(),
        "runs": args.runs, "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                                                for key, path in binaries.items()},
        "state": "warm CLI queries; source tracing separate from timing", "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-astgrep-candidates-") as directory:
        root = Path(directory)
        filler = "// unrelated filler\n" * 8192
        for index in range(256):
            call = "selected_call" if index < 4 else "other_call"
            (root / f"file{index:03}.rs").write_text(filler + f"fn item{index}() {{ {call}(value); }}\n")
        cases = {
            "selective": (["selected_call($X)"], 4, {"before": 256, "after": 4}),
            "source": (["selected_call($X)", "--source", "--max-tokens", "4000"], 4, {"before": 260, "after": 8}),
            "scoped": (["selected_call($X)", "--path", "file000.rs"], 1, {"before": 1, "after": 1}),
            "empty": (["missing_call($X)"], 0, {"before": 256, "after": 0}),
            "empty_scope": (["selected_call($X)", "--path", "absent"], 0, {"before": 0, "after": 0}),
        }
        for name, (arguments, expected_rows, expected_opens) in cases.items():
            command = ["astgrep", *arguments, "--language", "rust", "--limit", "0"]
            for binary in binaries.values():
                invoke(binary, root, command)
            expected = invoke(binaries["before"], root, command)[1]
            for binary in binaries.values():
                assert invoke(binary, root, command)[1] == expected, name
                answer = json.loads(invoke(binary, root, ["--json", *command])[1][0])
                rows = answer["data"]["source" if name == "source" else "matches"]
                assert len(rows) == expected_rows, (name, rows)
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (name, key)
                    samples[key].append(elapsed)
            opens = {key: source_opens(binary, root, command, expected) for key, binary in binaries.items()}
            assert opens == expected_opens, (name, opens, expected_opens)
            report["workloads"][name] = {
                "command": command, "rows": expected_rows, "source_opens": opens,
                "samples_ms": samples,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
            }
        report["fixture"] = {"files": 256, "bytes": sum(path.stat().st_size for path in root.glob("*.rs"))}
    report["errors"] = report["answer_mismatches"] = 0
    report["load_after"] = os.getloadavg()
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: {"median_ms": value["median_ms"], "source_opens": value["source_opens"]}
                      for key, value in report["workloads"].items()}, indent=2))


if __name__ == "__main__":
    main()
