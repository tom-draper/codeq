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
import sys

import corpus
from corpus import CODEQ

REPO = os.path.abspath(sys.argv[1])
RUNS = 5


def run(cmd):
    return corpus.timed_run(cmd, REPO)


def best(cmd):
    times, out = [], b""
    for _ in range(RUNS):
        t, out = run(cmd)
        times.append(t)
    return min(times) * 1000, out  # ms, last stdout


def codeq_count(out):
    """Number of result entries in codeq's JSON or compact-text response."""
    try:
        d = json.loads(out)["data"]
    except Exception:
        # Benchmarked invocations use compact text. Every result row occupies one
        # non-empty stdout line; refresh diagnostics are stderr by contract.
        return sum(1 for line in out.splitlines() if line.strip())
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
    # Target discovery reads the structured envelope, while the benchmarked
    # tasks deliberately exercise codeq's compact text default. The CLI used to
    # default to JSON, so this distinction was implicit; without it every
    # discovery parse fails and the supposedly repo-agnostic benchmark exits.
    _, out = run([cmd[0], "--json", *cmd[1:]])
    try:
        return extract(json.loads(out)["data"])
    except Exception:
        return None


def main():
    # One-time index build (cold cost, reported separately).
    t_index, _ = run([CODEQ, "index", "--rebuild"])

    # Pick real targets from the index so the benchmark is repo-agnostic.
    # Prefer a plain identifier that both codeq and rg handle cleanly; requiring
    # a TypeScript file or a class made this script crash on perfectly valid
    # repositories written in another supported language.
    target = pick([CODEQ, "symbols", "--limit", "200"],
                  lambda d: next((s for s in d if re.fullmatch(r"[A-Za-z][A-Za-z0-9]{3,}", s["name"])), d[0]))
    if target is None:
        print("no indexable symbols; benchmark needs at least one supported source definition", file=sys.stderr)
        return 2
    symbol = target["name"]
    source_file = target["path"]
    module = pick([CODEQ, "map", "--limit", "1"],
                  lambda d: d["imports_by_module"][0]["module"] if d["imports_by_module"] else None)

    print(f"repo: {REPO}")
    print(f"codeq index (rebuild, cold): {t_index*1000:.0f} ms")
    print(f"targets: symbol={symbol!r} file={source_file!r} module={module!r}\n")

    # (task, codeq argv, baseline name, baseline argv, note)
    tasks = [
        ("find definition of a symbol",
         [CODEQ, "definitions", symbol],
         "rg", ["rg", "-n", rf"\b{symbol}\b"],
         "codeq: 1 precise def; rg: every mention"),
        ("find all references to a symbol",
         [CODEQ, "references", symbol],
         "rg", ["rg", "-nw", symbol],
         "codeq adds enclosing symbol + def flag"),
        ("outline a file's structure",
         [CODEQ, "outline", source_file],
         "rg", ["rg", "-n", r"fn |function |class |interface |=>", source_file],
         "codeq: structured + nesting"),
        ("TODO comments only",
         [CODEQ, "search", "TODO", "--in", "comments"],
         "rg", ["rg", "-n", "TODO"],
         "codeq excludes TODO in code/strings"),
        ("list all classes",
         [CODEQ, "symbols", "--kind", "class", "--limit", "0"],
         "rg", ["rg", "-n", r"class \w+"],
         ""),
        ("text search with context",
         [CODEQ, "search", symbol, "-C", "2", "--limit", "0"],
         "rg", ["rg", "-n", "-C", "2", symbol],
         "pure text search (rg's home turf)"),
    ]
    if module is not None:
        tasks.insert(3, ("who imports a module",
                         [CODEQ, "importers", module],
                         "rg", ["rg", "-n", rf"from ['\"].*{module}"],
                         ""))

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
