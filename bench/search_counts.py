#!/usr/bin/env python3
"""Compare warm CLI count searches. Assertion failures or child errors exit nonzero."""
import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path

from json_rendering import peak_memory
from query_latency import invoke


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=9)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.runs >= 3
    binaries = {key: value.resolve() for key, value in
                (("before", args.before), ("after", args.after))}
    repo = Path(__file__).resolve().parent.parent
    report = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "source_sha256": {str(path): hashlib.sha256((repo / path).read_bytes()).hexdigest()
                          for path in (Path("src/search.rs"), Path("tests/cli.rs"), Path("bench/search_counts.py"))},
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "load_at_start": os.getloadavg(), "runs": args.runs,
        "binaries": {key: {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
                     for key, binary in binaries.items()},
        "cases": {}, "parity_checks": 0,
    }
    with tempfile.TemporaryDirectory(prefix="codeq-search-counts-") as temporary:
        base = Path(temporary)
        roots = {name: base / name for name in ("dense", "sparse", "source", "tiny")}
        for root in roots.values():
            root.mkdir()
        for i in range(4):
            (roots["dense"] / f"file{i}.txt").write_text("needle needle needle needle\n" * 125_000)
        (roots["sparse"] / "large.txt").write_text("plain text\n" * 1_000_000 + "needle\n")
        (roots["tiny"] / "small.txt").write_text("needle needle\nplain\nneedle")
        shutil.copytree(repo / "src", roots["source"] / "src")
        for root in roots.values():
            subprocess.run([str(binaries["before"]), "index"], cwd=root, capture_output=True, check=True)
        cases = [
            ("dense_literal", "dense", ["needle"], 500_000),
            ("dense_regex", "dense", ["[a-z]+"], 500_000),
            ("one_match_per_line", "dense", ["^needle"], 500_000),
            ("sparse_literal", "sparse", ["needle"], 1),
            ("no_matches", "sparse", ["z"], 0),
            ("source_code", "source", ["[a-z]+", "--in", "code"], None),
            ("source_comments", "source", ["[a-z]+", "--in", "comments"], None),
            ("tiny", "tiny", ["needle"], 2),
        ]
        for name, root_name, search, expected_count in cases:
            root = roots[root_name]
            command = ["--json", "search", *search, "-c", "--limit", "0"]
            _, expected = invoke(binaries["before"], root, command)
            total = sum(row["count"] for row in json.loads(expected[0])["data"]["counts"])
            if expected_count is None:
                assert total > 0
            else:
                assert total == expected_count, (name, total, expected_count)
            samples = {key: [] for key in binaries}
            for key, binary in binaries.items():
                for _ in range(2):
                    assert invoke(binary, root, command)[1] == expected
            for run in range(args.runs):
                order = list(binaries) if run % 2 == 0 else list(reversed(binaries))
                for key in order:
                    elapsed, output = invoke(binaries[key], root, command)
                    assert output == expected, (name, key, "stdout/stderr mismatch")
                    samples[key].append(elapsed)
                    report["parity_checks"] += 1
            memory = {}
            for key, binary in binaries.items():
                memory[key] = []
                for _ in range(3):
                    rss, output = peak_memory(binary, root, command)
                    assert output == expected, (name, key, "memory run mismatch")
                    memory[key].append(rss)
                    report["parity_checks"] += 1
            report["cases"][name] = {
                "command": command, "count": total,
                "fixture_bytes": sum(p.stat().st_size for p in root.rglob("*")
                                     if p.is_file() and ".codeq" not in p.relative_to(root).parts),
                "elapsed_ms": samples,
                "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                "peak_rss_kib": memory,
            }
            print(name, report["cases"][name]["median_ms"], flush=True)
        edge = roots["tiny"]
        (edge / "edge.ts").write_text("// needle needle\r\nconst $needle = 'needle'; needle();\n\nneedle()")
        subprocess.run([str(binaries["before"]), "index"], cwd=edge, capture_output=True, check=True)
        for pattern in ["", "^", "$", ".*", r"\n", r"needle\(\);\n\nneedle", "needle"]:
            for flags in [[], ["-v"], ["-w"], ["-o"], ["--in", "code"],
                          ["--in", "strings"], ["--in", "comments"], ["--limit", "1"]]:
                for output_mode in [[], ["--json"], ["--pretty"]]:
                    command = [*output_mode, "search", pattern, "-c", *flags]
                    before = invoke(binaries["before"], edge, command)[1]
                    after = invoke(binaries["after"], edge, command)[1]
                    assert before == after, (command, before, after)
                    report["parity_checks"] += 1
    report["errors"] = 0
    report["answer_mismatches"] = 0
    report["load_at_end"] = os.getloadavg()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"{report['parity_checks']} exact stdout/stderr checks passed; wrote {args.output}")


if __name__ == "__main__":
    main()
