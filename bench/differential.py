#!/usr/bin/env python3
"""Differential correctness + efficiency harness: codeq vs ripgrep, many repos.

For every repo in the corpus (bench/corpus.py) this runs a battery of realistic
read-only search actions through BOTH `codeq search` and an equivalent `rg`
invocation, then checks the property codeq actually promises:

    codeq's matches must be a SUPERSET of ripgrep's — it may find more
    (documented: cross-line, wildcard-broadened prefilters) but must never find
    fewer. Any line rg finds that codeq misses is a correctness BUG.

This is the check that would have caught the `^`/`$` anchor regression (codeq
returned 0 for `^import`) and the trigram-MATCH substring drops — both of which
passed the unit suite. Alongside correctness it records wall-time and output
bytes (a token proxy at ~4 B/tok) for each tool, so we track speed and token
cost too. Results are written to bench/results/<timestamp>.json; run
bench/trend.py to see movement over time.

Usage:
    python3 bench/differential.py            # full corpus
    python3 bench/differential.py flask gin  # named repos only
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

# Each action is a search expressed once for each tool. `codeq` is the args after
# `search`; `rg` is the args after the fixed `rg -n --no-heading --no-messages`.
# The two must express the SAME query so the superset check is meaningful.
ACTIONS = [
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
    """Returns (lineset, ok). ok=False means the output wasn't valid codeq JSON
    (a crash, a bad flag) — a *tool* error, distinct from a genuine miss."""
    try:
        data = json.loads(out)["data"]
    except (ValueError, KeyError):
        return set(), False
    return {(m["path"], m["line"]) for m in data.get("matches", [])}, True


def rg_lines(out):
    lines = set()
    for raw in out.decode("utf-8", "replace").splitlines():
        # path:line:text  (path may contain no colon issues for these repos)
        parts = raw.split(":", 2)
        if len(parts) >= 2 and parts[1].isdigit():
            lines.add((parts[0], int(parts[1])))
    return lines


def run_repo(name, lang, path):
    # Build the index once (timed separately); every query below re-checks
    # freshness, so per-action times are honest end-to-end costs.
    t0 = time.perf_counter()
    subprocess.run([CODEQ, "index"], cwd=path, capture_output=True)
    index_ms = (time.perf_counter() - t0) * 1000

    actions = []
    for aname, cargs, rargs in ACTIONS:
        cq_ms, cq_out, cq_rc = _timed([CODEQ, "search", *cargs, "--limit", "0"], path)
        rg_ms, rg_out, rg_rc = _timed(["rg", "-n", "--no-heading", "--no-messages", *rargs], path)
        cq, cq_ok = codeq_lines(cq_out)
        rg = rg_lines(rg_out)
        missing = rg - cq          # rg found, codeq didn't  -> correctness bug
        extra = cq - rg            # codeq found, rg didn't   -> allowed superset
        # rg exits 1 on "no matches" (not an error); >1 is a real failure.
        if not cq_ok or cq_rc != 0 or rg_rc > 1:
            verdict = "ERROR"       # a tool/flag failure, not a missed match
        elif missing:
            verdict = "FAIL"        # codeq is not a superset of rg
        else:
            verdict = "PASS"
        actions.append({
            "name": aname,
            "verdict": verdict,
            "codeq_n": len(cq), "rg_n": len(rg),
            "missing": len(missing), "extra": len(extra),
            "missing_sample": sorted(missing)[:5],
            "codeq_ms": round(cq_ms, 2), "rg_ms": round(rg_ms, 2),
            "codeq_bytes": len(cq_out), "rg_bytes": len(rg_out),
        })
    return {"lang": lang, "index_ms": round(index_ms, 2), "actions": actions}


def codeq_commit():
    p = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"],
                       capture_output=True, text=True)
    return p.stdout.strip()[:12]


def summarize(repos):
    acts = [a for r in repos.values() for a in r["actions"]]
    passed = sum(1 for a in acts if a["verdict"] == "PASS")
    failed = sum(1 for a in acts if a["verdict"] == "FAIL")
    errored = sum(1 for a in acts if a["verdict"] == "ERROR")
    # speedup / token-ratio over comparable actions only (both tools succeeded)
    speed, toks = [], []
    for a in acts:
        if a["verdict"] == "ERROR":
            continue
        if a["rg_ms"] > 0:
            speed.append(a["codeq_ms"] / a["rg_ms"])
        if a["rg_bytes"] > 0:
            toks.append(a["codeq_bytes"] / a["rg_bytes"])
    med = lambda xs: round(sorted(xs)[len(xs) // 2], 3) if xs else None
    return {
        "actions_total": len(acts),
        "correctness_pass": passed,
        "correctness_fail": failed,
        "errors": errored,
        "median_codeq_over_rg_time": med(speed),
        "median_codeq_over_rg_bytes": med(toks),
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
    print(f"\n  correctness: {s['correctness_pass']}/{s['actions_total']} PASS"
          f" ({s['correctness_fail']} FAIL, {s['errors']} ERROR)")
    print(f"  codeq/rg time  (median): {s['median_codeq_over_rg_time']}x  (<1 = codeq faster)")
    print(f"  codeq/rg bytes (median): {s['median_codeq_over_rg_bytes']}x")
    for rname, r in repos.items():
        for a in r["actions"]:
            if a["verdict"] == "FAIL":
                print(f"  FAIL  {rname}/{a['name']}: codeq missed {a['missing']} rg lines,"
                      f" e.g. {a['missing_sample'][:3]}")
            elif a["verdict"] == "ERROR":
                print(f"  ERROR {rname}/{a['name']}: tool failure (bad flag / crash / no JSON)")
    print(f"\n  wrote {dest}")


if __name__ == "__main__":
    main()
