#!/usr/bin/env python3
"""Check answer completeness and output cost for codeq vs rg/sed workflows.

Uses a deterministic Python fixture, no downloads. Counts actual subprocess calls
and UTF-8 stdout bytes (~4 bytes/token), not model reasoning or tool-call syntax.
The baseline batches and merges overlapping sed windows. These are mechanism
checks, not repository-wide speed or agent-success claims.

Usage: python3 bench/agent_workflows.py --codeq target/debug/codeq
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


class Workflow:
    def __init__(self, root):
        self.root = root
        self.calls = 0
        self.output = b""

    def run(self, *args):
        result = subprocess.run(args, cwd=self.root, capture_output=True, check=True)
        self.calls += 1
        self.output += result.stdout
        return result.stdout

    def cost(self):
        return {"calls": self.calls, "bytes": len(self.output),
                "approx_tokens": (len(self.output) + 3) // 4}


def windows(lines, total):
    merged = []
    for line in sorted(lines):
        start, end = max(1, line - 20), min(total, line + 39)
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
        else:
            merged.append((start, end))
    return merged


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codeq", type=Path, default=ROOT / "target/release/codeq")
    args = parser.parse_args()
    binary = str(args.codeq.resolve())
    results = []
    with tempfile.TemporaryDirectory(prefix="codeq-agent-workflows-") as tmp:
        root = Path(tmp)
        blocks, positions, definitions = [], [], []
        for i in range(3):
            blocks.extend(f"# surrounding context {i}:{j}, unrelated to the requested implementation\n"
                          for j in range(80))
            positions.append(len(blocks) + 1)
            body = (f"def target_{i}(value):\n"
                    f"    result = needle(value)\n"
                    f"    return result + {i}\n")
            definitions.append(body.encode())
            blocks.extend(body.splitlines(keepends=True))
        blocks.extend(f"# trailing context {j}, unrelated to the requested implementation\n"
                      for j in range(80))
        (root / "sample.py").write_text("".join(blocks))
        # Exclude cold-index diagnostics from output measurements, consistently
        # with the existing completion harness. No runtime timings are claimed.
        subprocess.run([binary, "index"], cwd=root, capture_output=True, check=True)

        def record(task, cq, baseline):
            results.append({"task": task, "codeq": cq.cost(), "rg_sed": baseline.cost()})

        # Unknown-name tasks use one ranked query, with no spelling retries.
        (root / "auth.py").write_text(
            'def refreshAuthToken(value):\n    """Renew a session credential."""\n    return value\n')
        discovery = []
        for query in ("refresh auth token", "renew session credential"):
            cq = Workflow(root)
            answer = json.loads(cq.run(binary, "--json", "discover", query, "--limit", "1"))
            assert answer["data"]["candidates"][0]["name"] == "refreshAuthToken", query
            discovery.append({"query": query, "top1": True, **cq.cost()})
        results.append({"task": "unknown-name-discovery", "queries": discovery,
                        "embeddings": "not evaluated; deterministic fixture is insufficient evidence"})

        # Identical requested bytes: neither tool gets a contrived token win.
        cq, baseline = Workflow(root), Workflow(root)
        start = positions[0]
        assert cq.run(binary, "read", f"sample.py:{start}-{start + 2}", "--raw") == baseline.run(
            "sed", "-n", f"{start},{start + 2}p", "sample.py") == definitions[0]
        record("known-range-exact-bytes", cq, baseline)

        # Start from an existing search hit, with no symbol name or end line.
        cq, baseline = Workflow(root), Workflow(root)
        hit = positions[1] + 1
        assert cq.run(binary, "read", "--at", f"sample.py:{hit}", "--raw") == definitions[1]
        start, end = windows([hit], len(blocks))[0]
        assert definitions[1] in baseline.run("sed", "-n", f"{start},{end}p", "sample.py")
        record("body-from-search-hit", cq, baseline)

        # Initial search: read every matching implementation, not just the lines.
        cq, baseline = Workflow(root), Workflow(root)
        source = cq.run(binary, "search", "needle", "--source", "--max-tokens", "1000")
        hits = baseline.run("rg", "-n", "needle", "sample.py")
        lines = [int(line.split(b":", 1)[0]) for line in hits.splitlines()]
        sed_args = ["sed", "-n"]
        for start, end in windows(lines, len(blocks)):
            sed_args.extend(["-e", f"{start},{end}p"])
        read = baseline.run(*sed_args, "sample.py")
        for body in definitions:
            assert body in source and body in read, "both answers must contain every complete body"
            assert source.count(body) == 1, "codeq must emit each body once"
        record("search-and-read-implementations", cq, baseline)

        # Code shape and complete implementations in one call, with the same
        # complete-body assertions as the textual search/read workflow.
        cq, baseline = Workflow(root), Workflow(root)
        source = cq.run(binary, "astgrep", "needle($VALUE)", "--language", "python",
                        "--source", "--max-tokens", "1000")
        hits = baseline.run("rg", "-n", r"needle\(", "sample.py")
        lines = [int(line.split(b":", 1)[0]) for line in hits.splitlines()]
        sed_args = ["sed", "-n"]
        for start, end in windows(lines, len(blocks)):
            sed_args.extend(["-e", f"{start},{end}p"])
        read = baseline.run(*sed_args, "sample.py")
        for body in definitions:
            assert body in source and body in read
            assert source.count(body) == 1
        record("structural-search-and-read-implementations", cq, baseline)

        # Several hits within each body: one call, no duplicate source.
        cq, baseline = Workflow(root), Workflow(root)
        at_args = []
        lines = []
        for position in positions:
            for line in [position + 1, position + 2]:
                at_args.extend(["--at", f"sample.py:{line}"])
                lines.append(line)
        source = cq.run(binary, "read", *at_args)
        sed_args = ["sed", "-n"]
        for start, end in windows(lines, len(blocks)):
            sed_args.extend(["-e", f"{start},{end}p"])
        read = baseline.run(*sed_args, "sample.py")
        for i in range(3):
            assert source.count(f"def target_{i}".encode()) == read.count(f"def target_{i}".encode()) == 1
        record("batches-of-search-hits", cq, baseline)

    print(json.dumps({"fixture": "synthetic Python, short definitions amid unrelated source",
                      "metric": "stdout bytes; approximate tokens = ceil(bytes/4)",
                      "baseline": "rg then merged, batched sed windows of up to 60 lines",
                      "results": results}, indent=2))


if __name__ == "__main__":
    main()
