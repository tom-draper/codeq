#!/usr/bin/env python3
"""Check a release executable in an empty agent home and fixture repository."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--version")
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    with tempfile.TemporaryDirectory(prefix="codeq-release-") as tmp:
        root = Path(tmp)
        home = root / "home"
        (home / ".codex").mkdir(parents=True)
        repo = root / "repo"
        repo.mkdir()
        (repo / "sample.py").write_text("def smoke_target():\n    return 42\n")
        env = dict(os.environ, HOME=str(home), USERPROFILE=str(home), CODEX_HOME=str(home / ".codex"))

        def run(*command):
            return subprocess.run([binary, *command], cwd=repo, env=env,
                                  capture_output=True, text=True, check=True).stdout

        capabilities = json.loads(run("--json", "capabilities"))["data"]
        assert capabilities["build_revision"] != "unknown", capabilities
        assert len(capabilities["guide_blake3"]) == 64, capabilities
        if args.version:
            assert capabilities["version"] == args.version.lstrip("v"), capabilities
        run("install", "--agent", "codex")
        actions = json.loads(run("--json", "doctor", "--agent", "codex"))
        assert "healthy" in json.dumps(actions) and "failed" not in json.dumps(actions), actions
        navigation = json.loads(run("--json", "explore", "--at", "sample.py:2"))["data"]
        assert navigation["focus"][0]["name"] == "smoke_target", navigation
        assert any("return 42" in chunk["text"] for chunk in navigation["source"]), navigation
        print(json.dumps({"version": capabilities["version"], "revision": capabilities["build_revision"],
                          "doctor": "healthy", "navigation": "passed"}))


if __name__ == "__main__":
    main()
