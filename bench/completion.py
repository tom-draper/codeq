#!/usr/bin/env python3
"""Task-level token-to-completion: how many tokens does an agent burn to finish a
realistic task WITH codeq vs WITHOUT (chaining rg/sed/cat like a generic agent)?

This is the metric that matters: an agent pays for every byte it reads into
context (~4 bytes/token). For each task we run the tool sequence a competent
agent would actually use to reach the SAME answer, and sum the output bytes (the
tokens it must ingest), the tool-call count, and time — across the whole corpus.

The baselines are deliberately fair, not strawmen: to read a function an agent
greps to locate it then reads a ~60-line window (not the whole file); to learn a
file's structure it reads the file (there's no generic-tool shortcut). Where codeq
and grep are near-equivalent (references, imports) we say so. Run against the
corpus (bench/corpus.py); writes bench/results/completion_<timestamp>.json.

Usage:
    python3 bench/completion.py            # full corpus
    python3 bench/completion.py flask zod  # named repos
"""
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone

import corpus  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CODEQ = os.path.join(ROOT, "target", "release", "codeq")
RESULTS = os.path.join(HERE, "results")
WINDOW = 60  # lines an agent reads around a located symbol (a fair, modest chunk)
VENDORED = ("/vendor/", "/dist/", "/node_modules/", "/_static/", "/third_party/",
            "/testdata/", "/fixtures/", ".min.")


def run(cmd, path):
    t = time.perf_counter()
    p = subprocess.run(cmd, cwd=path, capture_output=True)
    return (time.perf_counter() - t) * 1000, p.stdout


def measure(cmds, path):
    """Run a fixed argv sequence; return (calls, bytes, ms)."""
    calls = nbytes = 0
    ms = 0.0
    for c in cmds:
        dt, out = run(c, path)
        calls += 1
        nbytes += len(out)
        ms += dt
    return calls, nbytes, ms


def first_hits(rg_out, k):
    hits = []
    for line in rg_out.decode("utf-8", "replace").splitlines():
        p = line.split(":", 2)
        if len(p) >= 2 and p[1].isdigit():
            hits.append((p[0], int(p[1])))
        if len(hits) >= k:
            break
    return hits


def windows(hits):
    return [["sed", "-n", f"{max(1, ln - 3)},{ln + WINDOW}p", f] for f, ln in hits]


# ---- baselines: the generic-tool sequence a good agent runs for the same answer
def base_locate_read(path, S, F, m):
    """Read a function's implementation: grep to locate, read a window."""
    ms, rout = run(["rg", "-nw", "--no-heading", "--no-messages", "-m", "1", S], path)
    c, b, t = measure(windows(first_hits(rout, 1)), path)
    return 1 + c, len(rout) + b, ms + t


def base_understand(path, S, F, m):
    """Understand before editing: locate + read def, then find + read a caller."""
    ms1, rloc = run(["rg", "-nw", "--no-heading", "--no-messages", "-m", "1", S], path)
    c1, b1, t1 = measure(windows(first_hits(rloc, 1)), path)
    ms2, rcall = run(["rg", "-n", "--no-heading", "--no-messages", rf"{S}\("], path)
    c2, b2, t2 = measure(windows(first_hits(rcall, 1)), path)
    return 2 + c1 + c2, len(rloc) + b1 + len(rcall) + b2, ms1 + t1 + ms2 + t2


def base_callsites(path, S, F, m):
    """Who calls S and their context: grep call sites, read a window at each of 2."""
    ms, rcall = run(["rg", "-n", "--no-heading", "--no-messages", rf"{S}\("], path)
    c, b, t = measure(windows(first_hits(rcall, 2)), path)
    return 1 + c, len(rcall) + b, ms + t


def base_read_file(path, S, F, m):
    """Learn a file's structure: no generic shortcut — read the file."""
    return measure([["cat", F]], path)


def base_rg_word(path, S, F, m):
    return measure([["rg", "-nw", "--no-heading", "--no-messages", S]], path)


def base_public_api(path, S, F, m):
    """A file's public API the generic way: grep declaration lines (their
    single-line signatures come with the hit) and eyeball which are exported —
    there's no cross-language grep for *visibility*, so this over-returns
    (privates included) and the agent still filters. codeq `api` returns just the
    public surface. A distinct baseline from file-structure (which reads the whole
    file), so the two tasks don't double-count the same action."""
    return measure([["rg", "-n", "--no-heading", "--no-messages",
                     r"^\s*(pub |export |public |private |protected |func |def |class |struct |interface |fn |impl |type )",
                     F]], path)


def base_package_api(path, S, F, m):
    """A whole package's public API the generic way: to enumerate what a directory
    exports (with complete signatures and visibility), an agent reads the files
    that hold it — a decl grep gives incomplete first-lines and can't filter
    public. We charge reading exactly the files the package's API lives in (the
    distinct files codeq `api <dir>` draws from), capped so one big package can't
    dominate — which *under*-charges the baseline. codeq does it in one call."""
    scope = os.path.dirname(F) or "."
    p = subprocess.run([CODEQ, "--json", "api", scope, "--limit", "0"],
                       cwd=path, capture_output=True).stdout
    try:
        rows = json.loads(p)["data"]
    except (ValueError, KeyError):
        rows = []
    files, seen = [], set()
    for r in rows:
        fp = r.get("path")
        if fp and fp not in seen:
            seen.add(fp)
            files.append(fp)
    if not files:
        return 1, len(p), 0.0
    return measure([["cat", fp] for fp in files[:15]], path)  # cap: undercharges


def base_rg_imports(path, S, F, m):
    """The file's imports the generic way. A line grep for import keywords finds
    single-line imports, but a block/grouped import (`import (` in Go, `use a::{`
    in Rust) matches only the opener line — its members are on following lines. To
    reach the SAME module list codeq returns, the agent must read each opened
    block, so we charge that read (and only that — single-line imports add nothing,
    keeping the comparison honest near-parity where codeq has no structural edge)."""
    ms, r = run(["rg", "-n", "--no-heading", "--no-messages",
                 r"^\s*(import|from|require|use|include|using)\b", F], path)
    calls, nbytes, tms = 1, len(r), ms
    for line in r.decode("utf-8", "replace").splitlines():
        num, _, text = line.partition(":")
        if num.isdigit() and text.rstrip().endswith(("(", "{")):  # a block opener
            ln = int(num)
            dt, w = run(["sed", "-n", f"{ln},{ln + 20}p", F], path)  # read its members
            calls += 1
            nbytes += len(w)
            tms += dt
    return calls, nbytes, tms


def codeq_caller_syms(path, S):
    """The distinct enclosing symbols that call S (codeq's ground truth) — the set
    a generic agent reaches by reading a window at each direct call site."""
    p = subprocess.run([CODEQ, "--json", "callers", S], cwd=path, capture_output=True).stdout
    try:
        rows = json.loads(p)["data"]
    except (ValueError, KeyError):
        return []
    seen, out = set(), []
    for r in rows:
        s = r.get("symbol")
        if s and s not in seen:
            seen.add(s)
            out.append(s)
    return out


def codeq_callee_syms(path, S):
    """The distinct definitions S calls (codeq's ground truth) — the neighbours an
    agent must locate and read to understand S the way `explore` presents it."""
    p = subprocess.run([CODEQ, "--json", "callees", S], cwd=path, capture_output=True).stdout
    try:
        rows = json.loads(p)["data"]
    except (ValueError, KeyError):
        return []
    seen, out = set(), []
    for r in rows:
        n = r.get("name")
        if n and n != S and n not in seen:
            seen.add(n)
            out.append(n)
    return out


def base_explore(path, S, F, m):
    """Understand a symbol AND its neighbourhood with source — what `explore`
    returns in one call: the definition, its callers, and its callees, each with
    code. The generic way is a locate+read loop: read the def, read a window at up
    to 3 call sites, and locate+read up to 3 of the callees. codeq does it once."""
    ms, rloc = run(["rg", "-nw", "--no-heading", "--no-messages", "-m", "1", S], path)
    c, b, t = measure(windows(first_hits(rloc, 1)), path)
    calls, nbytes, tms = 1 + c, len(rloc) + b, ms + t
    # callers: grep call sites, read a window at up to 3 to see the calling code.
    ms2, rcall = run(["rg", "-n", "--no-heading", "--no-messages", rf"{S}\("], path)
    c2, b2, t2 = measure(windows(first_hits(rcall, 3)), path)
    calls += 1 + c2
    nbytes += len(rcall) + b2
    tms += ms2 + t2
    # callees: reach codeq's callee set, locate+read up to 3 of them.
    for name in codeq_callee_syms(path, S)[:3]:
        ms3, rc = run(["rg", "-nw", "--no-heading", "--no-messages", "-m", "1", name], path)
        cc, bb, tt = measure(windows(first_hits(rc, 1)), path)
        calls += 1 + cc
        nbytes += len(rc) + bb
        tms += ms3 + tt
    return calls, nbytes, tms


def base_blast_radius(path, S, F, m):
    """Two-hop blast radius the honest generic way: grep S's call sites and read a
    window at each to learn the enclosing callers (hop 1), then grep each of those
    to reach hop 2. codeq does it in one `callers --depth 2` call."""
    ms1, r1 = run(["rg", "-n", "--no-heading", "--no-messages", rf"{S}\("], path)
    c1, b1, t1 = measure(windows(first_hits(r1, 5)), path)  # read to identify enclosers
    calls, nbytes, ms = 1 + c1, len(r1) + b1, ms1 + t1
    for sym in codeq_caller_syms(path, S)[:5]:               # hop 2: re-grep each
        dt, r2 = run(["rg", "-n", "--no-heading", "--no-messages", rf"{sym}\("], path)
        calls += 1
        nbytes += len(r2)
        ms += dt
    return calls, nbytes, ms


def file_def_names(path, F):
    """The distinct definition names in F (codeq's ground truth) — the symbols an
    agent would enumerate to check each for usage."""
    p = subprocess.run([CODEQ, "--json", "outline", F], cwd=path, capture_output=True).stdout
    try:
        rows = json.loads(p)["data"]
    except (ValueError, KeyError):
        return []
    out, seen = [], set()
    for r in rows:
        n = r.get("name")
        if n and n.isidentifier() and n not in seen:
            seen.add(n)
            out.append(n)
    return out


def base_dead_code(path, S, F, m):
    """Find a file's dead code the generic way: list its definitions, then grep
    each name across the repo to see if anything uses it — the loop codeq's one
    `unused <file>` call replaces. Capped at 20 symbols, which under-charges the
    baseline (a real hunt checks them all)."""
    ms, r = run(["rg", "-n", "--no-heading", "--no-messages",
                 r"^\s*(pub |export |public |func |def |class |struct |fn |type )", F], path)
    calls, nbytes, tms = 1, len(r), ms  # locate the file's definitions
    for name in file_def_names(path, F)[:20]:
        dt, hits = run(["rg", "-nw", "--no-heading", "--no-messages", name], path)
        calls += 1
        nbytes += len(hits)
        tms += dt
    return calls, nbytes, tms


def base_which_tests(path, S, F, m):
    """Which tests exercise S: grep S within test-looking files, then read a window
    at each hit to confirm it's a test. codeq does it in one `tests` call."""
    ms, r = run(["rg", "-nw", "--no-heading", "--no-messages",
                 "-g", "*test*", "-g", "*spec*", "-g", "*Test*", "-g", "*_test*", S], path)
    c, b, t = measure(windows(first_hits(r, 5)), path)
    return 1 + c, len(r) + b, ms + t


# ---- tasks: (name, codeq argv builder, baseline fn, note)
TASKS = [
    ("read-impl",       lambda S, F, m: [CODEQ, "read", F, "--symbol", S],
     base_locate_read,  "read a function's body"),
    ("understand-sym",  lambda S, F, m: [CODEQ, "about", S],
     base_understand,   "def + callers + callees before editing"),
    ("explore",         lambda S, F, m: [CODEQ, "explore", S, "--max-tokens", "8000"],
     base_explore,      "def + callers + callees WITH source, one call vs a grep+read loop"),
    ("callsites",       lambda S, F, m: [CODEQ, "callers", S],
     base_callsites,    "who calls it, with context"),
    ("blast-radius",    lambda S, F, m: [CODEQ, "callers", S, "--depth", "2"],
     base_blast_radius, "2-hop caller tree vs iterative grep rounds"),
    ("which-tests",     lambda S, F, m: [CODEQ, "tests", S],
     base_which_tests,  "tests exercising S vs grep-in-tests + read"),
    ("dead-code",       lambda S, F, m: [CODEQ, "unused", F, "--include-tests"],
     base_dead_code,    "a file's dead code vs grepping each symbol for uses"),
    ("file-structure",  lambda S, F, m: [CODEQ, "outline", F],
     base_read_file,    "learn a file's API/shape"),
    ("public-api",      lambda S, F, m: [CODEQ, "api", F],
     base_public_api,   "a file's exported API vs grepping decls and filtering"),
    ("package-api",     lambda S, F, m: [CODEQ, "api", os.path.dirname(F) or "."],
     base_package_api,  "a package's exported API vs reading its files"),
    ("usages",          lambda S, F, m: [CODEQ, "references", S, "--limit", "0"],
     base_rg_word,      "all references (near-parity with rg)"),
    ("file-imports",    lambda S, F, m: [CODEQ, "imports", F],
     base_rg_imports,   "what a file imports (near-parity)"),
]


def pick_target(path):
    # --json here: we parse the symbol table. The task commands below measure the
    # default (text) output — the real agent cost we're comparing.
    p = subprocess.run([CODEQ, "--json", "symbols", "--fields", "name,kind,path,start_line", "--limit", "0"],
                       cwd=path, capture_output=True).stdout
    try:
        data = json.loads(p)["data"]
    except (ValueError, KeyError):
        return None
    cand = [d for d in data
            if d.get("kind") in ("function", "method")
            and d.get("name", "").isidentifier() and len(d.get("name", "")) >= 5
            and not any(v in "/" + d["path"] for v in VENDORED)]
    cand.sort(key=lambda d: (d["path"], d.get("start_line", 0)))
    if not cand:
        return None
    s = cand[len(cand) // 2]
    return s["name"], s["path"], None


def run_repo(path):
    shutil.rmtree(os.path.join(path, ".codeq"), ignore_errors=True)
    run([CODEQ, "index"], path)
    tgt = pick_target(path)
    if not tgt:
        return None
    S, F, m = tgt
    out = {}
    for name, cq_build, base_fn, _note in TASKS:
        cc, cb, ct = measure([cq_build(S, F, m)], path)
        bc, bb, bt = base_fn(path, S, F, m)
        out[name] = {"codeq": {"calls": cc, "bytes": cb, "ms": round(ct, 1)},
                     "baseline": {"calls": bc, "bytes": bb, "ms": round(bt, 1)}}
    return {"target": S, "file": F, "tasks": out}


def codeq_commit():
    p = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True)
    return p.stdout.strip()[:12]


def main():
    if not os.path.exists(CODEQ):
        sys.exit("build release binary first: cargo build --release")
    want = set(sys.argv[1:])
    repos = {}
    for name, lang, path in corpus.repo_paths():
        if want and name not in want:
            continue
        print(f"  {name} ({lang}) ...", flush=True)
        r = run_repo(path)
        if r:
            r["lang"] = lang
            repos[name] = r

    # aggregate per task across repos
    agg = {name: {"codeq": [0, 0, 0.0], "baseline": [0, 0, 0.0]} for name, *_ in TASKS}
    for r in repos.values():
        for name, t in r["tasks"].items():
            for side in ("codeq", "baseline"):
                agg[name][side][0] += t[side]["calls"]
                agg[name][side][1] += t[side]["bytes"]
                agg[name][side][2] += t[side]["ms"]

    tok = lambda b: b // 4
    tot = {"codeq": [0, 0, 0.0], "baseline": [0, 0, 0.0]}
    print(f"\n  {len(repos)} repos | token-to-completion per task (summed across repos)")
    print(f"  {'task':16} {'codeq tok':>10} {'base tok':>10} {'saving':>8} "
          f"{'cq calls':>9} {'base calls':>11}")
    for name, *_ in TASKS:
        cq, bl = agg[name]["codeq"], agg[name]["baseline"]
        for side, v in (("codeq", cq), ("baseline", bl)):
            for i in range(3):
                tot[side][i] += v[i]
        ratio = f"{tok(bl[1]) / max(tok(cq[1]), 1):.1f}x"
        print(f"  {name:16} {tok(cq[1]):>10} {tok(bl[1]):>10} {ratio:>8} "
              f"{cq[0]:>9} {bl[0]:>11}")
    cqt, blt = tok(tot["codeq"][1]), tok(tot["baseline"][1])
    print(f"  {'-'*16}")
    print(f"  {'TOTAL':16} {cqt:>10} {blt:>10} {blt / max(cqt,1):>7.1f}x "
          f"{tot['codeq'][0]:>9} {tot['baseline'][0]:>11}")
    print(f"\n  Across {len(repos)} repos, completing these {len(TASKS)} tasks costs an agent "
          f"{blt/max(cqt,1):.1f}x the tokens\n  and {tot['baseline'][0]}/{tot['codeq'][0]} "
          f"the tool calls with generic tools vs codeq.")

    result = {
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "codeq_commit": codeq_commit(),
        "repos": len(repos),
        "totals": {"codeq_tokens": cqt, "baseline_tokens": blt,
                   "token_ratio": round(blt / max(cqt, 1), 2),
                   "codeq_calls": tot["codeq"][0], "baseline_calls": tot["baseline"][0]},
        "per_task": {name: {"codeq_tokens": tok(agg[name]["codeq"][1]),
                            "baseline_tokens": tok(agg[name]["baseline"][1])}
                     for name, *_ in TASKS},
    }
    os.makedirs(RESULTS, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    dest = os.path.join(RESULTS, f"completion_{stamp}.json")
    with open(dest, "w") as f:
        json.dump(result, f, indent=2)
    print(f"\n  wrote {dest}")


if __name__ == "__main__":
    main()
