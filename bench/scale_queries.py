#!/usr/bin/env python3
"""Verify unused, untested, hotspots and historical API queries at increasing scale."""
import argparse
import hashlib
import json
import platform
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
    parser.add_argument("--files", type=int, nargs="+", default=[100, 1000])
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 5 or min(args.files) < 1:
        parser.error("use at least five runs and positive file counts")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {"platform": platform.platform(), "runs": args.runs, "workloads": {},
              "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
              "limits": "Whole warm CLI latency and separately measured peak RSS. Synthetic Rust files. "
                        "Historical API diff changes every public signature. Exact row names and counts checked before timing."}
    with tempfile.TemporaryDirectory(prefix="codeq-scale-queries-") as directory:
        for files in args.files:
            root = Path(directory) / str(files)
            root.mkdir()
            (root / "target.rs").write_text("fn target() {}\n")
            for i in range(files):
                (root / f"item_{i}.rs").write_text(f"pub fn public_{i}() {{target();}}\nfn private_{i}() {{target();}}\n")
            git = ["git", "-c", "user.name=bench", "-c", "user.email=bench@example.test", "-c", "commit.gpgsign=false"]
            for command in [["init", "-q"], ["add", "."], ["commit", "-qm", "fixture"]]:
                subprocess.run([*git, *command], cwd=root, check=True, capture_output=True)
            for i in range(files):
                (root / f"item_{i}.rs").write_text(f"pub fn public_{i}(value: i32) {{target();}}\nfn private_{i}() {{target();}}\n")
            cases = {
                "unused": ["unused", "--limit", "0"],
                "untested": ["untested", "--limit", "0"],
                "hotspots": ["hotspots", "--limit", "0"],
                "api_diff": ["api", "--compare-to", "HEAD", "--limit", "0"],
                "context": ["context", "--symbols", "target,public_0", "--max-tokens", "800"],
            }
            for name, arguments in cases.items():
                command = ["--json", *arguments]
                for binary in binaries.values():
                    invoke(binary, root, command)
                expected = invoke(binaries["before"], root, command)[1]
                data = json.loads(expected[0])["data"]
                if name == "unused":
                    assert {row["name"] for row in data} == {f"{prefix}_{i}" for prefix in ["public", "private"] for i in range(files)}
                elif name == "untested":
                    assert {row["name"] for row in data} == {f"public_{i}" for i in range(files)}
                elif name == "hotspots":
                    assert len(data) == 1 and data[0]["name"] == "target" and data[0]["refs"] == files * 2, data
                elif name == "context":
                    assert {row["name"] for row in data["chunks"]} == {"target", "public_0"}, data
                    assert any("value: i32" in row["text"] for row in data["chunks"]), data
                else:
                    assert len(data["changed"]) == files and not data["added"] and not data["removed"], data
                samples = {key: [] for key in binaries}
                for iteration in range(args.runs):
                    for key in (list(binaries) if iteration % 2 == 0 else list(reversed(binaries))):
                        elapsed, output = invoke(binaries[key], root, command)
                        assert output == expected, (files, name, key)
                        samples[key].append(elapsed)
                rss = {}
                for key, binary in binaries.items():
                    rss[key], output = peak_memory(binary, root, command)
                    assert output == expected, (files, name, key, "memory")
                row = {"command": command, "files": files + 1, "samples_ms": samples,
                       "answer_sha256": hashlib.sha256(b"\0".join(expected)).hexdigest(), "peak_rss_kib": rss,
                       "median_ms": {key: statistics.median(values) for key, values in samples.items()}}
                report["workloads"][f"{files}_{name}"] = row
                print(files, name, row["median_ms"], flush=True)
    report.update(errors=0, answer_mismatches=0)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
