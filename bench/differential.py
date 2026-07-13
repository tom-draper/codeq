#!/usr/bin/env python3
"""codeq verification + benefit harness: ~200 high-quality assertions per repo,
across the whole command surface, plus a per-command benefit report.

For every repo in the corpus (bench/corpus.py) it force-rebuilds the index with
the CURRENT binary and runs four batteries:

1. SEARCH  — curated regex/literal queries through `codeq search` vs equivalent
   `rg`. codeq must be a SUPERSET of ripgrep (never fewer matches); a miss is a
   FAIL, a crash/bad-flag is an ERROR. Records time + bytes for both.
2. FUZZ    — dozens of real identifiers sampled from codeq's own symbol table,
   each superset-checked against `rg -F`. Auto-scales the assertion count.
3. INVARIANT — ~6 checks per sampled symbol verifying definitions / references /
   at / read behave as expected, cross-checked against ground truth (rg + file
   contents). This is where a navigation/extraction regression shows up.
4. NAV     — the realistic navigation an agent does (definitions, references,
   callers, read, outline, imports) vs the grep/cat pipeline it would chain,
   measured on speed + output bytes (a token proxy).

Every run writes bench/results/<timestamp>.json (committed — history in git) and
prints a per-command report: correctness pass-rate + median codeq-vs-alternative
time and byte ratios, so it's obvious where codeq wins and where to improve.

Usage:
    python3 bench/differential.py              # full corpus
    python3 bench/differential.py flask zod    # named repos only
"""
import json
import os
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from datetime import datetime, timezone

import corpus  # noqa: E402  (same dir)

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CODEQ = os.path.join(ROOT, "target", "release", "codeq")
RESULTS = os.path.join(HERE, "results")
TIMING_RUNS = 3
FUZZ_K = 60        # identifiers superset-checked per repo
INVARIANT_K = 20   # symbols probed per repo (~6 invariants each)

# SEARCH battery: (name, codeq args after `search`, rg args after
# `rg -n --no-heading --no-messages`). Both must express the SAME query.
SEARCH_ACTIONS = [
    ("literal-return", ["return"],                    ["return"]),
    ("literal-null",   ["null"],                       ["null"]),
    ("word-test",      ["-w", "test"],                 ["-w", "test"]),
    ("word-error",     ["-w", "error"],                ["-w", "error"]),
    ("word-self",      ["-w", "self"],                 ["-w", "self"]),
    ("ci-http",        ["-i", "http"],                 ["-i", "http"]),
    ("ci-license",     ["-i", "license"],              ["-i", "license"]),
    ("fixed-arrow",    ["--fixed", "=>"],              ["-F", "=>"]),
    ("fixed-coloncolon", ["--fixed", "::"],            ["-F", "::"]),
    ("fixed-dotdot",   ["--fixed", ".."],              ["-F", ".."]),
    ("anchor-import",  ["^import"],                    ["^import"]),
    ("anchor-func",    ["^func "],                     ["^func "]),
    ("anchor-def",     ["^def "],                      ["^def "]),
    ("anchor-class",   ["^class "],                     ["^class "]),
    ("anchor-pub",     ["^pub "],                       ["^pub "]),
    ("anchor-hash",    ["^#"],                          ["^#"]),
    ("anchor-brace-eol", ["\\{$"],                     ["\\{$"]),
    ("anchor-semi-eol", [";$"],                         [";$"]),
    ("class-error",    ["[A-Z][a-zA-Z]+Error"],        ["[A-Z][a-zA-Z]+Error"]),
    ("class-hexcolor", ["#[0-9a-fA-F]{6}"],            ["#[0-9a-fA-F]{6}"]),
    ("class-const",    ["[A-Z][A-Z_]{3,}"],            ["[A-Z][A-Z_]{3,}"]),
    ("semver",         ["[0-9]+\\.[0-9]+\\.[0-9]+"],   ["[0-9]+\\.[0-9]+\\.[0-9]+"]),
    ("alt-todo",       ["TODO|FIXME|XXX|HACK"],        ["TODO|FIXME|XXX|HACK"]),
    ("alt-visibility", ["(public|private|protected)"], ["(public|private|protected)"]),
    ("optional",       ["colou?r"],                    ["colou?r"]),
    ("group-call",     ["\\.(map|filter|reduce)\\("],  ["\\.(map|filter|reduce)\\("]),
    ("word-boundary",  ["\\bconst\\b"],                ["\\bconst\\b"]),
    ("whitespace",     ["=\\s+function"],              ["=\\s+function"]),
    ("digits",         ["0x[0-9a-fA-F]+"],             ["0x[0-9a-fA-F]+"]),
    ("email-ish",      ["[a-z]+@[a-z]+\\.[a-z]+"],     ["[a-z]+@[a-z]+\\.[a-z]+"]),
]


def _timed(cmd, cwd):
    """Best-of-N wall time (ms), plus last stdout bytes and exit code."""
    best, out, rc = float("inf"), b"", 0
    for _ in range(TIMING_RUNS):
        t0 = time.perf_counter()
        p = subprocess.run(cmd, cwd=cwd, capture_output=True)
        best = min(best, (time.perf_counter() - t0) * 1000)
        out, rc = p.stdout, p.returncode
    return best, out, rc


def _run(cmd, cwd):
    return subprocess.run(cmd, cwd=cwd, capture_output=True).stdout


def _cq(argv, cwd):
    """Return (parsed data, raw bytes). data is None if output wasn't codeq JSON."""
    out = _run(argv, cwd)
    try:
        return json.loads(out)["data"], out
    except (ValueError, KeyError):
        return None, out


def codeq_lines(out):
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


# ---------------------------------------------------------------- SEARCH
def search_battery(path):
    out = []
    for aname, cargs, rargs in SEARCH_ACTIONS:
        cq_ms, cq_out, cq_rc = _timed([CODEQ, "--json", "search", *cargs, "--limit", "0"], path)
        rg_ms, rg_out, rg_rc = _timed(["rg", "-n", "--no-heading", "--no-messages", *rargs], path)
        cq, cq_ok = codeq_lines(cq_out)
        rg = rg_lines(rg_out)
        missing = rg - cq
        if not cq_ok or cq_rc != 0 or rg_rc > 1:
            verdict = "ERROR"
        elif missing:
            verdict = "FAIL"
        else:
            verdict = "PASS"
        out.append({
            "kind": "search", "command": "search", "name": aname, "verdict": verdict,
            "codeq_n": len(cq), "rg_n": len(rg),
            "missing": len(missing), "extra": len(cq - rg),
            "missing_sample": sorted(missing)[:5],
            "codeq_ms": round(cq_ms, 2), "base_ms": round(rg_ms, 2),
            "codeq_bytes": len(cq_out), "base_bytes": len(rg_out),
        })
    return out


# ---------------------------------------------------------------- FUZZ
def fuzz_battery(path):
    p = _run([CODEQ, "--json", "symbols", "--fields", "name", "--limit", "0"], path)
    try:
        data = json.loads(p)["data"]
    except (ValueError, KeyError):
        return {"total": 0, "pass": 0, "fail": 0, "failures": []}
    names = sorted({d["name"] for d in data
                    if len(d.get("name", "")) >= 6 and d["name"].isidentifier()})
    if not names:
        return {"total": 0, "pass": 0, "fail": 0, "failures": []}
    step = max(1, len(names) // FUZZ_K)
    sample = names[::step][:FUZZ_K]
    npass, failures = 0, []
    for lit in sample:
        cq_out = _run([CODEQ, "--json", "search", "--fixed", lit, "--limit", "0"], path)
        rg_out = _run(["rg", "-F", "-n", "--no-heading", "--no-messages", lit], path)
        cq, ok = codeq_lines(cq_out)
        missing = rg_lines(rg_out) - cq
        if ok and not missing:
            npass += 1
        else:
            failures.append({"literal": lit, "missing": len(missing),
                             "sample": sorted(missing)[:3], "crash": not ok})
    return {"total": len(sample), "pass": npass,
            "fail": len(sample) - npass, "failures": failures[:10]}


# ---------------------------------------------------------------- INVARIANTS
# Vendored / generated / minified blobs aren't code an agent navigates, and
# their pathological shapes (one 200KB line, hundreds of same-named idents)
# aren't representative — exclude them from the symbol sample.
VENDORED = ("/vendor/", "/dist/", "/node_modules/", "/_static/", "/third_party/",
            "/testdata/", "/fixtures/", ".min.")


def sample_symbols(path):
    p = _run([CODEQ, "--json", "symbols", "--fields", "name,kind,path,start_line,end_line", "--limit", "0"], path)
    try:
        data = json.loads(p)["data"]
    except (ValueError, KeyError):
        return []
    cand = [d for d in data
            if d.get("kind") in ("function", "method", "class", "struct")
            and d.get("name", "").isidentifier() and len(d.get("name", "")) >= 4
            and d.get("start_line") and d.get("end_line")
            and not any(v in "/" + d["path"] for v in VENDORED)]
    cand.sort(key=lambda d: (d["path"], d["start_line"]))
    if not cand:
        return []
    step = max(1, len(cand) // INVARIANT_K)
    return cand[::step][:INVARIANT_K]


def invariant_battery(path):
    """Per-symbol correctness invariants, cross-checked against ground truth.
    Verifies definitions / references / at / read do what they claim."""
    recs = []
    for sym in sample_symbols(path):
        S, F, L0, L1 = sym["name"], sym["path"], sym["start_line"], sym["end_line"]

        def rec(command, name, ok):
            recs.append({"kind": "invariant", "command": command, "name": name,
                         "verdict": "PASS" if ok else "FAIL", "symbol": S, "path": F})

        # definitions agrees with the symbol table
        defs, _ = _cq([CODEQ, "--json", "definitions", S], path)
        rec("definitions", "def-matches-symbol",
            bool(defs) and any(d.get("path") == F and d.get("start_line") == L0 for d in defs))

        # references includes the def site and is a superset of `rg -nw`
        # (--limit 0: the default caps results at 100, which isn't a miss).
        rd, _ = _cq([CODEQ, "--json", "references", S, "--limit", "0"], path)
        refset = {(r["path"], r["line"]) for r in rd.get("references", [])} if isinstance(rd, dict) else set()
        # The name may sit anywhere in the def's span (start_line can be a
        # leading annotation/attribute/decorator line), so check span membership.
        rec("references", "ref-includes-def",
            any(p == F and L0 <= ln <= L1 for (p, ln) in refset))
        rg_out = _run(["rg", "-nw", "--no-heading", "--no-messages", S], path)
        rec("references", "ref-superset-rg", not (rg_lines(rg_out) - refset))

        # `at` the def line resolves back to the symbol
        at0, _ = _cq([CODEQ, "--json", "at", f"{F}:{L0}"], path)
        rec("at", "at-def-roundtrip", bool(at0) and any(d.get("name") == S for d in at0))

        # `at` a line inside the body is enclosed by a def covering it
        if L1 > L0:
            mid = (L0 + L1) // 2
            atm, _ = _cq([CODEQ, "--json", "at", f"{F}:{mid}"], path)
            rec("at", "at-body-encloses",
                bool(atm) and any(d.get("start_line", 1 << 30) <= mid <= d.get("end_line", -1) for d in atm))

        # read --symbol returns the symbol's source: non-empty, contains the
        # name, and never more lines than the whole file (it may print several
        # defs when a name recurs, e.g. __init__, but never a super-file dump).
        rout = _run([CODEQ, "--json", "read", F, "--symbol", S], path)
        try:
            with open(os.path.join(path, F), "rb") as fh:
                file_lines = fh.read().count(b"\n") + 1
        except OSError:
            file_lines = 1 << 30
        nlines = rout.count(b"\n")
        rec("read", "read-symbol-faithful",
            len(rout) > 0 and S.encode() in rout and 0 < nlines <= file_lines + 1)
    return recs


# ---------------------------------------------------------------- NAV (benefit)
def nav_battery(path):
    syms = sample_symbols(path)
    if not syms:
        return []
    s = syms[len(syms) // 2]         # one representative symbol for timing
    name, f = s["name"], s["path"]
    specs = [
        ("definitions", [CODEQ, "--json", "definitions", name],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         rf"(fn|def|func|function|class|type|struct|interface)\s+{name}\b"]),
        ("references",  [CODEQ, "--json", "references", name, "--limit", "0"],
                        ["rg", "-nw", "--no-heading", "--no-messages", name]),
        ("callers",     [CODEQ, "--json", "callers", name],
                        ["rg", "-n", "--no-heading", "--no-messages", rf"{name}\("]),
        ("read",        [CODEQ, "--json", "read", f, "--symbol", name], ["cat", f]),
        ("outline",     [CODEQ, "--json", "outline", f],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         r"^\s*(fn|def|func|function|class|type|struct|interface|public|private|const)\b", f]),
        ("imports",     [CODEQ, "--json", "imports", f],
                        ["rg", "-n", "--no-heading", "--no-messages",
                         r"^\s*(import|from|use|require|include|using)\b", f]),
    ]
    out = []
    for aname, cargv, bargv in specs:
        cq_ms, cq_out, _ = _timed(cargv, path)
        b_ms, b_out, _ = _timed(bargv, path)
        out.append({
            "kind": "nav", "command": aname, "name": aname, "verdict": "INFO", "target": name,
            "codeq_ms": round(cq_ms, 2), "base_ms": round(b_ms, 2),
            "codeq_bytes": len(cq_out), "base_bytes": len(b_out),
        })
    return out


# ---------------------------------------------------------------- driver
def run_repo(name, lang, path):
    shutil.rmtree(os.path.join(path, ".codeq"), ignore_errors=True)  # always test current binary
    t0 = time.perf_counter()
    _run([CODEQ, "--json", "index"], path)
    index_ms = (time.perf_counter() - t0) * 1000
    actions = search_battery(path) + nav_battery(path)
    invariants = invariant_battery(path)
    fuzz = fuzz_battery(path)
    return {"lang": lang, "index_ms": round(index_ms, 2),
            "actions": actions, "invariants": invariants, "fuzz": fuzz}


def codeq_commit():
    p = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"],
                       capture_output=True, text=True)
    return p.stdout.strip()[:12]


def _median(xs):
    return round(sorted(xs)[len(xs) // 2], 3) if xs else None


def per_command(repos):
    corr = defaultdict(lambda: [0, 0])   # command -> [pass, total]
    tr, br = defaultdict(list), defaultdict(list)
    for r in repos.values():
        for a in r["actions"]:
            c = a["command"]
            if a["kind"] == "search":
                corr[c][1] += 1
                corr[c][0] += a["verdict"] == "PASS"
            if a.get("base_ms", 0) > 0:
                tr[c].append(a["codeq_ms"] / a["base_ms"])
            if a.get("base_bytes", 0) > 0:
                br[c].append(a["codeq_bytes"] / a["base_bytes"])
        for inv in r["invariants"]:
            c = inv["command"]
            corr[c][1] += 1
            corr[c][0] += inv["verdict"] == "PASS"
        corr["search"][0] += r["fuzz"]["pass"]
        corr["search"][1] += r["fuzz"]["total"]
    cmds = set(corr) | set(tr) | set(br)
    return {c: {"pass": corr[c][0], "total": corr[c][1],
                "time_x": _median(tr[c]), "bytes_x": _median(br[c])}
            for c in cmds}


def summarize(repos, pc):
    search = [a for r in repos.values() for a in r["actions"] if a["kind"] == "search"]
    inv = [i for r in repos.values() for i in r["invariants"]]
    fuzz_total = sum(r["fuzz"]["total"] for r in repos.values())
    fuzz_pass = sum(r["fuzz"]["pass"] for r in repos.values())
    corr_pass = (sum(a["verdict"] == "PASS" for a in search)
                 + sum(i["verdict"] == "PASS" for i in inv) + fuzz_pass)
    corr_total = len(search) + len(inv) + fuzz_total
    return {
        "repos": len(repos),
        "total_correctness_tests": corr_total,
        "correctness_pass": corr_pass,
        "correctness_fail": corr_total - corr_pass,
        "search_total": len(search),
        "search_fail": sum(a["verdict"] == "FAIL" for a in search),
        "errors": sum(a["verdict"] == "ERROR" for a in search),
        "invariant_total": len(inv),
        "invariant_fail": sum(i["verdict"] == "FAIL" for i in inv),
        "fuzz_total": fuzz_total, "fuzz_fail": fuzz_total - fuzz_pass,
        "actions_total": len(search),   # trend.py back-compat
        "median_codeq_over_rg_time": pc.get("search", {}).get("time_x"),
        "median_codeq_over_rg_bytes": pc.get("search", {}).get("bytes_x"),
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

    pc = per_command(repos)
    # Collect just the failures — the committed artifact stays small (KB, not MB);
    # the full per-action detail is printed to stdout below, not persisted.
    failures = []
    for rname, r in repos.items():
        for a in r["actions"]:
            if a.get("verdict") in ("FAIL", "ERROR"):
                failures.append({"repo": rname, "battery": "search", "name": a["name"],
                                 "verdict": a["verdict"], "missing": a.get("missing")})
        for i in r["invariants"]:
            if i["verdict"] == "FAIL":
                failures.append({"repo": rname, "battery": i["command"], "name": i["name"],
                                 "symbol": i["symbol"], "path": i["path"]})
        for fz in r["fuzz"]["failures"]:
            failures.append({"repo": rname, "battery": "fuzz", "literal": fz["literal"],
                             "missing": fz["missing"], "crash": fz["crash"]})
    result = {
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "codeq_commit": codeq_commit(),
        "per_command": pc,
        "summary": summarize(repos, pc),
        "per_repo": {n: {"lang": r["lang"], "index_ms": r["index_ms"],
                         "fuzz": {k: r["fuzz"][k] for k in ("total", "pass", "fail")}}
                     for n, r in repos.items()},
        "failures": failures,
    }
    os.makedirs(RESULTS, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    dest = os.path.join(RESULTS, f"{stamp}.json")
    with open(dest, "w") as fh:
        json.dump(result, fh, indent=2)

    s = result["summary"]
    print(f"\n  {s['repos']} repos | {s['total_correctness_tests']} correctness assertions "
          f"({s['correctness_pass']} PASS, {s['correctness_fail']} FAIL)")
    print(f"    search {s['search_total'] - s['search_fail']}/{s['search_total']}"
          f"  fuzz {s['fuzz_total'] - s['fuzz_fail']}/{s['fuzz_total']}"
          f"  invariant {s['invariant_total'] - s['invariant_fail']}/{s['invariant_total']}"
          f"  ({s['errors']} ERROR)")
    print(f"\n  per-command  (correctness | codeq/alt time | codeq/alt bytes; <1 = codeq wins)")
    print(f"  {'command':13} {'correct':>10} {'time x':>9} {'bytes x':>9}")
    for c in sorted(pc):
        v = pc[c]
        corr = f"{v['pass']}/{v['total']}" if v["total"] else "-"
        print(f"  {c:13} {corr:>10} {str(v['time_x']):>9} {str(v['bytes_x']):>9}")

    # surface failures for triage
    for rname, r in repos.items():
        for a in r["actions"]:
            if a.get("verdict") == "FAIL":
                print(f"  FAIL  {rname}/search/{a['name']}: missed {a['missing']} rg lines"
                      f" e.g. {a['missing_sample'][:3]}")
            elif a.get("verdict") == "ERROR":
                print(f"  ERROR {rname}/search/{a['name']}")
        for i in r["invariants"]:
            if i["verdict"] == "FAIL":
                print(f"  FAIL  {rname}/{i['command']}/{i['name']}: symbol '{i['symbol']}' ({i['path']})")
        for fz in r["fuzz"]["failures"]:
            print(f"  FAIL  {rname}/search(fuzz): '{fz['literal']}' missed {fz['missing']}"
                  f"{' CRASH' if fz['crash'] else ''}")
    print(f"\n  wrote {dest}")


if __name__ == "__main__":
    main()
