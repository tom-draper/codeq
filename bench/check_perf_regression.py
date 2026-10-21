#!/usr/bin/env python3
"""Prove the suite rejects a slower executable while all query answers still match."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="codeq-suite-regression-") as directory:
        root = Path(directory)
        wrapper = root / "delayed-codeq"
        wrapper.write_text("#!/usr/bin/env python3\nimport os, sys, time\ntime.sleep(0.1)\nos.execv(" + repr(str(binary)) + ", [" + repr(str(binary)) + "] + sys.argv[1:])\n")
        wrapper.chmod(0o755)
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("perf_suite.py")),
                                 str(binary), str(wrapper), "--suite", "scale_queries", "--runs", "5",
                                 "--max-regression-percent", "50", "--output-dir", str(root / "reports")],
                                capture_output=True, text=True, timeout=180)
        report = json.loads((root / "reports/summary.json").read_text())
        assert result.returncode == 1, result
        assert not report["failures"], report
        assert "scale_queries/100_untested" in report["regressions"], report
        child = json.loads((root / "reports/scale_queries.json").read_text())
        assert child["errors"] == child["answer_mismatches"] == 0
        assert len(report["workloads"]) == 10, report
        wrapper.write_text("#!/bin/sh\nexit 0\n")
        noop = subprocess.run([sys.executable, str(Path(__file__).with_name("perf_suite.py")),
                               str(binary), str(wrapper), "--suite", "scale_queries", "--runs", "5",
                               "--output-dir", str(root / "reports")],
                              capture_output=True, text=True, timeout=30)
        failed = json.loads((root / "reports/summary.json").read_text())
        assert noop.returncode == 1 and failed["failures"] and not failed["workloads"], (noop, failed)
        assert not (root / "reports/scale_queries.json").exists()
        print("Suite rejected the delayed executable; all ten workloads retained identical answers.")
        print("No-op executable rejected; stale child report was removed.")


if __name__ == "__main__":
    main()
