#!/usr/bin/env python3
"""Compare text output bytes, warm CLI latency, whole-process peak RSS, and SIGPIPE."""
import argparse
import hashlib
import json
import os
import platform
import signal
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
        "limits": "Whole warm CLI process, including query and Value conversion. Peak RSS measured separately through a fresh small interpreter. No metadata timestamps normalized.",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-text-render-") as directory:
        root = Path(directory)
        count = 60000
        (root / "source.py").write_text("".join(f"def item{i:05}():\n    return '{'x' * 160}'\n" for i in range(count)))
        base = ["search", "return", "--limit", "0"]
        cases = {
            "flat": base,
            "group": ["--group", *base],
            "context": [*base, "-C", "1"],
            "fields": ["--fields", "path,line,text,col", *base],
            "capped": ["--max-per-file", "2", *base],
            "capped_group": ["--group", "--max-per-file", "2", *base],
            "batch": ["definitions", "item00001", "item00002"],
            "source_block": ["context", "--symbols", "item00001", "--max-tokens", "1000"],
            "json_control": ["--json", *base],
        }
        for binary in binaries.values():
            invoke(binary, root, base)
            answer = json.loads(invoke(binary, root, ["--json", *base])[1][0])
            assert len(answer["data"]["matches"]) == count
        for name, command in cases.items():
            expected = invoke(binaries["before"], root, command)[1]
            assert expected[0], name
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (name, key)
                    samples[key].append(elapsed)
            peak_rss = {}
            for key, binary in binaries.items():
                peak_rss[key], output = peak_memory(binary, root, command)
                assert output == expected, (name, key)
            report["workloads"][name] = {
                "command": command, "stdout_bytes": len(expected[0]),
                "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
                "samples_ms": samples, "peak_rss_kib": peak_rss,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            print(name, report["workloads"][name]["median_ms"], peak_rss, flush=True)
        for binary in binaries.values():
            for command in [base, [*base, "-C", "1"], ["--group", *base]]:
                reader, writer = os.pipe()
                os.close(reader)
                process = subprocess.Popen([str(binary), *command], cwd=root, stdout=writer, stderr=subprocess.PIPE)
                os.close(writer)
                _, stderr = process.communicate(timeout=30)
                assert process.returncode == -signal.SIGPIPE and not stderr, (command, process.returncode, stderr)
        report["broken_pipe"] = "flat, grouped and context output terminate with SIGPIPE and empty stderr on both binaries"
        report["fixture"] = {"rows": count, "bytes": (root / "source.py").stat().st_size}
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
