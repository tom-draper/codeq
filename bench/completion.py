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
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone

import corpus  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
CODEQ = corpus.CODEQ
RESULTS = os.path.join(HERE, "results")
WINDOW = 60  # lines an agent reads around a located symbol (a fair, modest chunk)
VENDORED = ("/vendor/", "/dist/", "/node_modules/", "/_static/", "/third_party/",
            "/testdata/", "/fixtures/", ".min.")


def run(cmd, path):
    """(milliseconds, stdout) — this harness reports in ms, unlike the others."""
    secs, out = corpus.timed_run(cmd, path)
    return secs * 1000, out


class Agent:
    """What one generic-tool agent pays for a task: output bytes, wall time, and
    tool calls. A call is one model turn, so commands that don't depend on each
    other's output share a call, the way Claude Code issues parallel tool calls
    and Codex joins commands in one shell invocation. Only a step that needs the
    previous step's output costs another call."""

    def __init__(self, path):
        self.path, self.calls, self.bytes, self.ms = path, 0, 0, 0.0

    def call(self, *cmds):
        if not cmds:
            return []
        self.calls += 1
        outs = []
        for c in cmds:
            dt, out = run(c, self.path)
            self.bytes += len(out)
            self.ms += dt
            outs.append(out)
        return outs

    def cost(self):
        return self.calls, self.bytes, self.ms


def measure(cmds, path):
    """Run a fixed argv sequence; return (calls, bytes, ms). Used for codeq,
    where every task is one command."""
    calls = nbytes = 0
    ms = 0.0
    for c in cmds:
        dt, out = run(c, path)
        calls += 1
        nbytes += len(out)
        ms += dt
    return calls, nbytes, ms


def rg(*args):
    """An agent's grep. Long lines are cut, so minified or generated files
    (docs HTML, bundles) don't flood the output and inflate the baseline."""
    return ["rg", "-n", "--no-heading", "--no-messages", "--max-columns", "300", *args]


def hits(out):
    """(file, line, text) for each `file:line:text` row of rg output."""
    rows = []
    for line in out.decode("utf-8", "replace").splitlines():
        p = line.split(":", 2)
        if len(p) == 3 and p[1].isdigit():
            rows.append((p[0], int(p[1]), p[2]))
    return rows


def def_window(f, ln):
    """Read a definition from its first line, like a Read with offset/limit."""
    return ["sed", "-n", f"{max(1, ln - 3)},{ln + WINDOW}p", f]


def site_window(f, ln):
    """Read around a hit, enough above it to see the enclosing definition."""
    return ["sed", "-n", f"{max(1, ln - WINDOW // 2)},{ln + WINDOW // 2}p", f]


# How an agent greps for a definition. Languages whose methods carry no keyword
# (Java, C#, C++) miss here, and the agent falls back to `rg -w`.
DECL = r"\b(fn|def|func|fun|function|class|struct|interface|trait|enum|type|module)\s+"
IMPORT = r"^\s*(import|from|require|use|include|using)\b"
PUBLIC = r"^\s*(pub |export |public |private |protected |func |def |class |struct |interface |fn |impl |type )"


def pick(rows, known):
    """The hit an agent reading the output takes as the definition: the real one
    when it is listed (the agent recognises it), otherwise the first."""
    for r in rows:
        if known and (r[0], r[1]) == known:
            return r[0], r[1]
    return (rows[0][0], rows[0][1]) if rows else None


def locate(agent, names, known=None):
    """Find definitions: one declaration grep for every name, then one batched
    `rg -w` for the names it missed. Returns {name: (file, line)}."""
    known = known or {}
    pat = DECL + "(" + "|".join(re.escape(n) for n in names) + r")\b"
    (out,) = agent.call(rg("-e", pat))
    by_name = {}
    for f, ln, text in hits(out):
        for n in names:
            if re.search(DECL + re.escape(n) + r"\b", text):
                by_name.setdefault(n, []).append((f, ln, text))
    missing = [n for n in names if n not in by_name]
    for n, o in zip(missing, agent.call(*[rg("-w", n) for n in missing])):
        by_name[n] = hits(o)
    found = {}
    for n in names:
        loc = pick(by_name.get(n, []), known.get(n))
        if loc:
            found[n] = loc
    return found


def call_sites(out, exclude, k):
    """Up to k call-site hits from a `NAME(` grep, skipping the definition."""
    return [(f, ln) for f, ln, _ in hits(out) if (f, ln) != exclude][:k]


# ---- baselines: the generic-tool sequence an agent runs for the same answer
def base_find_def(path, S, F, m):
    """Jump to a definition: a declaration grep, `rg -w` if it misses."""
    a = Agent(path)
    locate(a, [S], {S: (F, m["def_line"])})
    return a.cost()


def base_enclosing(path, S, F, m):
    """Which definition encloses a line (a stack frame, a diff hunk): read the
    lines above it. Enough when the definition starts within 50 lines."""
    a = Agent(path)
    a.call(["sed", "-n", f"{max(1, m['line'] - 50)},{m['line'] + 10}p", F])
    return a.cost()


def base_members(path, S, F, m):
    """A type's fields and methods: find the declaration, read from it."""
    a = Agent(path)
    loc = locate(a, [m["type"]], {m["type"]: (m["type_file"], m["type_line"])})
    if m["type"] in loc:
        a.call(def_window(*loc[m["type"]]))
    return a.cost()


def base_importers(path, S, F, m):
    """Who imports a file: grep import lines naming its stem."""
    a = Agent(path)
    stem = re.escape(os.path.splitext(os.path.basename(F))[0])
    a.call(rg("-e", IMPORT + r".*\b" + stem + r"\b"))
    return a.cost()


def base_locate_read(path, S, F, m):
    """Read a function's implementation: find it, read from its first line."""
    a = Agent(path)
    loc = locate(a, [S], {S: (F, m["def_line"])})
    if S in loc:
        a.call(def_window(*loc[S]))
    return a.cost()


def base_understand(path, S, F, m):
    """Understand before editing: grep the definition and the call sites
    together, then read the definition and one caller."""
    a = Agent(path)
    decl, calls = a.call(rg("-e", DECL + re.escape(S) + r"\b"), rg("-e", re.escape(S) + r"\("))
    loc = pick(hits(decl) or hits(calls), (F, m["def_line"]))
    reads = [def_window(*loc)] if loc else []
    reads += [site_window(*h) for h in call_sites(calls, loc, 1)]
    a.call(*reads)
    return a.cost()


def base_callsites(path, S, F, m):
    """Who calls S: grep the call sites, then read around up to 5 of them to see
    which function each is in. codeq lists every caller, so the cap
    under-charges the baseline."""
    a = Agent(path)
    (calls,) = a.call(rg("-e", re.escape(S) + r"\("))
    a.call(*[site_window(*h) for h in call_sites(calls, (F, m["def_line"]), 5)])
    return a.cost()


def base_read_file(path, S, F, m):
    """Learn a file's structure: no generic shortcut, read the file."""
    a = Agent(path)
    a.call(["cat", F])
    return a.cost()


def base_rg_word(path, S, F, m):
    a = Agent(path)
    a.call(rg("-w", S))
    return a.cost()


def base_public_api(path, S, F, m):
    """A file's public API: grep declaration lines (their single-line signatures
    come with the hit). There's no cross-language grep for visibility, so this
    over-returns (privates included) and the agent still filters."""
    a = Agent(path)
    a.call(rg("-e", PUBLIC, F))
    return a.cost()


def base_package_api(path, S, F, m):
    """A package's public API: the same declaration grep over the directory."""
    a = Agent(path)
    a.call(rg("-e", PUBLIC, os.path.dirname(F) or "."))
    return a.cost()


def base_rg_imports(path, S, F, m):
    """The file's imports. A line grep finds single-line imports, but a grouped
    import (`import (` in Go, `use a::{` in Rust) matches only its opener, so the
    agent reads each opened block, all in one follow-up call."""
    a = Agent(path)
    (r,) = a.call(rg("-e", IMPORT, F))
    blocks = []
    for line in r.decode("utf-8", "replace").splitlines():
        num, _, text = line.partition(":")
        if num.isdigit() and text.rstrip().endswith(("(", "{")):
            blocks.append(["sed", "-n", f"{num},{int(num) + 20}p", F])
    a.call(*blocks)
    return a.cost()


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
    """A symbol and its neighbourhood with source, what `explore` returns in one
    call. Grep the definition and call sites together, read the definition and
    up to 3 call sites, then find and read up to 3 of the callees it names."""
    a = Agent(path)
    decl, calls = a.call(rg("-e", DECL + re.escape(S) + r"\b"), rg("-e", re.escape(S) + r"\("))
    loc = pick(hits(decl) or hits(calls), (F, m["def_line"]))
    reads = [def_window(*loc)] if loc else []
    reads += [site_window(*h) for h in call_sites(calls, loc, 3)]
    a.call(*reads)
    callees = codeq_callee_syms(path, S)[:3]
    if callees:
        found = locate(a, callees)
        a.call(*[def_window(*found[n]) for n in callees if n in found])
    return a.cost()


def base_blast_radius(path, S, F, m):
    """Two-hop impact: grep S's call sites, read around up to 5 to learn the
    enclosing callers, then grep calls to all of those in one search."""
    a = Agent(path)
    (calls,) = a.call(rg("-e", re.escape(S) + r"\("))
    a.call(*[site_window(*h) for h in call_sites(calls, (F, m["def_line"]), 5)])
    callers = codeq_caller_syms(path, S)[:5]
    if callers:
        a.call(rg("-e", "(" + "|".join(re.escape(c) for c in callers) + r")\("))
    return a.cost()


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
    """A file's dead code: list its definitions, then count the lines naming
    each one across the repo in one shell loop. A count of 1 is the definition
    alone, so the name is a candidate."""
    a = Agent(path)
    a.call(rg("-e", PUBLIC, F))
    names = file_def_names(path, F)
    if names:
        a.call(["sh", "-c",
                'for n in "$@"; do echo "$n $(rg -w --no-messages -- "$n" | wc -l)"; done',
                "sh", *names])
    return a.cost()


def base_callees(path, S, F, m):
    """What S calls: find S, then read its body. The calls are in the source, so
    one window is enough to list them (and to read what each is passed)."""
    a = Agent(path)
    loc = locate(a, [S], {S: (F, m["def_line"])})
    if S in loc:
        a.call(def_window(*loc[S]))
    return a.cost()


# A declaration that names the type in its header: `class X(T)`, `class X
# extends T`, `impl T for X`, `struct X : T`, `interface X extends T`.
HEADER = r"\b(class|struct|interface|impl|trait|enum|object|type)\b[^\n{;=]*\b"


def base_implementors(path, S, F, m):
    """Who implements or extends a type: one grep for declaration headers that
    name it. Cheap, but it cannot tell `extends T` from a field of type T."""
    a = Agent(path)
    a.call(rg("-e", HEADER + re.escape(m["impl_type"]) + r"\b"))
    return a.cost()


def base_untested(path, S, F, m):
    """A file's public definitions with no test: list its declarations, then count
    the uses of each name inside test-looking files in one shell loop."""
    a = Agent(path)
    a.call(rg("-e", PUBLIC, F))
    names = file_def_names(path, F)
    if names:
        globs = [x for g in TEST_GLOBS for x in ("-g", g)]
        a.call(["sh", "-c",
                'for n in "$@"; do echo "$n $(rg -w --no-messages ' + " ".join(f"'{g}'" for g in globs)
                + ' -- "$n" | wc -l)"; done',
                "sh", *names])
    return a.cost()


TEST_GLOBS = ("*test*", "*spec*", "*Test*", "**/test*/**", "**/spec/**", "**/Test*/**")


def base_which_tests(path, S, F, m):
    """Which tests exercise S: grep S in test-looking files, then read around up
    to 5 hits to see which test each is in."""
    a = Agent(path)
    (r,) = a.call(rg("-w", *[a for g in TEST_GLOBS for a in ("-g", g)], S))
    a.call(*[site_window(f, ln) for f, ln, _ in hits(r)[:5]])
    return a.cost()


# ---- tasks: (name, codeq argv builder, baseline fn, note)
TASKS = [
    ("find-def",        lambda S, F, m: [CODEQ, "definitions", S],
     base_find_def,     "jump to a definition (near parity)"),
    ("enclosing-def",   lambda S, F, m: [CODEQ, "at", f"{F}:{m['line']}"],
     base_enclosing,    "which definition encloses a line"),
    ("type-members",    lambda S, F, m: [CODEQ, "members", m["type"], "--path", m["type_file"]],
     base_members,      "a type's fields and methods vs locate + read"),
    ("file-importers",  lambda S, F, m: [CODEQ, "importers", F],
     base_importers,    "who imports a file (near parity)"),
    ("read-impl",       lambda S, F, m: [CODEQ, "read", F, "--symbol", S],
     base_locate_read,  "read a function's body"),
    ("understand-sym",  lambda S, F, m: [CODEQ, "about", S],
     base_understand,   "def + callers + callees before editing"),
    ("explore",         lambda S, F, m: [CODEQ, "explore", S, "--max-tokens", "8000"],
     base_explore,      "def + callers + callees WITH source, one call vs a grep+read loop"),
    ("callsites",       lambda S, F, m: [CODEQ, "callers", S],
     base_callsites,    "who calls it"),
    ("callees",         lambda S, F, m: [CODEQ, "callees", S, "--path", F],
     base_callees,      "what a function calls vs locate + read its body"),
    ("implementors",    lambda S, F, m: [CODEQ, "implementors", m["impl_type"]],
     base_implementors, "who extends or implements a type vs a header grep"),
    ("blast-radius",    lambda S, F, m: [CODEQ, "callers", S, "--depth", "2"],
     base_blast_radius, "2-hop caller tree vs iterative grep rounds"),
    ("which-tests",     lambda S, F, m: [CODEQ, "tests", S],
     base_which_tests,  "tests exercising S vs grep-in-tests + read"),
    ("untested-api",    lambda S, F, m: [CODEQ, "untested", F],
     base_untested,     "a file's public definitions with no test vs counting uses in tests"),
    ("dead-code",       lambda S, F, m: [CODEQ, "unused", F, "--include-tests"],
     base_dead_code,    "a file's dead code vs counting each symbol's uses"),
    ("file-structure",  lambda S, F, m: [CODEQ, "outline", F],
     base_read_file,    "learn a file's API/shape"),
    ("public-api",      lambda S, F, m: [CODEQ, "api", F],
     base_public_api,   "a file's exported API vs grepping decls and filtering"),
    ("package-api",     lambda S, F, m: [CODEQ, "api", os.path.dirname(F) or "."],
     base_package_api,  "a package's exported API vs grepping decls"),
    ("usages",          lambda S, F, m: [CODEQ, "references", S, "--limit", "0"],
     base_rg_word,      "all references (near-parity with rg)"),
    ("file-imports",    lambda S, F, m: [CODEQ, "imports", F],
     base_rg_imports,   "what a file imports (near-parity)"),
]


def is_test(d):
    """A test, by path or name. Asking who calls a test, or which tests use it,
    has no meaningful answer, so targets skip them."""
    return (re.search(r"(^|/)(tests?|spec|__tests__)(/|$)|[._-](test|spec)\.|_test\.|Tests?\.",
                      d["path"]) is not None
            or re.match(r"(test|Test|spec)", d.get("name", "")) is not None)


def pick_target(path):
    # --json here: we parse the symbol table. The task commands below measure the
    # default (text) output — the real agent cost we're comparing.
    p = subprocess.run([CODEQ, "--json", "symbols", "--fields", "name,kind,path,line,end_line",
                        "--limit", "0"],
                       cwd=path, capture_output=True).stdout
    try:
        data = json.loads(p)["data"]
    except (ValueError, KeyError):
        return None
    cand = [d for d in data
            if d.get("kind") in ("function", "method")
            and d.get("name", "").isidentifier() and len(d.get("name", "")) >= 5
            and not any(v in "/" + d["path"] for v in VENDORED)
            and not is_test(d)]
    cand.sort(key=lambda d: (d["path"], d.get("line", 0)))
    if not cand:
        return None
    s = cand[len(cand) // 2]
    # A line inside the body, for `at`, and a type for `members` (the median type,
    # chosen the same way). Repos with no type skip that task.
    m = {"def_line": s["line"], "line": (s["line"] + s.get("end_line", s["line"])) // 2}
    types = [d for d in data
             if d.get("kind") in ("struct", "class", "interface", "trait", "enum")
             and d.get("name", "").isidentifier() and len(d.get("name", "")) >= 3
             and not any(v in "/" + d["path"] for v in VENDORED)]
    types.sort(key=lambda d: (d["path"], d.get("line", 0)))
    if types:
        t = types[len(types) // 2]
        m["type"], m["type_file"], m["type_line"] = t["name"], t["path"], t["line"]
    # A type something extends or implements, for `implementors`: the median
    # such type by name. Repos with no relation edges skip that task.
    g = subprocess.run([CODEQ, "--json", "graph", "--limit", "0"],
                       cwd=path, capture_output=True).stdout
    try:
        edges = json.loads(g)["data"]
    except (ValueError, KeyError):
        edges = []
    bases = sorted({e["to"] for e in edges
                    if e.get("relation") in ("extends", "implements")
                    and e.get("to", "").isidentifier() and len(e["to"]) >= 3})
    if bases:
        m["impl_type"] = bases[len(bases) // 2]
    return s["name"], s["path"], m


def run_repo(path):
    shutil.rmtree(os.path.join(path, ".codeq"), ignore_errors=True)
    index_ms, _ = run([CODEQ, "index"], path)  # one-off cost, reported separately
    tgt = pick_target(path)
    if not tgt:
        return None
    S, F, m = tgt
    out = {}
    for name, cq_build, base_fn, _note in TASKS:
        if name == "type-members" and "type" not in m:
            continue
        if name == "implementors" and "impl_type" not in m:
            continue
        cc, cb, ct = measure([cq_build(S, F, m)], path)
        bc, bb, bt = base_fn(path, S, F, m)
        out[name] = {"codeq": {"calls": cc, "bytes": cb, "ms": round(ct, 1)},
                     "baseline": {"calls": bc, "bytes": bb, "ms": round(bt, 1)}}
    return {"target": S, "file": F, "index_ms": round(index_ms, 1), "tasks": out}


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
          f"{'cq calls':>9} {'base calls':>11} {'cq ms':>8} {'base ms':>8}")
    for name, *_ in TASKS:
        cq, bl = agg[name]["codeq"], agg[name]["baseline"]
        for side, v in (("codeq", cq), ("baseline", bl)):
            for i in range(3):
                tot[side][i] += v[i]
        ratio = f"{tok(bl[1]) / max(tok(cq[1]), 1):.1f}x"
        print(f"  {name:16} {tok(cq[1]):>10} {tok(bl[1]):>10} {ratio:>8} "
              f"{cq[0]:>9} {bl[0]:>11} {cq[2]:>8.0f} {bl[2]:>8.0f}")
    cqt, blt = tok(tot["codeq"][1]), tok(tot["baseline"][1])
    print(f"  {'-'*16}")
    print(f"  {'TOTAL':16} {cqt:>10} {blt:>10} {blt / max(cqt,1):>7.1f}x "
          f"{tot['codeq'][0]:>9} {tot['baseline'][0]:>11} "
          f"{tot['codeq'][2]:>8.0f} {tot['baseline'][2]:>8.0f}")
    index_ms = sum(r["index_ms"] for r in repos.values())
    print(f"  one-off index build across {len(repos)} repos: {index_ms:.0f} ms")
    print(f"\n  Across {len(repos)} repos, completing these {len(TASKS)} tasks costs an agent "
          f"{blt/max(cqt,1):.1f}x the tokens\n  and {tot['baseline'][0]}/{tot['codeq'][0]} "
          f"the tool calls with generic tools vs codeq.")

    result = {
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "codeq_commit": corpus.codeq_commit(),
        "repos": len(repos),
        "totals": {"codeq_tokens": cqt, "baseline_tokens": blt,
                   "token_ratio": round(blt / max(cqt, 1), 2),
                   "codeq_calls": tot["codeq"][0], "baseline_calls": tot["baseline"][0],
                   "codeq_ms": round(tot["codeq"][2], 1),
                   "baseline_ms": round(tot["baseline"][2], 1),
                   "index_ms": round(index_ms, 1)},
        "per_task": {name: {"codeq_tokens": tok(agg[name]["codeq"][1]),
                            "baseline_tokens": tok(agg[name]["baseline"][1]),
                            "codeq_calls": agg[name]["codeq"][0],
                            "baseline_calls": agg[name]["baseline"][0],
                            "codeq_ms": round(agg[name]["codeq"][2], 1),
                            "baseline_ms": round(agg[name]["baseline"][2], 1)}
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
