#!/usr/bin/env python3
"""Compare localized refresh latency and complete index parity at increasing file counts."""
import argparse
import hashlib
import json
import platform
import shutil
import statistics
import subprocess
import tempfile
from pathlib import Path

from index_scheduling import snapshot
from json_rendering import peak_memory
from query_latency import invoke


def prepare(root, mode, iteration, binary):
    target = root / "group_0/file_0.py"
    if mode == "cold":
        shutil.rmtree(root / ".codeq", ignore_errors=True)
    elif mode == "edit":
        target.write_text(f"def target(): return {iteration}\n")
    elif mode == "add":
        (root / f"group_0/added_{iteration}.py").write_text("def added(): pass\n")
    elif mode == "delete":
        target.write_text("def target(): pass\n")
        invoke(binary, root, ["files"])
        target.unlink()
    elif mode == "ignore":
        (root / ".codeqignore").write_text(f"group_{iteration % 2}/\n")


def comparable(output):
    result = json.loads(output[0])
    result.pop("repo", None)
    result.pop("index", None)
    return result, output[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--files", type=int, nargs="+", default=[1000, 10000])
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 5 or min(args.files) < 150:
        parser.error("use at least five runs and at least 150 files")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "source_sha256": {p: hashlib.sha256(Path(p).read_bytes()).hexdigest()
                          for p in ["src/index.rs", "bench/localized_refresh.py"]},
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
        "platform": platform.platform(), "runs": args.runs, "workloads": {},
        "limits": "Whole CLI latency, warm filesystem caches. Cold means a missing index. "
                  "All indexed facts, calls, spans, relations and file hashes checked outside timing. "
                  "Peak RSS sampled separately. Every query still stats cached paths. "
                  "Manifest and tree-walk bounds are checked by the localized_refresh Rust tests.",
    }
    command = ["--json", "definitions", "helper_100"]
    with tempfile.TemporaryDirectory(prefix="codeq-local-refresh-") as directory:
        for count in args.files:
            for mode in ["cold", "warm", "edit", "add", "delete", "ignore"]:
                roots = {key: Path(directory) / f"{count}-{mode}-{key}" for key in binaries}
                for key, root in roots.items():
                    root.mkdir()
                    for i in range(count):
                        path = root / f"group_{i // 50}/file_{i}.py"
                        path.parent.mkdir(exist_ok=True)
                        name = "target" if i == 0 else f"helper_{i}"
                        path.write_text(f"def {name}(): pass\n")
                    invoke(binaries[key], root, command)
                samples = {key: [] for key in binaries}
                for iteration in range(args.runs + 1):
                    answers = []
                    indexes = []
                    for key in (list(binaries) if iteration % 2 == 0 else list(reversed(binaries))):
                        prepare(roots[key], mode, iteration, binaries[key])
                        elapsed, answer = invoke(binaries[key], roots[key], command)
                        rows = json.loads(answer[0])["data"]
                        expected = count + iteration + 1 if mode == "add" else count - 1 if mode == "delete" else count - 50 if mode == "ignore" else count
                        assert len(rows) == 1 and rows[0]["name"] == "helper_100", (mode, key, rows)
                        assert not answer[1], (mode, key, answer[1])
                        reindexed = json.loads(answer[0]).get("index", {}).get("reindexed_files", 0)
                        assert reindexed == (count if mode == "cold" else 1 if mode in ["edit", "add"] else 50 if mode == "ignore" and iteration else 0), (mode, key, reindexed)
                        answers.append(comparable(answer))
                        indexes.append(snapshot(roots[key]))
                        assert len(indexes[-1]["files"]) == expected, (mode, key, expected)
                        if iteration:
                            samples[key].append(elapsed)
                    assert answers[0] == answers[1] and indexes[0] == indexes[1], (count, mode, iteration)
                rss = {}
                answers = []
                for key, binary in binaries.items():
                    prepare(roots[key], mode, args.runs + 1, binary)
                    rss[key], answer = peak_memory(binary, roots[key], command)
                    answers.append(comparable(answer))
                assert answers[0] == answers[1], (count, mode, "memory run")
                row = {"samples_ms": samples, "peak_rss_kib": rss,
                       "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                       "max_ms": {key: max(values) for key, values in samples.items()}}
                report["workloads"][f"{count}_{mode}"] = row
                print(count, mode, row["median_ms"], flush=True)
                for root in roots.values():
                    shutil.rmtree(root)
    report.update(errors=0, answer_mismatches=0)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
