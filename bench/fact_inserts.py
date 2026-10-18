#!/usr/bin/env python3
"""Compare batched fact inserts with complete indexing and query parity."""
import argparse
import hashlib
import json
import os
import platform
import shutil
import sqlite3
import statistics
import subprocess
import time
import tempfile
from pathlib import Path

from index_scheduling import snapshot
from json_rendering import peak_memory
from query_latency import invoke


CASES = [("dense", False), ("dense", True), ("mixed", False), ("small", False)]
QUERIES = [["--json", "symbols", "--limit", "0"], ["--json", "outline", "source000.py", "--docs"]] + [["--json", "search", "marker", "--in", kind, "--limit", "0"] for kind in ["comments", "strings", "code"]]


def write_fixture(root, kind, version):
    if kind == "mixed":
        for path in (Path(__file__).resolve().parent.parent / "tests/fixtures/languages").iterdir():
            if path.is_file():
                (root / path.name).write_bytes(path.read_bytes() + b"\n" * (version + 1))
        (root / "source000.py").write_text(f'def item000(): return "marker {version}"\n')
        return
    for i in range(8 if kind == "dense" else 32):
        source = ""
        for n in range(4000 if kind == "dense" else 1):
            name = ("test_" if n % 2 else "") + f"item{n:04}"
            decorator = "@marker\n" if n % 3 == 0 else ""
            doc = f'    """marker {version} π"""\n' if n % 2 else ""
            source += f'{decorator}def {name}():\n{doc}    return helper("marker {version}")\n'
        (root / f"source{i:03}.py").write_text(source)


def prepare(binary, root, kind, edited):
    shutil.rmtree(root / ".codeq", ignore_errors=True)
    write_fixture(root, kind, 0 if edited else 1)
    if edited:
        invoke(binary, root, ["files"])
        write_fixture(root, kind, 1)


def check_reader_snapshot(binary, root):
    root.mkdir()
    path = root / "source.py"
    path.write_text("def original(): return old_call('marker')\n")
    invoke(binary, root, ["files"])
    with sqlite3.connect(root / ".codeq/index.db") as reader:
        reader.execute("BEGIN")
        before = reader.execute("SELECT name FROM facts ORDER BY name").fetchall()
        assert before == [("original",)]
        assert reader.execute("SELECT name FROM calls").fetchall() == [("old_call",)]
        path.write_text("def replacement(): return new_call('marker')\n")
        invoke(binary, root, ["files"])
        assert reader.execute("SELECT name FROM facts ORDER BY name").fetchall() == before
        with sqlite3.connect(root / ".codeq/index.db") as current:
            assert current.execute("SELECT name FROM facts ORDER BY name").fetchall() == [("replacement",)]
            assert current.execute("SELECT name FROM calls").fetchall() == [("new_call",)]
        assert reader.execute("SELECT name FROM calls").fetchall() == [("old_call",)]
        reader.commit()
        assert reader.execute("SELECT name FROM calls").fetchall() == [("new_call",)]
        assert reader.execute("SELECT name FROM facts ORDER BY name").fetchall() == [("replacement",)]


def sample_index_cpu(binary, root, kind, edited, expected):
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
        "sampling": "maximum observed thread CPU ticks at 5 ms intervals; may miss final ticks. Main thread includes writing and orchestration; workers include parsing. Not isolated function timings.",
    }


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
        "limits": "Whole CLI indexing into a fresh database or after editing every fixture file. Filesystem caches warm. Peak RSS measured separately through a small interpreter. Database IDs normalized by path; all facts, spans, calls, relations, file hashes and parse health compared. Integrity and foreign keys checked.",
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-index-compare-") as directory:
        for kind, edited in CASES:
            name = kind + ("_edited" if edited else "_cold")
            root = Path(directory) / name
            root.mkdir()
            expected = expected_snapshot = None
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    prepare(binaries[key], root, kind, edited)
                    elapsed, output = invoke(binaries[key], root, ["files"])
                    current = snapshot(root)
                    assert len(current["facts"]) >= {"dense": 32000, "mixed": 100, "small": 32}[kind], name
                    if kind != "mixed":
                        assert len(current["facts"]) == {"dense": 32000, "small": 32}[kind], name
                    if expected is None:
                        expected, expected_snapshot = output, current
                    assert current == expected_snapshot and output == expected, (name, key)
                    samples[key].append(elapsed)
            peak_rss, database_bytes = {}, {}
            answers = expected_schema = None
            for key, binary in binaries.items():
                prepare(binary, root, kind, edited)
                peak_rss[key], output = peak_memory(binary, root, ["files"])
                assert output == expected and snapshot(root) == expected_snapshot, (name, key)
                database_bytes[key] = (root / ".codeq/index.db").stat().st_size
                queries = [invoke(binary, root, command)[1] for command in QUERIES]
                assert all(output[0] for output in queries), name
                if answers is None:
                    answers = queries
                assert queries == answers, (name, key, "search results")
                with sqlite3.connect(root / ".codeq/index.db") as conn:
                    schema = conn.execute("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").fetchall()
                if expected_schema is None:
                    expected_schema = schema
                assert schema == expected_schema, (name, key, "schema")
            with sqlite3.connect(root / ".codeq/index.db") as conn:
                schema = conn.execute("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").fetchall()
            cpu = {key: sample_index_cpu(binary, root, kind, edited, expected) for key, binary in binaries.items()}
            report["workloads"][name] = {
                "sampled_cpu": cpu,
                "source_bytes": sum(path.stat().st_size for path in root.iterdir() if path.is_file()),
                "rows": {table: len(rows) for table, rows in expected_snapshot.items()},
                "index_sha256": hashlib.sha256(json.dumps(expected_snapshot, sort_keys=True).encode()).hexdigest(),
                "schema_sha256": hashlib.sha256(json.dumps(schema).encode()).hexdigest(),
                "samples_ms": samples, "peak_rss_kib": peak_rss, "database_bytes": database_bytes,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            print(name, report["workloads"][name]["median_ms"], peak_rss, flush=True)
        for key, binary in binaries.items():
            check_reader_snapshot(binary, Path(directory) / ("reader_" + key))
        report["reader_snapshot"] = "old reader remains consistent while an edit commits; new reader sees replacement facts and calls"
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
