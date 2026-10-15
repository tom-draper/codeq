#!/usr/bin/env python3
"""Compare scoped search CLI output and warm latency on a mixed-language tree."""
import argparse
import hashlib
import json
import os
import platform
import statistics
import sqlite3
import tempfile
from pathlib import Path

from query_latency import invoke


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--files", type=int, default=4000)
    parser.add_argument("--runs", type=int, default=9)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.files >= 10 and args.runs >= 5
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {
        "platform": platform.platform(), "cpus": os.cpu_count(),
        "load": os.getloadavg(), "files": args.files, "runs": args.runs,
        "binary_sha256": {k: hashlib.sha256(v.read_bytes()).hexdigest()
                          for k, v in binaries.items()}, "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="codeq-scoped-search-") as directory:
        root = Path(directory)
        for index in range(args.files):
            extension = "py" if index < 10 else "js"
            (root / f"file{index:05}.{extension}").write_text("# needle\n")
        cases = {
            "language_literal": ["needle", "--language", "python"],
            "language_regex": ["n.e", "--language", "python"],
            "path_literal": ["needle", "--path", "file00000"],
            "positional_literal": ["needle", "file00000.py", "file00001.py"],
            "broad_literal": ["needle"],
        }
        for name, command in cases.items():
            command = ["search", *command, "-l", "--limit", "0"]
            for binary in binaries.values():
                invoke(binary, root, command)
            expected = invoke(binaries["before"], root, command)[1]
            expected_count = (10 if name.startswith("language") else
                              1 if name.startswith("path_") else
                              2 if name.startswith("positional") else args.files)
            assert len(expected[0].splitlines()) == expected_count, name
            assert expected[1] == b"", expected[1]
            # Mirror the candidate SELECT to record SQLite's plan separately
            # from the complete CLI timing. Positional paths remain substrings.
            base = "SELECT path, language FROM files WHERE is_text = 1"
            predicates, parameters = [], []
            if name.startswith("language"):
                predicates.append("language = ?")
                parameters.append("python")
            elif name.startswith("path_"):
                predicates.append("instr(path, ?) > 0")
                parameters.append("file00000")
            elif name.startswith("positional"):
                predicates.append("EXISTS (SELECT 1 FROM json_each(?) WHERE instr(files.path, value) > 0)")
                parameters.append(json.dumps(["file00000.py", "file00001.py"]))
            fts = " AND id IN (SELECT rowid FROM content_fts WHERE content_fts MATCH ?)"
            fts_parameters = ['"nee" AND "eed" AND "edl" AND "dle"']
            if name.endswith("regex"):
                fts, fts_parameters = "", []
            queries = {
                "before": (base + fts + " ORDER BY path", fts_parameters),
                "after": (base + "".join(" AND " + p for p in predicates) + fts + " ORDER BY path",
                          parameters + fts_parameters),
            }
            with sqlite3.connect(root / ".codeq/index.db") as conn:
                plans = {key: {
                    "sql": sql, "parameters": params,
                    "returned_candidates": len(conn.execute(sql, params).fetchall()),
                    "explain": conn.execute("EXPLAIN QUERY PLAN " + sql, params).fetchall(),
                } for key, (sql, params) in queries.items()}
            assert plans["before"]["returned_candidates"] == args.files
            assert plans["after"]["returned_candidates"] == expected_count
            samples = {key: [] for key in binaries}
            for iteration in range(args.runs):
                order = list(binaries) if iteration % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (name, key)
                    samples[key].append(elapsed)
            report["workloads"][name] = {
                "command": command, "expected_rows": expected_count,
                "samples_ms": samples, "candidate_queries": plans,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                "range_ms": {key: [min(values), max(values)] for key, values in samples.items()},
            }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value["median_ms"] for key, value in report["workloads"].items()}, indent=2))


if __name__ == "__main__":
    main()
