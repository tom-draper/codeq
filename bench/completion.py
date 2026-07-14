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


def base_rg_imports(path, S, F, m):
    return measure([["rg", "-n", "--no-heading", "--no-messages",
                     r"^\s*(import|from|require|use|include|using)\b", F]], path)


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
    ("callsites",       lambda S, F, m: [CODEQ, "callers", S],
     base_callsites,    "who calls it, with context"),
    ("blast-radius",    lambda S, F, m: [CODEQ, "callers", S, "--depth", "2"],
     base_blast_radius, "2-hop caller tree vs iterative grep rounds"),
    ("which-tests",     lambda S, F, m: [CODEQ, "tests", S],
     base_which_tests,  "tests exercising S vs grep-in-tests + read"),
    ("file-structure",  lambda S, F, m: [CODEQ, "outline", F],
     base_read_file,    "learn a file's API/shape"),
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
