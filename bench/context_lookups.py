#!/usr/bin/env python3
"""Compare source assembly for dense seeds, many files, and one sparse seed."""
import argparse
import hashlib
import json
import os
import platform
import statistics
import tempfile
from pathlib import Path

from query_latency import invoke


def measure(binaries, root, command, runs):
    for binary in binaries.values():
        invoke(binary, root, command)
    expected = invoke(binaries["before"], root, command)[1]
    data = json.loads(invoke(binaries["before"], root, ["--json", *command])[1][0])["data"]
    assert expected[0] and data["source"]
    for binary in binaries.values():
        assert json.loads(invoke(binary, root, ["--json", *command])[1][0])["data"] == data
    samples = {key: [] for key in binaries}
    for iteration in range(runs):
        order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
        for key in order:
            elapsed, output = invoke(binaries[key], root, command)
            assert output == expected
            samples[key].append(elapsed)
    return {
        "command": command, "samples_ms": samples, "source_chunks": len(data["source"]),
        "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
        "median_ms": {key: statistics.median(values) for key, values in samples.items()},
        "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.runs >= 5
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {"platform": platform.platform(), "cpus": os.cpu_count(), "load_before": os.getloadavg(),
              "runs": args.runs, "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                                                      for key, path in binaries.items()}, "workloads": {}}
    with tempfile.TemporaryDirectory(prefix="codeq-context-lookups-") as directory:
        base = Path(directory)
        for name in ["dense_bytes", "dense_names", "many_files", "sparse"]:
            root = base / name
            root.mkdir()
            if name == "dense_bytes":
                (root / "source.py").write_text("".join(f"def item{i:04}(): selected_call(value)\n" for i in range(1500)))
                command = ["astgrep", "selected_call($X)", "--language", "python", "--source", "--max-tokens", "80", "--limit", "0"]
            elif name == "dense_names":
                (root / "source.py").write_text("def target(): pass\n" + "".join(f"def item{i:04}(): target()\n" for i in range(1500)))
                command = ["explore", "target", "--max-tokens", "80", "--max-response-tokens", "1000000"]
            elif name == "many_files":
                (root / "target.py").write_text("def target(): pass\n")
                for i in range(128):
                    (root / f"item{i:03}.py").write_text(f"def item{i:03}(): target()\n")
                command = ["explore", "target", "--max-tokens", "80", "--max-response-tokens", "1000000"]
            else:
                (root / "source.py").write_text("".join(f"def item{i:04}(): other()\n" for i in range(5000)) + "def selected(): unique_marker()\n")
                command = ["search", "unique_marker", "--source", "--max-tokens", "80"]
            result = measure(binaries, root, command, args.runs)
            data = json.loads(invoke(binaries["after"], root, ["--json", *command])[1][0])["data"]
            if name == "dense_bytes":
                assert len(data["symbols"]) == 1500
            elif name in ("dense_names", "many_files"):
                assert len(data["callers"]) == (1500 if name == "dense_names" else 128)
            else:
                assert [row["name"] for row in data["source"]] == ["selected"]
            result["files"] = len(list(root.glob("*.py")))
            report["workloads"][name] = result
            print(f"{name}: {result['median_ms']}", flush=True)
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
