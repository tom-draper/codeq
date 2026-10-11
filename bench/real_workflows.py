#!/usr/bin/env python3
"""Check complete agent answers and measure calls/bytes on pinned real code.

No latency claims or model tokenizer dependency. The manifest is a reviewed
oracle, never supplied to a tool as an answer or as unknown source boundaries.
"""

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import shlex
import subprocess

HERE = Path(__file__).resolve().parent


class Workflow:
    def __init__(self, root):
        self.root = root
        self.transcript = []

    def run(self, executable, *args, label=None):
        result = subprocess.run([str(executable), *args], cwd=self.root,
                                capture_output=True, check=True)
        self.transcript.append({
            "command": shlex.join([label or str(executable), *args]),
            "stdout": result.stdout.decode(), "stderr": result.stderr.decode(),
        })
        return result.stdout.decode()

    def cost(self):
        inputs = sum(len(c["command"].encode()) for c in self.transcript)
        stdout = sum(len(c["stdout"].encode()) for c in self.transcript)
        stderr = sum(len(c["stderr"].encode()) for c in self.transcript)
        return {"calls": len(self.transcript), "input_bytes": inputs,
                "stdout_bytes": stdout, "stderr_bytes": stderr,
                "approx_tokens": math.ceil((inputs + stdout + stderr) / 4),
                "commands": [c["command"] for c in self.transcript]}


def normalized(text):
    # Numbered codeq reads use a tab; source bundles emit verbatim bodies.
    return [re.sub(r"^\d+\t", "", line).strip() for line in text.splitlines()]


def complete(output, body):
    actual = "\n".join(normalized(output))
    expected = "\n".join(normalized(body))
    # Syntax spans can end before a statement's trailing semicolon, while sed
    # returns the whole line. Require every source character, permit that suffix.
    return bool(expected) and expected in actual


def oracle(root, definition):
    raw = (root / definition["path"]).read_bytes()
    body = raw[definition["byte"]:definition["end_byte"]]
    if hashlib.sha256(body).hexdigest() != definition["sha256"]:
        raise ValueError(f"oracle source changed: {definition['path']}:{definition['line']}")
    return body.decode()


def merged_windows(hits, radius, total):
    windows = []
    for line in sorted(set(hits)):
        start, end = max(1, line - radius), min(total, line + radius)
        if windows and start <= windows[-1][1] + 1:
            windows[-1][1] = max(end, windows[-1][1])
        else:
            windows.append([start, end])
    return windows


def read_windows(workflow, path, hits, bodies):
    """Favorable baseline: oracle stops expansion as soon as bodies are complete.

    The tool only sees hit positions and a doubling window, never golden ends.
    All failed/truncated reads count toward cost, as they do for an agent.
    """
    total = len((workflow.root / path).read_text().splitlines())
    radius = 20
    while True:
        args = ["-n"]
        for start, end in merged_windows(hits, radius, total):
            args.extend(["-e", f"{start},{end}p"])
        output = workflow.run("sed", *args, path)
        if all(complete(output, body) for body in bodies):
            return output
        if radius >= total:
            raise AssertionError(f"baseline cannot return required bodies in {path}")
        radius *= 2


def search_hits(workflow, path, pattern, fixed=True):
    output = workflow.run("rg", "-n", "--no-heading", "-H",
                          *(["-F"] if fixed else []), "--", pattern, path)
    hits = [int(line.split(":", 2)[1]) for line in output.splitlines()]
    if not hits:
        raise AssertionError(f"no baseline hits for {pattern!r}")
    return hits


def run_task(root, spec, kind, binary, explore_tokens=3000):
    cq, baseline = Workflow(root), Workflow(root)
    focus = spec.get("explore_focus", spec["focus"]) if kind == "explore" else spec["focus"]
    path = focus["path"]
    body = oracle(root, focus)
    required = [body]
    if kind == "known-range":
        output = cq.run(binary, "read", f"{path}:{focus['line']}-{focus['end_line']}",
                        "--raw", label="codeq")
        other = baseline.run("sed", "-n", f"{focus['line']},{focus['end_line']}p", path)
        if output != other:
            raise AssertionError("known-range control must return identical bytes")
    elif kind == "position":
        hit = focus["line"] + 1
        output = cq.run(binary, "read", "--at", f"{path}:{hit}", "--raw", label="codeq")
        other = read_windows(baseline, path, [hit], required)
    elif kind == "search-source":
        output = cq.run(binary, "search", spec["pattern"], "--fixed", "--path", path,
                        "--source", "--max-tokens", "3000", label="codeq")
        hits = search_hits(baseline, path, spec["pattern"])
        other = read_windows(baseline, path, hits, required)
    else:
        helper = spec["helper"]
        required.append(oracle(root, helper))
        output = cq.run(binary, "explore", focus["name"], "--path", path,
                        "--max-tokens", str(explore_tokens), label="codeq")
        # A budget omission or deferred candidate is not a complete answer.
        # Follow up only with locations advertised in the initial result.
        positions = []
        for definition, expected in zip([focus, helper], required):
            if complete(output, expected):
                continue
            location = re.search(
                rf"^{re.escape(definition['path'])}:(\d+):[^\n]*\b{re.escape(definition['name'])}\b",
                output, re.MULTILINE)
            if not location:
                raise AssertionError(f"missing advertised candidate {definition['name']}")
            positions.extend(["--at", f"{definition['path']}:{location[1]}"])
        if positions:
            output += cq.run(binary, "read", *positions, label="codeq")
        # Declaration-shaped lookup, rather than every mention of the name.
        names = "|".join(re.escape(d["name"]) for d in [focus, helper])
        pattern = rf"\b(?:def|fn|func|function)\s+(?:\([^)]*\)\s*)?(?:{names})\b"
        hits = search_hits(baseline, path, pattern, fixed=False)
        other = read_windows(baseline, path, hits, required)
    for expected in required:
        if not complete(output, expected) or not complete(other, expected):
            raise AssertionError(f"incomplete {spec['repo']} {kind} answer")
    return {"repo": spec["repo"], "task": kind, "focus": focus["name"],
            "required_bodies": len(required), "complete": True,
            "codeq": cq.cost(), "rg_sed": baseline.cost()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codeq", type=Path, default=HERE.parent / "target/release/codeq")
    parser.add_argument("--compare-codeq", type=Path,
                        help="optional earlier binary, measured on the same complete tasks")
    parser.add_argument("--corpus", type=Path, default=HERE / ".corpus")
    parser.add_argument("--explore-tokens", type=int, default=3000,
                        help="source budget, incomplete answers trigger counted position reads")
    args = parser.parse_args()
    specs = json.loads((HERE / "real_workflows.json").read_text())
    results = []
    for spec in specs:
        root = args.corpus.resolve() / spec["repo"]
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip()
        if commit != spec["commit"]:
            raise ValueError(f"wrong revision for {spec['repo']}: {commit}")
        dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root)
        if dirty:
            raise ValueError(f"tracked source changed in {spec['repo']}")
        kinds = ["known-range", "position", "search-source"]
        if "helper" in spec:
            kinds.append("explore")
        for label, binary in [("current", args.codeq), ("comparison", args.compare_codeq)]:
            if binary is None:
                continue
            binary = binary.resolve()
            subprocess.run([str(binary), "index"], cwd=root, capture_output=True, check=True)
            for kind in kinds:
                result = run_task(root, spec, kind, binary, args.explore_tokens)
                result["variant"] = label
                results.append(result)
    print(json.dumps({
        "metric": "canonical command input + stdout + stderr bytes; approximate tokens = ceil(bytes/4)",
        "calls": "external command invocations, not measured model tool rounds",
        "explore_max_tokens": args.explore_tokens,
        "baseline": "rg declaration/content hits then merged sed windows, radius 20 doubling until complete",
        "limits": "reviewed fixed tasks, oracle-assisted completion, warmed indexes, no latency or model-agent claims",
        "revisions": {s["repo"]: s["commit"] for s in specs},
        "binaries": {label: hashlib.sha256(binary.read_bytes()).hexdigest()
                     for label, binary in [("current", args.codeq), ("comparison", args.compare_codeq)]
                     if binary is not None},
        "results": results,
    }, indent=2))


if __name__ == "__main__":
    main()
