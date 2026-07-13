#!/usr/bin/env python3
"""Differential correctness + efficiency harness: codeq vs the tools an agent
would otherwise chain (ripgrep, cat), across many real repos.

Two batteries run against every repo in the corpus (bench/corpus.py):

1. SEARCH — the same query through `codeq search` and an equivalent `rg`. codeq
   promises its matches are a SUPERSET of ripgrep's (it may find more; never
   fewer), so any line rg finds that codeq misses is a correctness FAIL. A codeq
   crash / bad flag is an ERROR, kept distinct from a miss. This is the check
   that would have caught the `^`/`$` anchor regression and the trigram-MATCH
   substring drops — both of which passed the unit suite.

2. NAV — the realistic navigation an agent actually does (find definition, find
   references, who-calls, read one symbol, outline, imports), each against the
   grep/cat pipeline it would otherwise run. These are codeq's differentiator and
   aren't a clean single-tool superset, so they're scored on speed + output bytes
   (a token proxy at ~4 B/tok), not PASS/FAIL. `read-symbol` vs `cat file` is the
   headline token comparison.

Every action records wall-time and output bytes for both tools. Results go to
bench/results/<timestamp>.json (committed — history lives in git); run
bench/trend.py to see movement over time.

Usage:
    python3 bench/differential.py               # full corpus
    python3 bench/differential.py flask zod     # named repos only
"""
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone

import corpus  # noqa: E402  (same dir)

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CODEQ = os.path.join(ROOT, "target", "release", "codeq")
RESULTS = os.path.join(HERE, "results")
TIMING_RUNS = 3

# SEARCH battery: (name, codeq args after `search`, rg args after the fixed
# `rg -n --no-heading --no-messages`). The two must express the SAME query.
SEARCH_ACTIONS = [
    ("literal-return", ["return"],                    ["return"]),
    ("anchor-import",  ["^import"],                    ["^import"]),
    ("anchor-func",    ["^func "],                     ["^func "]),
    ("anchor-def",     ["^def "],                      ["^def "]),
    ("word-test",      ["-w", "test"],                 ["-w", "test"]),
    ("fixed-arrow",    ["--fixed", "=>"],              ["-F", "=>"]),
    ("alt-todo",       ["TODO|FIXME|XXX"],             ["TODO|FIXME|XXX"]),
    ("class-error",    ["[A-Z][a-zA-Z]+Error"],        ["[A-Z][a-zA-Z]+Error"]),
    ("semver",         ["[0-9]+\\.[0-9]+\\.[0-9]+"],   ["[0-9]+\\.[0-9]+\\.[0-9]+"]),
    ("ci-http",        ["-i", "http"],                 ["-i", "http"]),
]


def _timed(cmd, cwd):
    """Best-of-N wall time in ms, plus the last run's stdout bytes and exit code."""
    best, out, rc = float("inf"), b"", 0
    for _ in range(TIMING_RUNS):
        t0 = time.perf_counter()
        p = subprocess.run(cmd, cwd=cwd, capture_output=True)
        best = min(best, (time.perf_counter() - t0) * 1000)
        out, rc = p.stdout, p.returncode
    return best, out, rc


def codeq_lines(out):
    """(lineset, ok). ok=False => output wasn't valid codeq JSON (crash/bad flag)."""
    try:
        data = json.loads(out)["data"]
    except (ValueError, KeyError):
        return set(), False
    return {(m["path"], m["line"]) for m in data.get("matches", [])}, True


def rg_lines(out):
    lines = set()
    for raw in out.decode("utf-8", "replace").splitlines():
        parts = raw.split(":", 2)
        if len(parts) >= 2 and parts[1].isdigit():
            lines.add((parts[0], int(parts[1])))
    return lines


def search_battery(path):
    out = []
    for aname, cargs, rargs in SEARCH_ACTIONS:
        cq_ms, cq_out, cq_rc = _timed([CODEQ, "search", *cargs, "--limit", "0"], path)
        rg_ms, rg_out, rg_rc = _timed(["rg", "-n", "--no-heading", "--no-messages", *rargs], path)
        cq, cq_ok = codeq_lines(cq_out)
        rg = rg_lines(rg_out)
        missing = rg - cq
        if not cq_ok or cq_rc != 0 or rg_rc > 1:   # rg exits 1 on no-match (fine)
            verdict = "ERROR"
        elif missing:
            verdict = "FAIL"
        else:
            verdict = "PASS"
        out.append({
            "kind": "search", "name": aname, "verdict": verdict,
            "codeq_n": len(cq), "rg_n": len(rg),
            "missing": len(missing), "extra": len(cq - rg),
            "missing_sample": sorted(missing)[:5],
            "codeq_ms": round(cq_ms, 2), "base_ms": round(rg_ms, 2),
            "codeq_bytes": len(cq_out), "base_bytes": len(rg_out),
        })
    return out


def pick_target(path):
    """A representative symbol (name, file) for the nav battery, chosen via codeq
    so both tools look up the same thing. Deterministic: the median-by-name
    function/method with a >=6-char name (avoids trivial one-letter names)."""
    p = subprocess.run([CODEQ, "symbols", "--fields", "name,kind,path"],
                       cwd=path, capture_output=True)
    try:
        data = json.loads(p.stdout)["data"]
    except (ValueError, KeyError):
        return None
    cand = [(d["name"], d["path"]) for d in data
            if d.get("kind") in ("function", "method") and len(d.get("name", "")) >= 6]
    if not cand:
        return None
    cand.sort()
    return cand[len(cand) // 2]


def nav_battery(path):
    tgt = pick_target(path)
    if not tgt:
        return []
    name, f = tgt
    # (action, codeq args after CODEQ, baseline full argv). Baselines are the
    # chained tools an agent reaches for to get the same answer.
    specs = [
        ("definition",  [CODEQ, "definitions", name],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         rf"(fn|def|func|function|class|type|struct|interface)\s+{name}\b"]),
        ("references",  [CODEQ, "references", name],
                        ["rg", "-nw", "--no-heading", "--no-messages", name]),
        ("callers",     [CODEQ, "callers", name],
                        ["rg", "-n", "--no-heading", "--no-messages", rf"{name}\("]),
        ("read-symbol", [CODEQ, "read", f, "--symbol", name],
                        ["cat", f]),
        ("outline",     [CODEQ, "outline", f],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         r"^\s*(fn|def|func|function|class|type|struct|interface|public|private|const)\b", f]),
        ("imports",     [CODEQ, "imports", f],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         r"^\s*(import|from|use|require|include|using)\b", f]),
    ]
    out = []
    for aname, cargv, bargv in specs:
        cq_ms, cq_out, _ = _timed(cargv, path)
        b_ms, b_out, _ = _timed(bargv, path)
        out.append({
            "kind": "nav", "name": aname, "verdict": "INFO", "target": name,
            "codeq_ms": round(cq_ms, 2), "base_ms": round(b_ms, 2),
            "codeq_bytes": len(cq_out), "base_bytes": len(b_out),
        })
    return out


def run_repo(name, lang, path):
    t0 = time.perf_counter()
    subprocess.run([CODEQ, "index"], cwd=path, capture_output=True)
    index_ms = (time.perf_counter() - t0) * 1000
    actions = search_battery(path) + nav_battery(path)
    return {"lang": lang, "index_ms": round(index_ms, 2), "actions": actions}


def codeq_commit():
    p = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"],
                       capture_output=True, text=True)
    return p.stdout.strip()[:12]


def _median(xs):
    return round(sorted(xs)[len(xs) // 2], 3) if xs else None


def summarize(repos):
    acts = [a for r in repos.values() for a in r["actions"]]
    search = [a for a in acts if a["kind"] == "search"]
    nav = [a for a in acts if a["kind"] == "nav"]
    passed = sum(1 for a in search if a["verdict"] == "PASS")
    failed = sum(1 for a in search if a["verdict"] == "FAIL")
    errored = sum(1 for a in search if a["verdict"] == "ERROR")

    def ratios(group):
        t = [a["codeq_ms"] / a["base_ms"] for a in group if a["base_ms"] > 0]
        b = [a["codeq_bytes"] / a["base_bytes"] for a in group if a["base_bytes"] > 0]
        return _median(t), _median(b)

    s_time, s_bytes = ratios(search)
    n_time, n_bytes = ratios(nav)
    read = [a for a in nav if a["name"] == "read-symbol"]
    r_time, r_bytes = ratios(read)
    return {
        "search_total": len(search), "correctness_pass": passed,
        "correctness_fail": failed, "errors": errored,
        "actions_total": len(search),   # kept for trend.py back-compat
        "median_codeq_over_rg_time": s_time,     # search speed ratio
        "median_codeq_over_rg_bytes": s_bytes,   # search byte ratio
        "nav_total": len(nav),
        "median_nav_time": n_time, "median_nav_bytes": n_bytes,
        "median_read_symbol_bytes": r_bytes,     # headline token win vs `cat`
    }


def main():
    if not os.path.exists(CODEQ):
        sys.exit(f"build release binary first: cargo build --release  (missing {CODEQ})")
    want = set(sys.argv[1:])
    repos = {}
    for name, lang, path in corpus.repo_paths():
        if want and name not in want:
            continue
        print(f"  {name} ({lang}) ...", flush=True)
        repos[name] = run_repo(name, lang, path)

    result = {
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "codeq_commit": codeq_commit(),
        "repos": repos,
        "summary": summarize(repos),
    }
    os.makedirs(RESULTS, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    dest = os.path.join(RESULTS, f"{stamp}.json")
    with open(dest, "w") as f:
        json.dump(result, f, indent=2)

    s = result["summary"]
    print(f"\n  SEARCH correctness: {s['correctness_pass']}/{s['search_total']} PASS"
          f" ({s['correctness_fail']} FAIL, {s['errors']} ERROR)")
    print(f"  search   codeq/rg  time {s['median_codeq_over_rg_time']}x"
          f"  bytes {s['median_codeq_over_rg_bytes']}x   (<1 = codeq wins)")
    print(f"  nav      codeq/base time {s['median_nav_time']}x"
          f"  bytes {s['median_nav_bytes']}x")
    print(f"  read-symbol vs `cat` bytes: {s['median_read_symbol_bytes']}x   (token win)")
    for rname, r in repos.items():
        for a in r["actions"]:
            if a.get("verdict") == "FAIL":
                print(f"  FAIL  {rname}/{a['name']}: codeq missed {a['missing']} rg lines,"
                      f" e.g. {a['missing_sample'][:3]}")
            elif a.get("verdict") == "ERROR":
                print(f"  ERROR {rname}/{a['name']}: tool failure (bad flag / crash / no JSON)")
    print(f"\n  wrote {dest}")


if __name__ == "__main__":
    main()
