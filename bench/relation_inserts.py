#!/usr/bin/env python3
"""Compare relation-heavy cold and edited indexing, complete row parity, and peak RSS."""
import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
from pathlib import Path

from index_scheduling import snapshot
from json_rendering import peak_memory
from query_latency import invoke


CASES = [("dense", False), ("dense", True), ("single_base", False), ("single_base", True), ("mixed", False), ("small", False)]
QUERIES = [["--json", "implementors", "Base", "--limit", "0"],
           ["--json", "supertypes", "Item0"], ["--json", "graph", "--limit", "0"]]


def write_fixture(root, kind, version):
    files, count = {"dense": (8, 128), "single_base": (8, 4000), "mixed": (32, 100), "small": (32, 1)}[kind]
    for i in range(files):
        bases = ", ".join(["Base"] + [f"Base{n}" for n in range(31)]) if kind == "dense" else "Base"
        source = "".join(f"class Item{n}({bases}): pass # marker {version}\n" for n in range(count))
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
    path.write_text("class Original(Base): pass\n")
    invoke(binary, root, ["files"])
    with sqlite3.connect(root / ".codeq/index.db") as reader:
        reader.execute("BEGIN")
        before = reader.execute("SELECT subtype FROM impls ORDER BY subtype").fetchall()
        assert before == [("Original",)]
        path.write_text("class Replacement(Base): pass\n")
        invoke(binary, root, ["files"])
        assert reader.execute("SELECT subtype FROM impls ORDER BY subtype").fetchall() == before
        with sqlite3.connect(root / ".codeq/index.db") as current:
            assert current.execute("SELECT subtype FROM impls ORDER BY subtype").fetchall() == [("Replacement",)]
        reader.commit()
        assert reader.execute("SELECT subtype FROM impls ORDER BY subtype").fetchall() == [("Replacement",)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", choices=[kind + ("_edited" if edited else "_cold") for kind, edited in CASES], help="Run only these workloads; repeatable")
    parser.add_argument("--max-regression-percent", type=float, help="Opt-in median threshold for stable runners; writes the report before returning nonzero")
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("use at least five runs")
    if args.max_regression_percent is not None and (not math.isfinite(args.max_regression_percent) or args.max_regression_percent < 0):
        parser.error("regression threshold must be finite and nonnegative")
    binaries = {key: value.resolve() for key, value in {"before": args.before, "after": args.after}.items()}
    report = {
        "platform": platform.platform(), "load_before": os.getloadavg(), "runs": args.runs,
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "limits": "Whole CLI indexing into a fresh database or after editing every fixture file. Filesystem caches warm. Peak RSS measured separately through a small interpreter. Database IDs normalized by path; all facts, spans, calls, relations, file hashes and parse health compared. Integrity and foreign keys checked.",
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "source_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in ("src/index.rs", "bench/relation_inserts.py")},
        "workloads": {},
        "regressions": [],
    }
    with tempfile.TemporaryDirectory(prefix="codeq-index-compare-") as directory:
        for kind, edited in CASES:
            name = kind + ("_edited" if edited else "_cold")
            if args.case and name not in args.case:
                continue
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
                    assert len(current["impls"]) == {"dense": 32768, "single_base": 32000, "mixed": 3200, "small": 32}[kind], name
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
            report["workloads"][name] = {
                "source_bytes": sum(path.stat().st_size for path in root.iterdir() if path.is_file()),
                "rows": {table: len(rows) for table, rows in expected_snapshot.items()},
                "index_sha256": hashlib.sha256(json.dumps(expected_snapshot, sort_keys=True).encode()).hexdigest(),
                "schema_sha256": hashlib.sha256(json.dumps(schema).encode()).hexdigest(),
                "max_ms": {key: max(values) for key, values in samples.items()},
                "samples_ms": samples, "peak_rss_kib": peak_rss, "database_bytes": database_bytes,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
            }
            medians = report["workloads"][name]["median_ms"]
            if args.max_regression_percent is not None and medians["after"] > medians["before"] * (1 + args.max_regression_percent / 100):
                report["regressions"].append(name)
            print(name, report["workloads"][name]["median_ms"], peak_rss, flush=True)
        for key, binary in binaries.items():
            check_reader_snapshot(binary, Path(directory) / ("reader_" + key))
        report["reader_snapshot"] = "old reader remains consistent while an edit commits; new reader sees replacement relations"
    report.update(errors=0, answer_mismatches=0, load_after=os.getloadavg())
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    if report["regressions"]:
        raise SystemExit("median regression threshold exceeded: " + ", ".join(report["regressions"]))


if __name__ == "__main__":
    main()
