#!/usr/bin/env python3
"""Clone and cache a corpus of open-source repos for differential benchmarking.

The corpus is a fixed set of real projects spanning codeq's languages. Each is
shallow-cloned at a *pinned tag* into bench/.corpus/ (gitignored) so results are
reproducible and any change in the numbers reflects codeq, not repo drift. The
resolved commit of each repo is recorded in bench/.corpus/LOCK.json.

Usage:
    python3 bench/corpus.py sync         # clone/refresh every repo
    python3 bench/corpus.py list         # print name, lang, path, commit

Other bench scripts import `repo_paths()` to iterate (name, lang, path).
"""
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CACHE = os.path.join(HERE, ".corpus")
LOCK = os.path.join(CACHE, "LOCK.json")
# The binary every harness measures. Release, never debug: a debug build is
# several times slower and would make codeq look bad against system tools.
CODEQ = os.path.join(ROOT, "target", "release", "codeq")

# name, git url, pinned ref (tag), primary language. Kept small and varied so a
# full run is minutes, not hours; add rows to widen coverage. Pinned tags are
# immutable references, so the corpus stays fixed across syncs.
REPOS = [
    # python
    ("flask",    "https://github.com/pallets/flask",           "3.0.3",               "python"),
    ("requests", "https://github.com/psf/requests",            "v2.32.3",             "python"),
    ("rich",     "https://github.com/Textualize/rich",         "v13.7.1",             "python"),
    ("httpie",   "https://github.com/httpie/httpie",           "3.2.2",               "python"),
    # go
    ("gin",      "https://github.com/gin-gonic/gin",            "v1.9.1",              "go"),
    ("cobra",    "https://github.com/spf13/cobra",              "v1.8.0",              "go"),
    ("mux",      "https://github.com/gorilla/mux",              "v1.8.1",              "go"),
    # javascript
    ("express",  "https://github.com/expressjs/express",        "4.19.2",              "javascript"),
    ("lodash",   "https://github.com/lodash/lodash",            "4.17.21",             "javascript"),
    # typescript
    ("zod",      "https://github.com/colinhacks/zod",           "v3.23.8",             "typescript"),
    ("dayjs",    "https://github.com/iamkun/dayjs",             "v1.11.11",            "typescript"),
    # rust
    ("ripgrep",  "https://github.com/BurntSushi/ripgrep",       "14.1.0",              "rust"),
    ("serde",    "https://github.com/serde-rs/serde",           "v1.0.203",            "rust"),
    ("clap",     "https://github.com/clap-rs/clap",             "v4.5.4",              "rust"),
    # ruby
    ("sinatra",  "https://github.com/sinatra/sinatra",          "v4.0.0",              "ruby"),
    ("rack",     "https://github.com/rack/rack",                "v3.1.3",              "ruby"),
    # java
    ("gson",     "https://github.com/google/gson",              "gson-parent-2.11.0",  "java"),
    # c
    ("jq",       "https://github.com/jqlang/jq",                "jq-1.7.1",            "c"),
    ("redis",    "https://github.com/redis/redis",              "7.2.5",               "c"),
    # cpp
    ("json",     "https://github.com/nlohmann/json",            "v3.11.3",             "cpp"),
    ("fmt",      "https://github.com/fmtlib/fmt",               "10.2.1",              "cpp"),
    ("spdlog",   "https://github.com/gabime/spdlog",            "v1.14.1",             "cpp"),
    # csharp
    ("newtonsoft", "https://github.com/JamesNK/Newtonsoft.Json", "13.0.3",             "csharp"),
    # php
    ("guzzle",   "https://github.com/guzzle/guzzle",            "7.8.1",               "php"),
    ("monolog",  "https://github.com/Seldaek/monolog",          "3.6.0",               "php"),
    # kotlin
    ("okio",     "https://github.com/square/okio",              "3.9.1",               "kotlin"),
    # swift
    ("alamofire", "https://github.com/Alamofire/Alamofire",     "5.9.1",               "swift"),
    # scala
    ("os-lib",   "https://github.com/com-lihaoyi/os-lib",       "0.10.7",              "scala"),
    # tsx
    ("cmdk",     "https://github.com/pacocoursey/cmdk",         "v1.1.1",              "tsx"),
    # markdown
    ("rust-book", "https://github.com/rust-lang/book",          "trpl-v0.3.0",         "markdown"),
    # sql
    ("pgvector", "https://github.com/pgvector/pgvector",        "v0.7.4",              "sql"),
]


def _resolved_commit(path):
    p = subprocess.run(["git", "-C", path, "rev-parse", "HEAD"],
                       capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else None


def sync():
    os.makedirs(CACHE, exist_ok=True)
    lock = {}
    for name, url, ref, lang in REPOS:
        dest = os.path.join(CACHE, name)
        if os.path.isdir(os.path.join(dest, ".git")):
            print(f"  [cached] {name} ({lang})")
        else:
            print(f"  [clone ] {name} ({lang}) @ {ref} ...", flush=True)
            r = subprocess.run(
                ["git", "clone", "--depth", "1", "--branch", ref, url, dest],
                capture_output=True, text=True,
            )
            if r.returncode != 0:
                print(f"  [FAIL  ] {name}: {r.stderr.strip().splitlines()[-1:]}",
                      file=sys.stderr)
                continue
        commit = _resolved_commit(dest)
        lock[name] = {"url": url, "ref": ref, "lang": lang, "commit": commit}
    with open(LOCK, "w") as f:
        json.dump(lock, f, indent=2)
    print(f"synced {len(lock)}/{len(REPOS)} repos -> {CACHE}")


def codeq_commit():
    """The codeq revision a benchmark run was produced by, so a results file can
    be tied to the code that produced it. Shared because every harness needs it
    and each had its own copy."""
    p = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"],
                       capture_output=True, text=True)
    return p.stdout.strip()[:12]


def timed_run(cmd, cwd):
    """Run `cmd` in `cwd`, returning (elapsed_seconds, stdout). Every harness
    times subprocesses the same way, so they share one clock — measuring with
    per-script copies invites them to drift apart and stop being comparable."""
    t0 = time.perf_counter()
    # stdin is closed: rg with no path searches stdin when it is a pipe, so an
    # inherited pipe would hang the run instead of searching the repo.
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL)
    return time.perf_counter() - t0, p.stdout


def repo_paths():
    """Yield (name, lang, abspath) for every synced repo. Runs sync if empty."""
    if not os.path.isdir(CACHE):
        sync()
    for name, _url, _ref, lang in REPOS:
        dest = os.path.join(CACHE, name)
        if os.path.isdir(os.path.join(dest, ".git")):
            yield name, lang, dest


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "sync":
        sync()
    elif cmd == "list":
        for name, lang, path in repo_paths():
            print(f"  {name:10} {lang:11} {path}  {(_resolved_commit(path) or '')[:12]}")
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
