#!/usr/bin/env python3
"""Compare cold CLI indexing and persisted shapes on nested class fixtures."""
import argparse
import hashlib
import json
import os
import platform
import shutil
import sqlite3
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
    parser.add_argument("--count-allocations", action="store_true",
                        help="compile and preload the Linux/glibc allocator probe in separate untimed runs")
    args = parser.parse_args()
    assert args.runs >= 5
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {
        "platform": platform.platform(), "cpus": os.cpu_count(),
        "load": os.getloadavg(), "runs": args.runs,
        "binary_sha256": {k: hashlib.sha256(v.read_bytes()).hexdigest()
                          for k, v in binaries.items()},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-clone-shapes-") as directory:
        root = Path(directory)
        for file in range(16):
            source = []
            for cls in range(20):
                source.append(f"class Container{cls}:\n")
                for method in range(20):
                    source.append(f"    def method{method}(self, value):\n")
                    for line in range(12):
                        source.append(f"        value = value + {line}\n")
                    source.append("        return value\n")
            (root / f"module{file:02}.py").write_text("".join(source))
        samples = {key: [] for key in binaries}
        expected_facts = None
        expected_output = None
        for iteration in range(args.runs):
            order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
            for key in order:
                shutil.rmtree(root / ".codeq", ignore_errors=True)
                elapsed, output = invoke(binaries[key], root, ["files"])
                with sqlite3.connect(root / ".codeq/index.db") as conn:
                    facts = conn.execute("SELECT * FROM facts ORDER BY path, start_byte, name, kind").fetchall()
                    assert len(facts) == 16 * 20 * 21, len(facts)
                    assert conn.execute("PRAGMA integrity_check").fetchone() == ("ok",)
                if expected_facts is None:
                    expected_facts, expected_output = facts, output
                assert facts == expected_facts, (iteration, key, "facts changed")
                assert output == expected_output, (iteration, key, "output changed")
                assert b"reindexed 16 file(s)" in output[1], output[1]
                samples[key].append(elapsed)
        if args.count_allocations:
            assert platform.system() == "Linux" and platform.libc_ver()[0] == "glibc"
            with tempfile.TemporaryDirectory(prefix="codeq-allocation-probe-") as probe_dir:
                library = Path(probe_dir) / "allocations.so"
                subprocess.run(["cc", "-O2", "-shared", "-fPIC", "-o", str(library),
                                str(Path(__file__).with_name("count_allocations.c"))], check=True)
                counts = {}
                for key, binary in binaries.items():
                    counts[key] = []
                    for _ in range(2):
                        shutil.rmtree(root / ".codeq", ignore_errors=True)
                        env = dict(os.environ, LD_PRELOAD=str(library))
                        result = subprocess.run([str(binary), "files"], cwd=root,
                                                env=env, capture_output=True, check=True)
                        stderr, marker, count = result.stderr.rpartition(b"# allocation_probe: ")
                        assert marker and (result.stdout, stderr) == expected_output
                        with sqlite3.connect(root / ".codeq/index.db") as conn:
                            assert conn.execute("SELECT * FROM facts ORDER BY path, start_byte, name, kind").fetchall() == expected_facts
                        counts[key].append(int(count))
                report["intercepted_allocations_untimed"] = counts
        report.update({
            "files": 16, "facts": len(expected_facts),
            "source_bytes": sum(p.stat().st_size for p in root.glob("*.py")),
            "samples_ms": samples,
            "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
            "state": "fresh database each run, filesystem cache warm, alternating binaries",
        })
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
