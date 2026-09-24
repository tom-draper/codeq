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
3. INVARIANT — ~8 checks per sampled symbol verifying definitions / references /
   at / read / explore / astgrep behave as expected, cross-checked against ground
   truth (rg + file contents), plus a handful of whole-repo checks (map --totals
   agrees with map, graph edges are named, the envelope's `rows` pointer
   resolves). This is where a navigation/extraction regression shows up.

   Symbols are sampled **stratified by kind**. They used to be filtered to
   function/method/class/struct, which meant the battery never touched a field, a
   variant, a module-scope constant or any SQL declaration — on flask alone that
   is 132 constants, 212 fields and 2 tables it could not see, and it is why the
   assertion count stayed flat across a run of extraction changes. A harness that
   cannot reach the new facts is not checking the new work.
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
CODEQ = corpus.CODEQ
RESULTS = os.path.join(HERE, "results")
TIMING_RUNS = 3
FUZZ_K = 60        # identifiers superset-checked per repo
INVARIANT_K = 30   # symbols probed per repo (~6 invariants each), spread across kinds

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


# The one place codeq is deliberately NARROWER than `rg -w`.
#
# `rg -w` anchors on \b, where `$` is a non-word character, so `-w locale`
# matches inside `this.$locale()`. codeq treats `$` as identifier syntax in the
# languages whose lexer does, so `$locale` is a different identifier and codeq
# does not answer for `locale` there. dayjs writes `this.$locale()` beside a real
# `locale` export, so both spellings are live in one file and the difference is
# the whole point — a decision about what `-w` means, not a miss.
#
# Carved out by the rule and not by name: these extensions only, and only when
# EVERY occurrence on the line is sigil-bound. One free-standing hit and the line
# still has to be answered. The exemptions are counted into the summary so a
# silent exemption cannot grow into a silent hole.
SIGIL_BINDS_EXT = (".js", ".mjs", ".cjs", ".jsx", ".ts", ".mts", ".cts", ".tsx",
                   ".svelte", ".vue", ".java", ".kt", ".kts", ".scala", ".sc")


def sigil_bound_lines(out, needle):
    """(path, line) where `rg -w` only matched inside a `$`-bound identifier."""
    if not needle or not needle.isidentifier():
        return set()
    bound = set()
    for raw in out.decode("utf-8", "replace").splitlines():
        parts = raw.split(":", 2)
        if len(parts) < 3 or not parts[1].isdigit():
            continue
        path, text = parts[0], parts[2]
        if not path.endswith(SIGIL_BINDS_EXT):
            continue
        hits, at = [], text.find(needle)
        while at != -1:
            hits.append(at)
            at = text.find(needle, at + 1)
        if hits and all(
            (i > 0 and text[i - 1] == "$")
            or (i + len(needle) < len(text) and text[i + len(needle)] == "$")
            for i in hits
        ):
            bound.add((path, int(parts[1])))
    return bound


# ---------------------------------------------------------------- SEARCH
def search_battery(path):
    out = []
    for aname, cargs, rargs in SEARCH_ACTIONS:
        cq_ms, cq_out, cq_rc = _timed([CODEQ, "--json", "search", *cargs, "--limit", "0"], path)
        rg_ms, rg_out, rg_rc = _timed(["rg", "-n", "--no-heading", "--no-messages", *rargs], path)
        cq, cq_ok = codeq_lines(cq_out)
        rg = rg_lines(rg_out)
        missing = rg - cq
        # Same `-w` rule as the references invariant, applied wherever `-w` is
        # the query, so one future `$`-bound hit doesn't read as a regression.
        exempt = missing & sigil_bound_lines(rg_out, rargs[-1]) if "-w" in rargs else set()
        missing -= exempt
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
            "sigil_exempt": len(exempt),
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


# Markdown headings are definitions, but their names are prose ("Output contract"),
# so the identifier filter below drops them anyway; naming it here says the
# omission is deliberate rather than accidental.
UNSAMPLED_KINDS = {"heading", "import"}


def sample_symbols(path):
    """A spread of symbols to run the invariant battery over — stratified by kind.

    This used to filter to ("function", "method", "class", "struct"), which meant
    the battery never touched a field, a variant, a module-scope constant, or any
    SQL declaration. Measured on flask alone that is 132 constants, 212 fields and
    2 tables the harness could not see, and it is why the assertion count sat flat
    across a day of extraction changes: the check that exists to catch what
    fixtures cannot was structurally unable to reach the new facts.

    Round-robin across kinds rather than a flat stride, so a kind with three
    instances in a repo is still sampled instead of being rounded away by a kind
    with a thousand.
    """
    p = _run([CODEQ, "--json", "symbols", "--fields", "name,kind,path,line,end_line", "--limit", "0"], path)
    try:
        data = json.loads(p)["data"]
    except (ValueError, KeyError):
        return []
    cand = [d for d in data
            if d.get("kind") not in UNSAMPLED_KINDS
            and d.get("name", "").isidentifier() and len(d.get("name", "")) >= 4
            and d.get("line") and d.get("end_line")
            and not any(v in "/" + d["path"] for v in VENDORED)]
    if not cand:
        return []
    by_kind = defaultdict(list)
    for d in sorted(cand, key=lambda d: (d["kind"], d["path"], d["line"])):
        by_kind[d["kind"]].append(d)
    # Spread within each kind first, so the sample is not all from one file.
    streams = []
    for kind in sorted(by_kind):
        ds = by_kind[kind]
        step = max(1, len(ds) // INVARIANT_K)
        streams.append(ds[::step])
    picked, i = [], 0
    while len(picked) < INVARIANT_K and any(i < len(st) for st in streams):
        for st in streams:
            if i < len(st) and len(picked) < INVARIANT_K:
                picked.append(st[i])
        i += 1
    return picked


def invariant_battery(path):
    """Per-symbol correctness invariants, cross-checked against ground truth.
    Verifies definitions / references / at / read do what they claim."""
    recs = []
    for sym in sample_symbols(path):
        S, F, L0, L1 = sym["name"], sym["path"], sym["line"], sym["end_line"]

        def rec(command, name, ok, exempt=0):
            r = {"kind": "invariant", "command": command, "name": name,
                 "verdict": "PASS" if ok else "FAIL", "symbol": S, "path": F}
            if exempt:
                r["sigil_exempt"] = exempt
            recs.append(r)

        # definitions agrees with the symbol table
        defs, _ = _cq([CODEQ, "--json", "definitions", S], path)
        rec("definitions", "def-matches-symbol",
            bool(defs) and any(d.get("path") == F and d.get("line") == L0 for d in defs))

        # references includes the def site and is a superset of `rg -nw`
        # (--limit 0: the default caps results at 100, which isn't a miss).
        rd, _ = _cq([CODEQ, "--json", "references", S, "--limit", "0"], path)
        refset = {(r["path"], r["line"]) for r in rd.get("references", [])} if isinstance(rd, dict) else set()
        # The name may sit anywhere in the def's span (line can be a
        # leading annotation/attribute/decorator line), so check span membership.
        rec("references", "ref-includes-def",
            any(p == F and L0 <= ln <= L1 for (p, ln) in refset))
        rg_out = _run(["rg", "-nw", "--no-heading", "--no-messages", S], path)
        missing = rg_lines(rg_out) - refset
        exempt = missing & sigil_bound_lines(rg_out, S)
        rec("references", "ref-superset-rg", not (missing - exempt), len(exempt))

        # `at` the def line resolves back to the symbol
        at0, _ = _cq([CODEQ, "--json", "at", f"{F}:{L0}"], path)
        rec("at", "at-def-roundtrip", bool(at0) and any(d.get("name") == S for d in at0))

        # `at` a line inside the body is enclosed by a def covering it
        if L1 > L0:
            mid = (L0 + L1) // 2
            atm, _ = _cq([CODEQ, "--json", "at", f"{F}:{mid}"], path)
            rec("at", "at-body-encloses",
                bool(atm) and any(d.get("line", 1 << 30) <= mid <= d.get("end_line", -1) for d in atm))

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

        # `explore --at` resolves a position to the same definition `at` does.
        # The two are documented as different routes to one answer, so they must
        # not disagree; nothing was checking that they don't.
        raw_ex = _run([CODEQ, "--json", "explore", "--at", f"{F}:{L0}",
                       "--max-tokens", "200"], path)
        try:
            env = json.loads(raw_ex)
        except ValueError:
            env = {}
        if isinstance(env.get("data"), dict):
            # `focus` is a list: a position can be enclosed by more than one
            # definition (a method inside a class), and explore keeps them.
            focus = env["data"].get("focus") or []
            # A line that declares several things (`attr_accessor :a, :b`, enum
            # variants) names no single symbol, and `explore --at` refuses with a
            # note rather than picking one. That refusal is a correct answer, so
            # the invariant is "resolves to the symbol, or explains why it can't"
            # — not "always resolves".
            refused = "declares" in (env.get("note") or "")
            rec("explore", "explore-at-matches-at",
                refused or any(d.get("name") == S and d.get("path") == F for d in focus))

        # `astgrep --explain` returns a tree that actually covers the line asked
        # about — the command exists so a pattern can be written from real node
        # kinds, which is worthless if it reports a node from somewhere else.
        # Skipped when the command produced no JSON — a language codeq indexes but
        # has no grammar for (CSS, HCL, markdown) errors out by design, and
        # `_cq` reports stdout, so the failure has to be read from *that* rather
        # than by grepping for a message that goes to stderr. The first run of
        # this check asserted against three CSS files and blamed codeq for it.
        exp, _ = _cq([CODEQ, "--json", "astgrep", "--explain", f"{F}:{L0}"], path)
        if isinstance(exp, list):
            rec("astgrep", "explain-covers-line",
                bool(exp) and all(n.get("line", 0) <= L1 for n in exp))
    return recs


def repo_battery(path):
    """Whole-repo invariants: results that must agree with each other."""
    recs = []

    def rec(command, name, ok):
        recs.append({"kind": "invariant", "command": command, "name": name,
                     "verdict": "PASS" if ok else "FAIL", "symbol": "-", "path": "-"})

    # `map --totals` is a view of `map`, not a second answer.
    full, _ = _cq([CODEQ, "--json", "map"], path)
    summ, _ = _cq([CODEQ, "--json", "map", "--totals"], path)
    if isinstance(full, dict) and isinstance(summ, dict):
        rec("map", "totals-view-agrees", full.get("totals") == summ.get("totals"))
        langs = full.get("languages") or []
        rec("map", "totals-reconcile",
            summ.get("totals", {}).get("bytes") == sum(l.get("bytes", 0) for l in langs))

    # Every relation edge names something; `--relation X` is a subset of all.
    edges, _ = _cq([CODEQ, "--json", "graph", "--limit", "0"], path)
    if isinstance(edges, list):
        rec("graph", "edges-are-named",
            all(e.get("from") and e.get("to") and e.get("relation") for e in edges))
        kinds = {e.get("relation") for e in edges}
        for k in sorted(x for x in kinds if x):
            sub, _ = _cq([CODEQ, "--json", "graph", "--relation", k, "--limit", "0"], path)
            sub = sub if isinstance(sub, list) else []
            rec("graph", "relation-filter-is-subset",
                len(sub) == sum(1 for e in edges if e.get("relation") == k))

    # An empty result explains itself. Eleven of twenty-six commands once
    # returned empty with no note, no stderr and exit 0, and they were fixed one
    # at a time as each was tripped over. This is the check that stops the
    # twelfth arriving quietly: every case below is empty *because a filter
    # selected nothing*, which is the ambiguous kind. A bare pattern miss is
    # deliberately not asserted — an empty `search zzz` is unambiguous.
    Z = "zzznotarealsymbol"
    for args in (
        ["definitions", Z], ["callers", Z], ["callees", Z], ["tests", Z],
        ["members", Z], ["implementors", Z], ["supertypes", Z],
        ["callpath", Z, "alsonotreal"],
        ["imports", "nosuchdir"], ["hotspots", "nosuchdir"], ["clones", "nosuchdir"],
        ["tree", "nosuchdir"], ["unused", "nosuchdir"], ["untested", "nosuchdir"],
        ["api", "nosuchdir"], ["context", "--symbols", Z],
        ["search", "fn", "--path", "nosuchdir"],
        ["astgrep", "$X.zzz()", "--language", "rust", "--path", "nosuchdir"],
    ):
        raw = _run([CODEQ, "--json", *args], path)
        try:
            env = json.loads(raw)
        except ValueError:
            continue
        d = env.get("data")
        if isinstance(d, list):
            empty = not d
        elif isinstance(d, dict):
            arrs = [v for v in d.values() if isinstance(v, list)]
            empty = bool(arrs) and all(not a for a in arrs)
        else:
            empty = False
        if empty:
            rec("empty-note", f"{args[0]}-explains-empty", bool(env.get("note")))

    # Row containers are self-describing: the `rows` pointer must resolve.
    for args in (["symbols", "--limit", "5"], ["search", "return", "--limit", "5"],
                 ["search", "return", "-l", "--limit", "5"]):
        raw = _run([CODEQ, "--json", *args], path)
        try:
            env = json.loads(raw)
        except ValueError:
            continue
        ptr = env.get("rows")
        cur = env
        ok = bool(ptr)
        for key in (ptr or "").split("."):
            if isinstance(cur, dict) and key in cur:
                cur = cur[key]
            else:
                ok = False
                break
        rec("envelope", "rows-pointer-resolves", ok and isinstance(cur, list))
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
    invariants = invariant_battery(path) + repo_battery(path)
    fuzz = fuzz_battery(path)
    return {"lang": lang, "index_ms": round(index_ms, 2),
            "actions": actions, "invariants": invariants, "fuzz": fuzz}


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
        # Lines `rg -w` matched inside a `$`-bound identifier, which codeq
        # deliberately does not answer for. Reported so the exemption stays
        # something you can watch rather than something that hides.
        "sigil_exempt": (sum(i.get("sigil_exempt", 0) for i in inv)
                         + sum(a.get("sigil_exempt", 0) for a in search)),
        "fuzz_total": fuzz_total, "fuzz_fail": fuzz_total - fuzz_pass,
        "actions_total": len(search),   # trend.py back-compat
        "median_codeq_over_rg_time": pc.get("search", {}).get("time_x"),
        "median_codeq_over_rg_bytes": pc.get("search", {}).get("bytes_x"),
    }


def assert_rg_searches_the_filesystem(path):
    """Fail loudly if `rg` is reading stdin instead of the repo.

    Every superset assertion here is `rg_lines(...) - codeq_lines(...)`, so an
    `rg` that returns nothing makes all of them pass — silently, and with a
    perfect score. That is exactly what happens when stdin is not a TTY: given
    no path argument, ripgrep searches stdin instead of the working directory,
    which is the normal condition under CI, a pipe, or a heredoc. The commands
    below deliberately pass no path, because adding one prefixes every result
    with `./` and would change what the byte-ratio metrics mean.

    So probe for it rather than defend against it: a repo with source in it must
    match `[a-z]` on at least one line.
    """
    if not rg_lines(_run(["rg", "-n", "--no-heading", "--no-messages", "[a-z]"], path)):
        sys.exit(
            "rg matched nothing in a non-empty repo — it is reading stdin, not the\n"
            "repository, so every superset assertion would pass vacuously.\n"
            "Run with a terminal on stdin (or `< /dev/tty`)."
        )


def main():
    if not os.path.exists(CODEQ):
        sys.exit(f"build release binary first: cargo build --release  (missing {CODEQ})")
    args = sys.argv[1:]
    if any(arg in ("-h", "--help") for arg in args):
        print(__doc__.strip())
        return
    known = {name for name, *_ in corpus.REPOS}
    unknown = sorted(set(args) - known)
    if unknown:
        choices = ", ".join(sorted(known))
        sys.exit(f"unknown corpus repo(s): {', '.join(unknown)}\nchoices: {choices}")
    want = set(args)
    repos = {}
    for name, lang, path in corpus.repo_paths():
        if want and name not in want:
            continue
        if not repos:
            assert_rg_searches_the_filesystem(path)
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
        "codeq_commit": corpus.codeq_commit(),
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
    if s.get("sigil_exempt"):
        print(f"    {s['sigil_exempt']} `rg -w` line(s) exempt: matched only inside a "
              f"$-bound identifier, which codeq answers as a different name")
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
