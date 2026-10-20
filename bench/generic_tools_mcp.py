#!/usr/bin/env python3
"""Small generic file tools used as the control arm of agent_eval.py."""
import json
import os
import sys
from pathlib import Path


TOOLS = [
    {"name": "list_files", "description": "List tracked-looking source files below a directory.",
     "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}},
                     "additionalProperties": False}},
    {"name": "search_text", "description": "Find literal text in files below a directory.",
     "inputSchema": {"type": "object", "properties": {
         "query": {"type": "string"}, "path": {"type": "string"}},
         "required": ["query"], "additionalProperties": False}},
    {"name": "read_file", "description": "Read a file, optionally limited to a line range.",
     "inputSchema": {"type": "object", "properties": {
         "path": {"type": "string"}, "start_line": {"type": "integer"},
         "end_line": {"type": "integer"}}, "required": ["path"],
         "additionalProperties": False}},
    {"name": "write_file", "description": "Replace a file with the supplied UTF-8 text.",
     "inputSchema": {"type": "object", "properties": {
         "path": {"type": "string"}, "content": {"type": "string"}},
         "required": ["path", "content"], "additionalProperties": False}},
]


def within(root, relative):
    path = (root / relative).resolve()
    if path != root and root not in path.parents:
        raise ValueError("path escapes workspace")
    return path


def call(root, name, args):
    rel = args.get("path", ".")
    target = within(root, rel)
    if name == "list_files":
        paths = sorted(p.relative_to(root).as_posix() for p in target.rglob("*")
                       if p.is_file() and ".git" not in p.parts)
        return "\n".join(paths[:500])
    if name == "search_text":
        query = args["query"]
        rows = []
        files = [target] if target.is_file() else sorted(target.rglob("*"))
        for path in files:
            if not path.is_file() or ".git" in path.parts:
                continue
            try:
                lines = path.read_text().splitlines()
            except (UnicodeError, OSError):
                continue
            for number, line in enumerate(lines, 1):
                if query in line:
                    rows.append(f"{path.relative_to(root).as_posix()}:{number}:{line}")
                    if len(rows) == 500:
                        return "\n".join(rows)
        return "\n".join(rows)
    if name == "read_file":
        lines = target.read_text().splitlines()
        start = max(1, int(args.get("start_line", 1)))
        end = min(len(lines), int(args.get("end_line", len(lines))))
        return "\n".join(f"{n}\t{lines[n - 1]}" for n in range(start, end + 1))
    if name == "write_file":
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(args["content"])
        return f"wrote {target.relative_to(root).as_posix()}"
    raise ValueError(f"unknown tool: {name}")


def main():
    root = Path(os.environ["CODEQ_EVAL_WORKSPACE"]).resolve()
    for line in sys.stdin:
        request = None
        try:
            request = json.loads(line)
            method = request.get("method")
            ident = request.get("id")
            if method == "initialize":
                result = {"protocolVersion": request.get("params", {}).get("protocolVersion", "2024-11-05"),
                          "capabilities": {"tools": {}},
                          "serverInfo": {"name": "codeq-eval-generic-tools", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": TOOLS}
            elif method == "tools/call":
                params = request.get("params", {})
                try:
                    content = call(root, params["name"], params.get("arguments", {}))
                    result = {"content": [{"type": "text", "text": content}], "isError": False}
                except Exception as error:
                    result = {"content": [{"type": "text", "text": str(error)}], "isError": True}
            elif method and method.startswith("notifications/"):
                continue
            else:
                result = {}
            if ident is not None:
                print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
        except Exception as error:
            ident = request.get("id") if isinstance(request, dict) else None
            if ident is not None:
                print(json.dumps({"jsonrpc": "2.0", "id": ident,
                                  "error": {"code": -32603, "message": str(error)}}), flush=True)


if __name__ == "__main__":
    main()
