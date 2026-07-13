#!/usr/bin/env python3
"""Benchmark codeq against grep/rg/find on agent-style exploration tasks.

Usage: python3 bench/bench.py <repo-path>

Measures, per task: wall time (best of N runs), output bytes (a token proxy at
~4 bytes/token), and result count (how many hits the agent must read). codeq
uses its warm index (built once, timed separately); each query still re-walks
for freshness, so the numbers are honest end-to-end costs.
"""
import json
import os
import re
import subprocess
import sys
import time

REPO = os.path.abspath(sys.argv[1])
CODEQ = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "target", "release", "codeq"))
RUNS = 5


def run(cmd):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=REPO, capture_output=True)
    return time.perf_counter() - t0, p.stdout


def best(cmd):
    times, out = [], b""
    for _ in range(RUNS):
        t, out = run(cmd)
        times.append(t)
    return min(times) * 1000, out  # ms, last stdout


def codeq_count(out):
    """Number of result entries in a codeq JSON response."""
    try:
        d = json.loads(out)["data"]
    except Exception:
        return 0
    if isinstance(d, list):
        return len(d)
    if isinstance(d, dict):
        for k in ("matches", "references", "files", "counts", "chunks",
                  "definitions_by_file", "imports_by_module"):
            if k in d and isinstance(d[k], list):
                return len(d[k])
        return 1
    return 0


def line_count(out):
    return out.count(b"\n")


def pick(cmd, extract):
    _, out = run(cmd)
    try:
        return extract(json.loads(out)["data"])
    except Exception:
        return None


def main():
    # One-time index build (cold cost, reported separately).
    t_index, _ = run([CODEQ, "index", "--rebuild"])

    # Pick real targets from the index so the benchmark is repo-agnostic.
    # Prefer a plain-identifier class name that both codeq and rg handle cleanly.
    def clean_name(d):
        for s in d:
            if re.fullmatch(r"[A-Za-z][A-Za-z0-9]{3,}", s["name"]):
                return s["name"]
        return d[0]["name"]

    klass = pick([CODEQ, "symbols", "--kind", "class", "--limit", "200"], clean_name)
    tsfile = pick([CODEQ, "files", "--language", "typescript", "--limit", "1"],
                  lambda d: d[0]["path"])
    module = pick([CODEQ, "map", "--limit", "1"],
                  lambda d: d["imports_by_module"][0]["module"])

    print(f"repo: {REPO}")
    print(f"codeq index (rebuild, cold): {t_index*1000:.0f} ms")
    print(f"targets: class={klass!r} file={tsfile!r} module={module!r}\n")

    # (task, codeq argv, baseline name, baseline argv, note)
    tasks = [
        ("find definition of a symbol",
         [CODEQ, "definitions", klass],
         "rg", ["rg", "-n", rf"\b{klass}\b"],
         "codeq: 1 precise def; rg: every mention"),
        ("find all references to a symbol",
         [CODEQ, "references", klass],
         "rg", ["rg", "-nw", klass],
         "codeq adds enclosing symbol + def flag"),
        ("outline a file's structure",
         [CODEQ, "outline", tsfile],
         "rg", ["rg", "-n", r"function |class |interface |=>", tsfile],
         "codeq: structured + nesting"),
        ("who imports a module",
         [CODEQ, "importers", module],
         "rg", ["rg", "-n", rf"from ['\"].*{module}"],
         ""),
        ("TODO comments only",
         [CODEQ, "search", "TODO", "--in", "comments"],
         "rg", ["rg", "-n", "TODO"],
         "codeq excludes TODO in code/strings"),
        ("list all classes",
         [CODEQ, "symbols", "--kind", "class", "--limit", "0"],
         "rg", ["rg", "-n", r"class \w+"],
         ""),
        ("text search with context",
         [CODEQ, "search", klass, "-C", "2", "--limit", "0"],
         "rg", ["rg", "-n", "-C", "2", klass],
         "pure text search (rg's home turf)"),
    ]

    hdr = f"{'task':<34}{'tool':<7}{'ms':>8}{'bytes':>9}{'~tok':>7}{'results':>9}"
    print(hdr)
    print("-" * len(hdr))
    for name, cq, bname, bcmd, note in tasks:
        cms, cout = best(cq)
        bms, bout = best(bcmd)
        print(f"{name:<34}{'codeq':<7}{cms:>8.1f}{len(cout):>9}{len(cout)//4:>7}{codeq_count(cout):>9}")
        print(f"{'':<34}{bname:<7}{bms:>8.1f}{len(bout):>9}{len(bout)//4:>7}{line_count(bout):>9}")
        if note:
            print(f"{'':<34}↳ {note}")
    print()


if __name__ == "__main__":
    main()
