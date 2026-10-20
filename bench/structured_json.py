#!/usr/bin/env python3
"""Compare structured JSON rendering latency, peak memory, byte parity, and broken pipes."""
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

from query_latency import invoke


from direct_json import peak_memory, stable


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
        "platform": platform.platform(), "load_before": os.getloadavg(), "runs": args.runs,
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "limits": "Warm whole CLI process. Linux wait4 peak RSS is measured separately through a fresh small interpreter, avoiding the benchmark driver RSS floor at fork. Output is redirected to temporary files. Only index.elapsed_ms is normalized for meta/pretty output parity.",
        "source_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in ["src/envelope.rs", "src/envelope/sequence.rs", "bench/structured_json.py"]},
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-json-") as directory:
        root = Path(directory)
        count = 30000
        (root / "source.py").write_text("".join(f"def item{i:05}():\n    return '{'x' * 160}'\n" for i in range(count)))
        base = ["search", "return", "--limit", "0"]
        cases = {
            "compact": (["--json", *base], count),
            "pretty": (["--pretty", *base], count),
            "fields": (["--json", "--fields", "path,line,missing", *base], count),
            "capped": (["--json", "--max-per-file", "2", *base], 2),
            "group": (["--json", "--group", *base], count),
            "text": (base, None),
            "array": (["--json", "symbols", "--limit", "0"], count),
            "batch": (["--json", "about", "item00000", "item00001"], None),
            "nested": (["--json", "map", "--totals"], None),
            "meta": (["--json", "--meta", *base], count),
        }
        for name, (command, expected_rows) in cases.items():
            for binary in binaries.values():
                invoke(binary, root, command)
            expected = stable(invoke(binaries["before"], root, command)[1])
            assert expected[0], name
            if expected_rows is not None:
                data = json.loads(expected[0])["data"]
                rows = data["matches"] if isinstance(data, dict) else data
                assert len(rows) == expected_rows, name
            if name == "batch":
                envelope = json.loads(expected[0])
                assert envelope["rows"] == "data.results" and len(envelope["data"]["results"]) == 2
            if name == "nested":
                envelope = json.loads(expected[0])
                assert envelope["rows"] == "data.languages"
                assert envelope["data"]["totals"] and len(envelope["data"]["languages"]) == 1
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert stable(output) == expected, (name, key)
                    samples[key].append(elapsed)
            peak_rss = {}
            for key, binary in binaries.items():
                peak_rss[key], output = peak_memory(binary, root, command)
                assert stable(output) == expected, (name, key)
            report["workloads"][name] = {
                "command": command, "rows": expected_rows, "stdout_bytes": len(expected[0]),
                "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
                "samples_ms": samples, "peak_rss_kib": peak_rss,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            print(name, report["workloads"][name]["median_ms"], peak_rss, flush=True)
        for key, binary in binaries.items():
            for flag in ("--json", "--pretty"):
                reader, writer = os.pipe()
                os.close(reader)
                process = subprocess.Popen([str(binary), flag, *base], cwd=root, stdout=writer, stderr=subprocess.PIPE)
                os.close(writer)
                _, stderr = process.communicate(timeout=30)
                assert process.returncode == -signal.SIGPIPE and not stderr, (key, flag, process.returncode, stderr)
        report["broken_pipe"] = "Both JSON modes terminate with SIGPIPE and empty stderr on both binaries"
        report["fixture"] = {"rows": count, "bytes": (root / "source.py").stat().st_size}
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
