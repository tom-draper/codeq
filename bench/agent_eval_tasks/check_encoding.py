"""Exercise the edited Requests helpers without importing third-party packages.

Only the two real function definitions are loaded, using Python's AST rather
than Codeq extraction. This checks helper behavior, not package integration.
"""
import ast
from pathlib import Path
import sys


def helpers(workspace, parser_name="_parse_content_type_header"):
    source = Path(workspace) / "src/requests/utils.py"
    tree = ast.parse(source.read_text())
    wanted = {parser_name, "get_encoding_from_headers"}
    nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in wanted]
    assert {n.name for n in nodes} == wanted, "missing encoding helper"
    namespace = {}
    exec(compile(ast.Module(body=nodes, type_ignores=[]), str(source), "exec"), namespace)
    return namespace


def check(workspace):
    functions = helpers(workspace)
    encoding = functions["get_encoding_from_headers"]
    for headers, expected in [
        ({}, None),
        ({"content-type": "application/problem+json"}, "utf-8"),
        ({"content-type": "application/vnd.api+json; version=1"}, "utf-8"),
        ({"content-type": "application/problem+json; charset=iso-8859-2"}, "iso-8859-2"),
        ({"content-type": "application/json"}, "utf-8"),
        ({"content-type": "text/plain"}, "ISO-8859-1"),
        ({"content-type": "application/octet-stream"}, None),
        ({"content-type": "application/not+json-data"}, None),
    ]:
        assert encoding(headers) == expected, (headers, expected, encoding(headers))


if __name__ == "__main__":
    check(sys.argv[1])
