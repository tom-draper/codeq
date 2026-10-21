#!/usr/bin/env python3
"""Compare verified MCP sequences, queued bursts and resident memory at increasing scale."""
import argparse
import hashlib
import json
import platform
import queue
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from query_latency import invoke


class Server:
    def __init__(self, binary, root):
        self.process = subprocess.Popen(
            [str(binary), "--root", str(root), "mcp", "--workers", "2"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1)
        self.replies = queue.Queue()
        self.next_id = 0
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        try:
            for line in self.process.stdout:
                self.replies.put((time.perf_counter_ns(), json.loads(line)))
        except Exception as error:
            self.replies.put(error)
        finally:
            self.replies.put(EOFError("MCP stdout closed"))

    def batch(self, calls, refreshed=False):
        pending = {}
        for name, arguments, expected in calls:
            self.next_id += 1
            pending[self.next_id] = (time.perf_counter_ns(), expected)
            request = {"jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                       "params": {"name": name, "arguments": arguments}}
            self.process.stdin.write(json.dumps(request) + "\n")
            self.process.stdin.flush()
        latencies = []
        refresh_notes = 0
        while pending:
            received = self.replies.get(timeout=30)
            if isinstance(received, Exception):
                raise received
            ended, reply = received
            started, expected = pending.pop(reply["id"])
            assert "error" not in reply, reply
            result = reply["result"]
            assert not result.get("isError", False), reply
            content = result["content"]
            suffix = "# codeq: reindexed 1 file(s)"
            if refreshed and len(content) == 1 and content[0].get("text", "").endswith(suffix):
                content = [{"type": "text", "text": content[0]["text"][:-len(suffix)]}]
                refresh_notes += 1
            assert content == [{"type": "text", "text": expected}], reply
            latencies.append((ended - started) / 1e6)
        assert refresh_notes == int(refreshed), (refresh_notes, refreshed)
        return latencies

    def memory(self):
        path = Path(f"/proc/{self.process.pid}/status")
        if not path.exists():
            return None
        fields = dict(line.split(":", 1) for line in path.read_text().splitlines() if ":" in line)
        return {key: int(fields[key].split()[0]) for key in ["VmRSS", "VmHWM"]}

    def close(self):
        self.process.stdin.close()
        try:
            code = self.process.wait(timeout=10)
            assert code == 0, (code, self.process.stderr.read())
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            self.reader.join(timeout=1)
            self.process.stdout.close()
            self.process.stderr.close()


def fixture(root, files):
    root.mkdir()
    for i in range(files):
        (root / f"item_{i}.py").write_text(f"def item_{i}(): return target()\n")
    (root / "target.py").write_text("def target(): return 0\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--files", type=int, nargs="+", default=[100, 1000])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 5 or min(args.files) < 1:
        parser.error("use at least five runs and positive file counts")
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    report = {"platform": platform.platform(), "runs": args.runs,
              "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest() for key, path in binaries.items()},
              "source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "limits": "Two-worker persistent MCP servers on warm indexes. Latency includes transport and queue wait. "
                        "Bursts contain 12 requests, below the 32-slot admission limit. Responses checked against CLI text. "
                        "RSS and high-water marks are Linux /proc samples in KiB; null on other platforms. "
                        "Finite sequences expose memory trends, not proof of absence of leaks.",
              "workloads": {}}
    specifications = [
        ("codeq_definitions", {"symbol": "target"}, ["definitions", "target"], "target"),
        ("codeq_callers", {"name": "target", "limit": 0}, ["callers", "target", "--limit", "0"], "item_0"),
        ("codeq_read", {"file": "target.py"}, ["read", "target.py"], "def target()"),
    ]
    with tempfile.TemporaryDirectory(prefix="codeq-mcp-perf-") as directory:
        for files in args.files:
            roots = {key: Path(directory) / f"{files}-{key}" for key in binaries}
            servers = {}
            expected = {}
            try:
                for key, binary in binaries.items():
                    fixture(roots[key], files)
                    invoke(binary, roots[key], ["index"])
                    expected[key] = []
                    for name, arguments, command, marker in specifications:
                        _, output = invoke(binary, roots[key], command)
                        text = output[0].decode()
                        if output[1].strip():
                            if text and not text.endswith("\n"):
                                text += "\n"
                            text += output[1].decode().strip()
                        assert marker in text, (name, text)
                        expected[key].append((name, arguments, text))
                    servers[key] = Server(binary, roots[key])
                    for call in expected[key]:
                        servers[key].batch([call])
                assert expected["before"] == expected["after"]
                for mode in ["sequential", "queued", "edited", "queued_edited"]:
                    samples = {key: [] for key in binaries}
                    memory = {key: [server.memory()] for key, server in servers.items()}
                    for iteration in range(args.runs):
                        for key in (list(binaries) if iteration % 2 == 0 else list(reversed(binaries))):
                            version = iteration + (1001 if mode == "queued_edited" else 1)
                            if mode in ["edited", "queued_edited"]:
                                (roots[key] / "target.py").write_text(f"def target(): return {version}\n")
                            calls = list(expected[key])
                            if mode in ["edited", "queued_edited"]:
                                name, arguments, text = calls[0]
                                if mode == "edited":
                                    calls[0] = (name, arguments, text + "# codeq: reindexed 1 file(s)")
                                name, arguments, text = calls[2]
                                calls[2] = (name, arguments, text.replace("return 0", f"return {version}"))
                            if mode in ["queued", "queued_edited"]:
                                samples[key].extend(servers[key].batch(calls * 4, refreshed=mode == "queued_edited"))
                            else:
                                for call in calls:
                                    samples[key].extend(servers[key].batch([call]))
                            memory[key].append(servers[key].memory())
                    row = {"files": files + 1, "samples_ms": samples, "memory_kib": memory,
                           "median_ms": {key: statistics.median(values) for key, values in samples.items()},
                           "max_ms": {key: max(values) for key, values in samples.items()}}
                    report["workloads"][f"{files}_{mode}"] = row
                    print(files, mode, row["median_ms"], flush=True)
            finally:
                for server in servers.values():
                    server.close()
    report.update(errors=0, answer_mismatches=0)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
