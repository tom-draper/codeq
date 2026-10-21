#!/usr/bin/env python3
"""Run the same live agent tasks with generic file tools and with Codeq MCP.

The suite uses a pinned, self-contained Python fixture and Claude Code's
stream-json event log. Model calls are opt-in because they incur provider cost.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone

from agent_eval_tasks import TaskSuite, snapshot

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "agent_eval_fixture"


def rpc_schemas(command, cwd, env):
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "codeq-agent-eval", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    ]
    payload = "".join(json.dumps(item) + "\n" for item in requests)
    result = subprocess.run(command, input=payload, text=True, cwd=cwd, env=env,
                            capture_output=True, check=True, timeout=30)
    for line in result.stdout.splitlines():
        try:
            reply = json.loads(line)
        except json.JSONDecodeError:
            continue
        if reply.get("id") == 2:
            return reply["result"]["tools"], result.stderr
    raise RuntimeError(f"MCP server did not answer tools/list: {result.stderr}")


def mcp_config(generic_command, codeq_command):
    return {"mcpServers": {
        "generic": {"command": generic_command[0], "args": generic_command[1:]},
        **({"codeq": {"command": codeq_command[0], "args": codeq_command[1:]}}
           if codeq_command else {}),
    }}


def prepare_workspace(destination):
    shutil.copytree(FIXTURE, destination, ignore=shutil.ignore_patterns("__pycache__"))
    return destination


def parse_events(stdout):
    events = []
    for line in stdout.splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    calls, failures, followups, names = 0, 0, 0, []
    tool_events, tool_results = [], []
    usage_totals = {}
    stream_message = None
    message_usage = {}
    final_text = ""
    for event in events:
        event_type = event.get("type")
        usage = {}
        if event_type == "message_start":
            usage = event.get("message", {}).get("usage", {})
            if stream_message is not None:
                totals = usage_totals.setdefault("stream", [0, 0])
                totals[0] += stream_message[0]
                totals[1] += stream_message[1]
            stream_message = [sum(usage.get(key, 0) for key in (
                "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")),
                usage.get("output_tokens", 0)]
        elif event_type == "message_delta":
            usage = event.get("usage", {})
            if stream_message is None:
                stream_message = [0, 0]
            stream_message[1] = usage.get("output_tokens", stream_message[1])
        elif event_type == "assistant":
            usage = event.get("message", {}).get("usage", {})
            if usage:
                key = event.get("message", {}).get("id", len(message_usage))
                message_usage[key] = [sum(usage.get(field, 0) for field in (
                    "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")),
                    usage.get("output_tokens", 0)]
        if event_type == "assistant":
            message = event.get("message", {})
            for block in message.get("content", []):
                if block.get("type") == "tool_use":
                    calls += 1
                    name = block.get("name", "unknown")
                    names.append(name)
                    tool_events.append({"id": block.get("id"), "name": name,
                                        "input": block.get("input", {})})
                    short_name = name.rsplit("__", 1)[-1].removeprefix("codeq_")
                    if calls > 1 and short_name in {"read_file", "read", "outline", "explore", "context"}:
                        followups += 1
        elif event_type == "user":
            for block in event.get("message", {}).get("content", []):
                if block.get("type") == "tool_result":
                    if block.get("is_error"):
                        failures += 1
                    tool_results.append({"tool_use_id": block.get("tool_use_id"),
                                         "is_error": block.get("is_error", False),
                                         "content": block.get("content")})
        elif event_type == "result":
            final_text = event.get("result", "")
            # Some CLI versions emit only aggregate usage in the result event.
            usage = event.get("usage", {})
            if usage:
                usage_totals["result"] = [
                    sum(usage.get(key, 0) for key in (
                        "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")),
                    usage.get("output_tokens", 0),
                ]
    if stream_message is not None:
        totals = usage_totals.setdefault("stream", [0, 0])
        totals[0] += stream_message[0]
        totals[1] += stream_message[1]
    if message_usage:
        usage_totals["message"] = [sum(v[i] for v in message_usage.values()) for i in (0, 1)]
    input_tokens, output_tokens = next(
        (usage_totals[source] for source in ("result", "message", "stream")
         if source in usage_totals), [0, 0])
    return {"events": events, "tool_calls": calls, "failed_calls": failures,
            "followup_reads": followups, "tool_names": names,
            "tool_events": tool_events, "tool_results": tool_results,
            "input_tokens": input_tokens, "output_tokens": output_tokens,
            "final_text": final_text}


def validate(task, workspace):
    if task["kind"] == "answer":
        try:
            answer = json.loads(task["last_text"])
        except (json.JSONDecodeError, TypeError):
            return {"passed": False, "reason": "final answer was not JSON", "observed": task["last_text"]}
        observed = answer.get("answer") if isinstance(answer, dict) else answer
        expected = task["expected_answer"]
        passed = type(observed) is type(expected) and observed == expected
        return {"passed": passed, "observed": observed}
    tests_before = (FIXTURE / "test_pricing.py").read_bytes()
    tests_path = workspace / "test_pricing.py"
    tests_after = tests_path.read_bytes() if tests_path.is_file() else b""
    try:
        proc = subprocess.run([sys.executable, "-B", "-m", "unittest", "-q"], cwd=workspace,
                              capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return {"passed": False, "reason": "validation timed out"}
    passed = proc.returncode == 0 and tests_after == tests_before
    try:
        final_answer = json.loads(task["last_text"])
    except (json.JSONDecodeError, TypeError):
        final_answer = {}
    originals = snapshot(FIXTURE)
    actual = snapshot(workspace)
    changed_files = sorted(path for path in originals.keys() | actual.keys()
                           if originals.get(path) != actual.get(path))
    passed = passed and final_answer.get("status") == "done" and changed_files == ["pricing.py"]
    return {"passed": passed, "validation_exit_code": proc.returncode,
            "validation_stdout": proc.stdout, "validation_stderr": proc.stderr,
            "tests_unchanged": tests_after == tests_before, "changed_files": changed_files,
            "final_status": final_answer.get("status")}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codeq", type=Path, default=HERE.parent / "target/release/codeq")
    parser.add_argument("--codeq-tools", choices=("all", "core"), default="all",
                        help="Codeq MCP tool profile to evaluate")
    parser.add_argument("--agent", default="claude", help="Claude Code executable")
    parser.add_argument("--model", help="pin a Claude model alias or full model name")
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument("--timeout-seconds", type=int, default=180)
    parser.add_argument("--output", type=Path, help="write the JSON report to this path")
    parser.add_argument("--tasks", type=Path, help="pinned repository task manifest")
    parser.add_argument("--corpus", type=Path, default=HERE / ".corpus")
    parser.add_argument("--profiles", nargs="+", choices=("generic", "core", "all"),
                        help="tool profiles, default generic/core/all for repository tasks")
    parser.add_argument("--self-check", action="store_true",
                        help="verify pinned sources and edit oracles without model calls")
    parser.add_argument("--dry-run", action="store_true", help="print setup and schema sizes without model calls")
    args = parser.parse_args()
    suite = TaskSuite(args.tasks, args.corpus) if args.tasks else None
    if args.self_check:
        if suite is None:
            parser.error("--self-check requires --tasks")
        print(json.dumps({"manifest_sha256": suite.sha256, "checks": suite.self_check()}, indent=2))
        return
    codeq = args.codeq.resolve()
    if not codeq.is_file():
        parser.error(f"Codeq executable not found: {codeq}")
    generic = [sys.executable, str(HERE / "generic_tools_mcp.py")]
    codeq_server = [str(codeq), "mcp", "--tools", args.codeq_tools]
    env = os.environ.copy()
    env["CODEQ_EVAL_WORKSPACE"] = str(FIXTURE)
    generic_schema, generic_stderr = rpc_schemas(generic, FIXTURE, env)
    profiles = args.profiles or (["generic", "core", "all"] if suite else ["generic", "codeq"])
    profiles = list(dict.fromkeys(profiles))
    commands = {profile: [str(codeq), "mcp", "--tools",
                         args.codeq_tools if profile == "codeq" else profile]
                for profile in profiles if profile != "generic"}
    schemas, schema_stderr = {"generic": generic_schema}, {"generic": generic_stderr}
    for profile, command in commands.items():
        schemas[profile], schema_stderr[profile] = rpc_schemas(command, FIXTURE, env)
    try:
        version = subprocess.run([args.agent, "--version"], text=True, capture_output=True,
                                 check=True, timeout=30).stdout.strip()
    except FileNotFoundError:
        if not args.dry_run:
            raise
        version = None
    tasks = [
        {"id": "answer-current-behavior", "kind": "answer",
         "prompt": "Inspect pricing.py. For input [{\"price\": 12, \"cancelled\": true}, {\"price\": 5}], what integer does sum_prices return? Answer only JSON with key answer.",
         "expected_answer": 17},
        {"id": "edit-ignore-cancelled", "kind": "edit",
         "prompt": "Update sum_prices in pricing.py so cancelled items are ignored. Keep the tests unchanged. When finished, answer only JSON with key status and value done.",
         "last_text": ""},
    ]
    if suite:
        tasks = [{"id": task.id, "kind": task.kind, "prompt": task.prompt, "spec": task}
                 for task in suite.tasks]
        # Fail on missing source, stale evidence, or broken edit oracles before
        # starting any provider calls, including later tasks in the matrix.
        suite_checks = suite.self_check()
    else:
        suite_checks = None
    if args.dry_run:
        print(json.dumps({"agent": args.agent, "version": version,
                          "model": args.model, "codeq_command": codeq_server, "tasks": [t["id"] for t in tasks],
                          "profiles": profiles, "planned_model_calls": len(tasks) * len(profiles),
                          "preflight": suite_checks, "tool_schemas": schemas,
                          "schema_bytes": {name: len(json.dumps(value).encode())
                                           for name, value in schemas.items()},
                          "schema_stderr": schema_stderr}, indent=2))
        return

    reports = []
    with tempfile.TemporaryDirectory(prefix="codeq-agent-eval-") as temp:
        for task_index, task in enumerate(tasks):
            order = profiles[task_index % len(profiles):] + profiles[:task_index % len(profiles)]
            for profile in order:
                destination = Path(temp) / f"{task['id']}-{profile}"
                prepared = suite.prepare(task["spec"], destination) if suite else None
                workspace = prepared.workspace if prepared else prepare_workspace(destination)
                config = mcp_config(generic, commands.get(profile))
                config_path = Path(temp) / f"{task['id']}-{profile}.mcp.json"
                config_path.write_text(json.dumps(config))
                prompt = task["prompt"] + "\nWorkspace: " + str(workspace)
                run_env = env.copy()
                run_env["CODEQ_EVAL_WORKSPACE"] = str(workspace)
                command = [args.agent, "--print", "--output-format", "stream-json", "--verbose",
                           "--strict-mcp-config", "--mcp-config", str(config_path),
                           "--tools", "", "--permission-mode", "acceptEdits",
                           "--max-turns", str(args.max_turns)]
                if args.model:
                    command.extend(["--model", args.model])
                command.append(prompt)
                start = time.monotonic()
                try:
                    result = subprocess.run(command, cwd=workspace, stdin=subprocess.DEVNULL,
                                            text=True, capture_output=True, env=run_env,
                                            timeout=args.timeout_seconds)
                    exit_code, stdout, stderr = result.returncode, result.stdout, result.stderr
                except subprocess.TimeoutExpired as error:
                    exit_code = 124
                    stdout = error.stdout.decode(errors="replace") if isinstance(error.stdout, bytes) else error.stdout or ""
                    stderr = error.stderr.decode(errors="replace") if isinstance(error.stderr, bytes) else error.stderr or ""
                elapsed = time.monotonic() - start
                parsed = parse_events(stdout)
                if exit_code:
                    validation = {"status": "error", "passed": False, "reason": "agent failed"}
                elif prepared:
                    validation = prepared.validate(parsed["final_text"])
                else:
                    validation = validate({**task, "last_text": parsed["final_text"]}, workspace)
                reports.append({
                    "task": task["id"], "profile": profile, "command": command,
                    "exit_code": exit_code, "elapsed_seconds": elapsed,
                    "input_tokens": parsed["input_tokens"],
                    "output_tokens": parsed["output_tokens"],
                    "total_model_tokens": parsed["input_tokens"] + parsed["output_tokens"],
                    "tool_calls": parsed["tool_calls"], "failed_calls": parsed["failed_calls"],
                    "followup_reads": parsed["followup_reads"], "tool_names": parsed["tool_names"],
                    "tool_events": parsed["tool_events"], "tool_results": parsed["tool_results"],
                    "model_answer": parsed["final_text"], "stderr": stderr,
                    "validation": validation,
                    **({"repo": task["spec"].repo, "commit": task["spec"].commit,
                        "source_sha256": prepared.source_sha256} if prepared else {}),
                })
    fixture_hash = hashlib.sha256()
    for path in sorted(p for p in FIXTURE.rglob("*") if p.is_file()):
        fixture_hash.update(path.relative_to(FIXTURE).as_posix().encode())
        fixture_hash.update(b"\0")
        fixture_hash.update(path.read_bytes())
    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "agent": args.agent, "agent_version": version, "model": args.model,
        "fixture_sha256": fixture_hash.hexdigest() if not suite else None,
        "codeq_command": codeq_server,
        "profiles": profiles, "tool_schemas": schemas,
        "tool_schema_bytes": {name: len(json.dumps(value).encode()) for name, value in schemas.items()},
        "schema_stderr": schema_stderr,
        "manifest_sha256": suite.sha256 if suite else None,
        "preflight": suite_checks,
        "results": reports,
    }
    rendered = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(rendered + "\n")
    print(rendered)
    return 0 if all(run["validation"]["passed"] for run in reports) else 1


if __name__ == "__main__":
    raise SystemExit(main())
