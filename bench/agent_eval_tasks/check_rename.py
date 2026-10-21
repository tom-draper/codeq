"""Check a rename across Requests source and its existing test references."""
import ast
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_encoding import helpers


def check(workspace, baseline=None):
    root = Path(workspace)
    old, new = "_parse_content_type_header", "_split_content_type_header"
    if baseline:
        original = json.loads(Path(baseline).read_text())["tests/test_utils.py"]
        expected = ast.dump(ast.parse(original.replace(old, new)), include_attributes=False)
        actual = ast.dump(ast.parse((root / "tests/test_utils.py").read_text()), include_attributes=False)
        assert actual == expected, "existing tests changed beyond the intended rename"
    for name in ("src/requests/utils.py", "tests/test_utils.py"):
        tree = ast.parse((root / name).read_text())
        identifiers = {node.id for node in ast.walk(tree) if isinstance(node, ast.Name)}
        identifiers.update(node.name for node in ast.walk(tree)
                           if isinstance(node, (ast.FunctionDef, ast.alias)))
        assert old not in identifiers, f"old reference in {name}"
        assert new in identifiers, f"missing renamed reference in {name}"
    functions = helpers(workspace, new)
    parse = functions[new]
    assert parse('multipart/form-data; boundary="abc"; flag') == (
        "multipart/form-data", {"boundary": "abc", "flag": True})
    assert parse("application/json ; ; ") == ("application/json", {})
    encoding = functions["get_encoding_from_headers"]
    assert encoding({"content-type": "application/json"}) == "utf-8"
    assert encoding({"content-type": "text/plain; charset=utf-16"}) == "utf-16"
    assert encoding({}) is None


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
