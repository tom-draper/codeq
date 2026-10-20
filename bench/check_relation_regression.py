#!/usr/bin/env python3
"""Prove relation_inserts.py rejects a deliberately slower executable.

Usage: python3 bench/check_relation_regression.py target/release/codeq
"""
import json
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    benchmark = Path(__file__).with_name("relation_inserts.py")
    with tempfile.TemporaryDirectory(prefix="codeq-regression-check-") as directory:
        root = Path(directory)
        wrapper = root / "delayed-codeq"
        wrapper.write_text("#!/usr/bin/env python3\nimport os, sys, time\ntime.sleep(0.15)\nos.execv(" + repr(str(binary)) + ", [" + repr(str(binary)) + "] + sys.argv[1:])\n")
        wrapper.chmod(0o755)
        report = root / "report.json"
        result = subprocess.run([sys.executable, str(benchmark), str(binary), str(wrapper),
                                 "--runs", "5", "--case", "small_cold",
                                 "--max-regression-percent", "100", "--output", str(report)],
                                capture_output=True, text=True, timeout=60)
        assert result.returncode == 1, result
        data = json.loads(report.read_text())
        assert data["regressions"] == ["small_cold"], data
        assert data["errors"] == data["answer_mismatches"] == 0
        assert "median regression threshold exceeded: small_cold" in result.stderr, result
        print("Delayed executable rejected; outputs and database rows still match.")


if __name__ == "__main__":
    main()
