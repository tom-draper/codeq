#!/usr/bin/env python3
"""Compare callee-query output, latency, and source reads for repeated targets."""
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


def trace(binary, root, command, expected):
    sources = {str(path) for path in root.glob("source.*")}
    with tempfile.NamedTemporaryFile() as output:
        result = subprocess.run(
            ["strace", "-qq", "-s", "512", "-e", "openat,read,close", "-o", output.name,
             str(binary), *command], cwd=root, capture_output=True, check=True,
        )
        assert (result.stdout, result.stderr) == expected
        descriptors, opens, read_bytes = set(), 0, 0
        for line in Path(output.name).read_text().splitlines():
            opened = re.match(r'openat\([^,]+, "([^"]+)".*= (\d+)$', line)
            if opened:
                path, descriptor = opened.groups()
                descriptors.discard(int(descriptor))
                if path in sources:
                    descriptors.add(int(descriptor))
                    opens += 1
            read = re.match(r'read\((\d+),.*= (\d+)$', line)
            if read and int(read[1]) in descriptors:
                read_bytes += int(read[2])
            closed = re.match(r'close\((\d+)\)', line)
            if closed:
                descriptors.discard(int(closed[1]))
    return {"source_opens": opens, "source_bytes_read": read_bytes}


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
    with tempfile.TemporaryDirectory(prefix="codeq-callee-sources-") as directory:
        base = Path(directory)
        for language, count in [("python", 128), ("rust", 64)]:
            root = base / language
            root.mkdir()
            if language == "python":
                source = "#" + "x" * (2 * 1024 * 1024) + "\ndef helper(): pass\n"
                source += "def caller():\n    helper()\n" * count
                extension, expected_name = "py", "helper"
            else:
                source = "//" + "x" * (2 * 1024 * 1024) + "\nstruct Thing;\nimpl Thing { fn build() {} }\n"
                source += "fn caller() { Thing::build(); }\n" * count
                extension, expected_name = "rs", "build"
            (root / f"source.{extension}").write_text(source)
            for examples in [False, True]:
                name = language + ("_examples" if examples else "_cached")
                command = ["callees", "caller"] + (["--examples"] if examples else [])
                for binary in binaries.values():
                    invoke(binary, root, command)
                expected = invoke(binaries["before"], root, command)[1]
                expected_data = json.loads(invoke(binaries["before"], root, ["--json", *command])[1][0])["data"]
                assert [row["name"] for row in expected_data] == [expected_name], expected_data
                for binary in binaries.values():
                    assert json.loads(invoke(binary, root, ["--json", *command])[1][0])["data"] == expected_data
                samples = {key: [] for key in binaries}
                for iteration in range(args.runs):
                    order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                    for key in order:
                        elapsed, output = invoke(binaries[key], root, command)
                        assert output == expected, name
                        samples[key].append(elapsed)
                traces = {key: trace(binary, root, command, expected) for key, binary in binaries.items()}
                assert traces["before"]["source_opens"] == count, traces
                assert traces["after"]["source_opens"] == (1 if language == "rust" or examples else 0), traces
                report["workloads"][name] = {
                    "command": command, "definitions": count, "source_bytes": len(source.encode()),
                    "samples_ms": samples, "source_traces_untimed": traces,
                    "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                    "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
                }
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: {"median_ms": value["median_ms"], "reads": value["source_traces_untimed"]}
                      for key, value in report["workloads"].items()}, indent=2))


if __name__ == "__main__":
    main()
