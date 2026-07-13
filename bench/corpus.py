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

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, ".corpus")
LOCK = os.path.join(CACHE, "LOCK.json")

# name, git url, pinned ref (tag), primary language. Kept small and varied so a
# full run is minutes, not hours; add rows to widen coverage. Pinned tags are
# immutable references, so the corpus stays fixed across syncs.
REPOS = [
    ("flask",   "https://github.com/pallets/flask",      "3.0.3",   "python"),
    ("gin",     "https://github.com/gin-gonic/gin",       "v1.9.1",  "go"),
    ("express", "https://github.com/expressjs/express",   "4.19.2",  "javascript"),
    ("ripgrep", "https://github.com/BurntSushi/ripgrep",  "14.1.0",  "rust"),
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
