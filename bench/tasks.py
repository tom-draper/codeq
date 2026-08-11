#!/usr/bin/env python3
"""Task-level eval: codeq vs grep/rg/cat on realistic agent exploration tasks.

Usage: python3 bench/tasks.py <repo-path>

Unlike bench.py (single commands), this models multi-step *tasks* — the tool
sequence a competent agent would actually run to answer a question — and sums
the cost across the sequence: tool calls, output bytes (a token proxy at
~4 bytes/token, i.e. what the agent must read), and time. The baseline
sequences are deliberately what a good agent does with generic tools (e.g. rg
to locate, then read the file), not a strawman.
"""
import json
import os
import re
import sys

import corpus
from corpus import CODEQ

REPO = os.path.abspath(sys.argv[1])


def run(cmd):
    return corpus.timed_run(cmd, REPO)


def measure(steps):
    """steps: list of argv, or callables(prev_outputs)->argv|None for dependent
    steps. Returns (calls, bytes, ms)."""
    calls, nbytes, secs, outs = 0, 0, 0.0, []
    for step in steps:
        cmd = step(outs) if callable(step) else step
        if cmd is None:
            continue
        dt, out = run(cmd)
        calls += 1
        nbytes += len(out)
        secs += dt
        outs.append(out)
    return calls, nbytes, secs * 1000


def pick(cmd, extract):
    # Target probes parse JSON, so they must ask for it: codeq's default output
    # is text and `--json` is opt-in. Without this every target resolved to None
    # and the run died on the first baseline step -- silently, because `pick`
    # swallows the parse error. The measured task steps below are unaffected:
    # they consume the grep-style text on purpose, which is what an agent reads.
    _, out = run([cmd[0], "--json"] + cmd[1:])
    try:
        return extract(json.loads(out)["data"])
    except Exception:
        return None


def main():
    run([CODEQ, "index"])  # warm the index (one-time; not counted per task)

    def clean_class(d):
        for s in d:
            if re.fullmatch(r"[A-Za-z][A-Za-z0-9]{3,}", s["name"]):
                return s["name"]
        return d[0]["name"] if d else None

    sym = pick([CODEQ, "symbols", "--kind", "class", "--limit", "200"], clean_class)
    file = pick([CODEQ, "map", "--limit", "1"], lambda d: d["definitions_by_file"][0]["path"])
    module = pick([CODEQ, "map", "--limit", "1"], lambda d: d["imports_by_module"][0]["module"])

    # Dependent baseline step: rg to locate `sym`, then read a window around it.
    def read_window(outs):
        lines = outs[0].decode("utf-8", "ignore").splitlines()
        if not lines:
            return None
        first = lines[0]
        parts = first.split(":")
        path, ln = parts[0], int(parts[1])
        return ["sed", "-n", f"{max(1, ln - 2)},{ln + 40}p", path]

    def read_file(outs):
        lines = outs[0].decode("utf-8", "ignore").splitlines()
        if not lines:
            return None
        return ["cat", lines[0].split(":")[0]]

    tasks = [
        ("learn a file's API/structure",
         [[CODEQ, "outline", file, "--docs"]],
         [["cat", file]],
         "codeq: signatures; baseline: read whole file"),
        ("read a symbol's definition",
         [[CODEQ, "context", "--symbols", sym, "--max-tokens", "2000"]],
         [["rg", "-n", rf"\b{sym}\b", "--max-count", "1"], read_window],
         "codeq: exact body, 1 call; baseline: locate + read window"),
        ("find all usages of a symbol",
         [[CODEQ, "references", sym]],
         [["rg", "-nw", sym]],
         "codeq adds the enclosing function of each use"),
        ("find TODOs in comments only",
         [[CODEQ, "search", "TODO", "--in", "comments"]],
         [["rg", "-n", "TODO"]],
         "baseline also matches TODO in code/strings"),
        ("which files import a module",
         [[CODEQ, "importers", module]],
         [["rg", "-nlF", module]],
         "baseline overmatches (any line mentioning it)"),
        ("gather context to modify a function",
         [[CODEQ, "context", "--symbols", sym, "--expand", "--max-tokens", "4000"]],
         [["rg", "-n", rf"\b{sym}\b", "--max-count", "1"], read_file],
         "codeq: fn + referenced defs, bounded; baseline: read the file(s)"),
    ]

    print(f"repo: {REPO}")
    print(f"targets: symbol={sym!r} file={file!r} module={module!r}\n")
    hdr = f"{'task':<34}{'tool':<9}{'calls':>6}{'bytes':>8}{'~tok':>7}{'ms':>8}"
    print(hdr)
    print("-" * len(hdr))

    tot = {"codeq": [0, 0, 0.0], "baseline": [0, 0, 0.0]}
    for name, cq, base, note in tasks:
        for label, steps in (("codeq", cq), ("baseline", base)):
            calls, nbytes, ms = measure(steps)
            tot[label][0] += calls
            tot[label][1] += nbytes
            tot[label][2] += ms
            tag = name if label == "codeq" else ""
            print(f"{tag:<34}{label:<9}{calls:>6}{nbytes:>8}{nbytes // 4:>7}{ms:>8.1f}")
        print(f"{'':<34}↳ {note}")

    print("-" * len(hdr))
    for label in ("codeq", "baseline"):
        c, bts, ms = tot[label]
        print(f"{'TOTAL (6 tasks)':<34}{label:<9}{c:>6}{bts:>8}{bts // 4:>7}{ms:>8.1f}")
    cq_tok, bl_tok = tot["codeq"][1] // 4, tot["baseline"][1] // 4
    if cq_tok:
        print(f"\nbaseline reads {bl_tok / max(cq_tok,1):.1f}x the tokens and makes "
              f"{tot['baseline'][0]}/{tot['codeq'][0]} the tool calls of codeq.")


if __name__ == "__main__":
    main()
