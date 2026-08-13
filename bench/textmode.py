#!/usr/bin/env python3
"""Text-mode faithfulness check: codeq's DEFAULT (text) output must not lose the
facts the --json envelope carries.

differential.py / completion.py only ever exercise --json, so the whole text
renderer — the surface an agent actually reads — went untested, and a string of
bugs slipped through (a dropped -C context flag, map's counts vanishing,
capabilities dumping raw JSON). This runs each command in BOTH modes and asserts
the text output still represents the same facts: every result row's locator
appears, metric counts appear, -C context lines appear, and no command emits raw
JSON in text mode.

It's rendering-layer, not repo-scale, so one repo is enough — defaults to the
codeq repo itself (fast, always present, no corpus needed).

Usage: python3 bench/textmode.py [repo-path]
"""
import json
import os
import subprocess
import sys

import corpus

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CODEQ = corpus.CODEQ
REPO = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else ROOT


def text(args):
    return subprocess.run([CODEQ, *args], cwd=REPO, capture_output=True).stdout.decode("utf-8", "replace")


def data(args):
    out = subprocess.run([CODEQ, "--json", *args], cwd=REPO, capture_output=True).stdout
    try:
        return json.loads(out)["data"]
    except (ValueError, KeyError):
        return None


def rows(d, key=None):
    if isinstance(d, list):
        return d
    if isinstance(d, dict) and key:
        return d.get(key, [])
    return []


def not_json(t):
    """Text mode must not emit the JSON envelope.

    Checked by *parsing*, not by looking for the substring `"data"`. The old
    substring test failed whenever a legitimate text result happened to quote it
    — `search field_names` returns source lines containing `v["data"]`, so the
    harness reported codeq emitting JSON when it had emitted exactly the grep-style
    rows it should. Target selection varies by repo, so it passed for months and
    then didn't."""
    stripped = t.lstrip()
    if not stripped.startswith("{"):
        return True
    try:
        parsed = json.loads(stripped)
    except ValueError:
        return True  # starts with a brace but isn't JSON: not the envelope
    return not (isinstance(parsed, dict) and "schema" in parsed and "data" in parsed)


def missing_locators(t, rs):
    """Rows whose `path:line` locator is absent from the text output."""
    # One key, per ROW_KEY_RULE. This used to try `line` then fall back to
    # `start_line` -- the harness carrying the same papering-over as the
    # renderer, which is why neither of them ever reported the split.
    miss = []
    for r in rs[:80]:
        p = r.get("path")
        line = r.get("line")
        if p and line is not None and f"{p}:{line}" not in t:
            miss.append(f"{p}:{line}")
    return miss


FAILS = []


def check(name, ok, detail=""):
    if not ok:
        FAILS.append(f"{name}  {detail}")


def main():
    if not os.path.exists(CODEQ):
        sys.exit("build release binary first: cargo build --release")
    subprocess.run([CODEQ, "index"], cwd=REPO, capture_output=True)

    # A representative function symbol + its file (chosen via codeq itself).
    syms = data(["symbols", "--fields", "name,kind,path,line", "--limit", "0"]) or []
    funcs = [s for s in syms
             if s.get("kind") in ("function", "method")
             and s.get("name", "").isidentifier() and len(s.get("name", "")) >= 5]
    funcs.sort(key=lambda s: (s["path"], s.get("line", 0)))
    if not funcs:
        sys.exit("no functions found to test against")
    sym = funcs[len(funcs) // 2]["name"]
    file = funcs[len(funcs) // 2]["path"]

    # search: locators present, not raw JSON
    t, d = text(["search", sym, "--limit", "0"]), data(["search", sym, "--limit", "0"])
    check("search/not-json", not_json(t), t[:50])
    check("search/locators", not missing_locators(t, rows(d, "matches")),
          str(missing_locators(t, rows(d, "matches"))[:3]))

    # search -C: context lines must render (the flag was a silent no-op)
    tc = text(["search", sym, "-C", "2", "--limit", "0"])
    dc = data(["search", sym, "-C", "2", "--limit", "0"])
    ctx = [b for m in rows(dc, "matches")[:25] for b in (m.get("before") or [])]
    gone = [b for b in ctx[:12] if b.strip() and b.strip()[:32] not in tc]
    check("search-C/context-lines", not ctx or not gone, f"{len(gone)} context lines dropped")
    check("search-C/adds-lines", not ctx or len(tc.splitlines()) > len(t.splitlines()),
          "-C output no longer than plain")

    # symbols: locators + names
    t, d = text(["symbols", "--limit", "0"]), data(["symbols", "--limit", "0"]) or []
    check("symbols/not-json", not_json(t), t[:50])
    check("symbols/locators", not missing_locators(t, rows(d)), "")
    check("symbols/names", all(s["name"] in t for s in d[:40]), "some names absent from text")

    # map: metric counts must render (they were dropped into empty payloads)
    t, d = text(["map"]), data(["map"])
    check("map/not-json", not_json(t), t[:50])
    dbf = rows(d, "definitions_by_file")
    if dbf:
        check("map/def-counts", f"definitions={dbf[0].get('definitions')}" in t,
              f"definitions={dbf[0].get('definitions')} absent")
    imb = rows(d, "imports_by_module")
    if imb:
        check("map/import-counts", f"importers={imb[0].get('importers')}" in t,
              f"importers={imb[0].get('importers')} absent")

    # capabilities: session-less command must still honor text mode
    t = text(["capabilities"])
    check("capabilities/not-json", not_json(t), t[:50])
    check("capabilities/content", "search" in t and "context" in t, "command names absent")

    # references / definitions / imports / outline / at
    t, d = text(["references", sym, "--limit", "0"]), data(["references", sym, "--limit", "0"])
    check("references/locators", not missing_locators(t, rows(d, "references")), "")
    check("references/not-json", not_json(t), t[:50])

    t, d = text(["definitions", sym]), data(["definitions", sym])
    check("definitions/locators", not missing_locators(t, rows(d)), "")

    t, d = text(["imports", file]), data(["imports", file]) or []
    check("imports/modules", all(r.get("module", "") in t for r in d[:15]), "import modules absent")

    t, d = text(["outline", file]), data(["outline", file]) or []
    check("outline/names", all(s.get("name", "") in t for s in d[:25]), "outline names absent")

    defs = data(["definitions", sym]) or []
    if defs:
        loc = f"{defs[0]['path']}:{defs[0]["line"]}"
        t = text(["at", loc])
        check("at/not-json", not_json(t), t[:50])
        check("at/resolves", sym in t, f"'{sym}' absent from `at {loc}`")

    print(f"text-mode faithfulness on {os.path.basename(REPO)} (target: {sym}): "
          f"{'ALL PASS' if not FAILS else str(len(FAILS)) + ' FAIL'}")
    for f in FAILS:
        print(f"  FAIL  {f}")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
