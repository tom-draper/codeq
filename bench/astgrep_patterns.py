#!/usr/bin/env python3
"""Compare matcher-heavy warm CLI queries, including parsing and rendering."""
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
        "platform": platform.platform(), "load_before": os.getloadavg(),
        "runs": args.runs, "binary_sha256": {
            key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "limits": "Warm whole CLI process, including parsing and rendering. Allocation probe includes all intercepted malloc/calloc/realloc calls, not matcher-only allocations.",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-patterns-") as directory:
        root = Path(directory)
        probe = root / "alloc.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-O2", str(Path(__file__).with_name("count_allocations.c")), "-o", str(probe)], check=True)
        count = 5000
        (root / "source.rs").write_text("fn f() {\n" + "".join(
            f"foo(a{i}, b{i}, a{i}); foo(a{i}, b{i}, c{i}); pair(foo(a{i}, b{i}), bar(a{i}, b{i}));\n"
            for i in range(count)) + "}\n")
        cases = {
            "repeated_near_misses": ("foo($X, $Y, $X)", count),
            "named_variadic": ("foo($X, $$$REST, $X)", count),
            "repeated_variadic": ("pair(foo($$$ARGS), bar($$$ARGS))", count),
            "anonymous_variadic": ("foo($$$)", count * 3),
        }
        for name, (pattern, expected_rows) in cases.items():
            command = ["--json", "astgrep", pattern, "--language", "rust", "--limit", "0"]
            for binary in binaries.values():
                invoke(binary, root, command)
            expected = invoke(binaries["before"], root, command)[1]
            assert len(json.loads(expected[0])["data"]["matches"]) == expected_rows, name
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (name, key)
                    samples[key].append(elapsed)
            allocations = {}
            for key, binary in binaries.items():
                result = subprocess.run([str(binary), *command], cwd=root, env={**os.environ, "LD_PRELOAD": str(probe)}, capture_output=True, check=True)
                matches = re.findall(rb"# allocation_probe: (\d+)\n", result.stderr)
                assert len(matches) == 1 and result.stdout == expected[0], name
                assert re.sub(rb"# allocation_probe: \d+\n", b"", result.stderr) == expected[1], name
                allocations[key] = int(matches[0])
            report["workloads"][name] = {
                "command": command, "rows": expected_rows, "stdout_bytes": len(expected[0]),
                "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(),
                "samples_ms": samples, "allocations": allocations,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            print(name, report["workloads"][name]["median_ms"], allocations, flush=True)
        report["fixture"] = {"groups": count, "bytes": (root / "source.rs").stat().st_size}
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
