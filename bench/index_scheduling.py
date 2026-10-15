#!/usr/bin/env python3
"""Compare cold and edited indexing on small, large, and skewed file batches."""
import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

from query_latency import invoke


def fixture(root, kind, variant):
    count = 128 if kind == "skewed" else 8
    for index in range(count):
        large = kind != "small" and index < 8
        rust = large and index % 2 == 0
        lines = ["// scheduling marker\n" if rust else "# scheduling marker\n"]
        for function in range(600 if large else 1):
            if rust:
                lines.append(f"pub fn item{function}(mut value: i64) -> i64 {{\n")
                lines.extend(f"    value = value + {line + variant};\n" for line in range(12))
                lines.append("    helper(value)\n}\n")
            else:
                lines.append(f"def item{function}(value):\n")
                lines.extend(f"    value = value + {line + variant}\n" for line in range(12))
                lines.append("    return helper(value)\n")
        (root / f"module{index:03}.{'rs' if rust else 'py'}").write_text("".join(lines))
    return count


def snapshot(root):
    with sqlite3.connect(root / ".codeq/index.db") as conn:
        assert conn.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        assert not conn.execute("PRAGMA foreign_key_check").fetchall()
        return {
            "files": conn.execute("SELECT path, language, size_bytes, is_text, content_hash, parse_errors, parse_recovered FROM files ORDER BY path").fetchall(),
            "facts": conn.execute("SELECT * FROM facts ORDER BY path, start_byte, name, kind").fetchall(),
            "impls": conn.execute("SELECT * FROM impls ORDER BY path, line, subtype, supertype, relation").fetchall(),
            "calls": conn.execute("SELECT f.path, c.start_byte, c.name, c.receiver FROM calls c JOIN files f ON f.id = c.file_id ORDER BY 1, 2, 3, 4").fetchall(),
            "spans": conn.execute("SELECT f.path, s.start_byte, s.end_byte, s.kind FROM spans s JOIN files f ON f.id = s.file_id ORDER BY 1, 2, 3, 4").fetchall(),
        }


def prepare(binary, root, kind, edited):
    shutil.rmtree(root / ".codeq", ignore_errors=True)
    count = fixture(root, kind, 0)
    if edited:
        invoke(binary, root, ["files"])
        fixture(root, kind, 1)
    return count


def worker_cpu(binary, root, kind, edited, expected):
    prepare(binary, root, kind, edited)
    process = subprocess.Popen([str(binary), "files"], cwd=root,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    ticks = {}
    while process.poll() is None:
        for task in Path(f"/proc/{process.pid}/task").glob("*/stat"):
            try:
                fields = task.read_text().rsplit(") ", 1)[1].split()
                tid = int(task.parent.name)
                ticks[tid] = max(ticks.get(tid, 0), int(fields[11]) + int(fields[12]))
            except (FileNotFoundError, ProcessLookupError):
                pass
        time.sleep(0.005)
    output = process.communicate()
    assert process.returncode == 0 and output == expected
    hz = os.sysconf("SC_CLK_TCK")
    return {
        "main_cpu_seconds_approx": ticks.pop(process.pid, 0) / hz,
        "worker_cpu_seconds_approx": sorted(tick / hz for tick in ticks.values()),
        "sampling": "maximum observed per-thread CPU ticks, sampled every 5 ms; may miss final ticks",
    }


def profile(binary, root, kind, edited, expected):
    prepare(binary, root, kind, edited)
    with tempfile.NamedTemporaryFile() as memory, tempfile.NamedTemporaryFile() as threads:
        result = subprocess.run(
            ["strace", "-f", "-qq", "-e", "clone,clone3", "-o", threads.name,
             "/usr/bin/time", "-v", "-o", memory.name, str(binary), "files"],
            cwd=root, capture_output=True, check=True,
        )
        assert (result.stdout, result.stderr) == expected
        stats = Path(memory.name).read_text()
        return {
            "peak_rss_kib": int(re.search(r"Maximum resident set size \(kbytes\): (\d+)", stats)[1]),
            "user_seconds": float(re.search(r"User time \(seconds\): ([\d.]+)", stats)[1]),
            "system_seconds": float(re.search(r"System time \(seconds\): ([\d.]+)", stats)[1]),
            "threads_created": len(re.findall(r"clone(?:3)?\([^\n]*CLONE_THREAD[^\n]*= [1-9]\d*", Path(threads.name).read_text())),
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
    report = {
        "platform": platform.platform(), "cpus": os.cpu_count(), "load_before": os.getloadavg(),
        "runs": args.runs, "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                                                for key, path in binaries.items()},
        "state": "fresh database per invocation; filesystem cache warm; edited cases prebuild outside timing",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-index-scheduling-") as directory:
        root = Path(directory)
        for kind, edited in [("few_large", False), ("few_large", True), ("skewed", False), ("skewed", True), ("small", False)]:
            for path in root.glob("module*"):
                path.unlink()
            name = kind + ("_edited" if edited else "_cold")
            expected_output = expected_snapshot = None
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    count = prepare(binaries[key], root, kind, edited)
                    elapsed, output = invoke(binaries[key], root, ["files"])
                    current = snapshot(root)
                    assert len(current["files"]) == count
                    assert len(current["facts"]) == (count if kind == "small" else 4800 + count - 8)
                    assert len(current["calls"]) == len(current["facts"])
                    assert len(current["spans"]) == count
                    assert f"reindexed {count} file(s)".encode() in output[1], output[1]
                    if expected_output is None:
                        expected_output, expected_snapshot = output, current
                    assert output == expected_output, (name, key, "output changed")
                    assert current == expected_snapshot, (name, key, "index changed")
                    samples[key].append(elapsed)
            profiles = {}
            for key, binary in binaries.items():
                profiles[key] = profile(binary, root, kind, edited, expected_output)
                profiles[key].update(worker_cpu(binary, root, kind, edited, expected_output))
            assert snapshot(root) == expected_snapshot
            report["workloads"][name] = {
                "files": count, "source_bytes": sum(path.stat().st_size for path in root.glob("module*")),
                "rows": {table: len(rows) for table, rows in expected_snapshot.items()},
                "samples_ms": samples, "profiles_untimed": profiles,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
            }
            print(f"{name}: {report['workloads'][name]['median_ms']}", flush=True)
    report["errors"] = report["answer_mismatches"] = 0
    report["load_after"] = os.getloadavg()
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
