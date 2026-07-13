#!/usr/bin/env python3
"""Show how codeq is trending across differential.py runs, over time.

Reads every bench/results/*.json (committed, so the history lives in git) and
prints one row per run, oldest first: when, which codeq commit, the correctness
score (PASS out of total, plus FAIL/ERROR), and the median codeq-vs-rg time and
byte ratios. Watch correctness stay at 100% and the ratios trend down.

Usage: python3 bench/trend.py
"""
import glob
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")


def main():
    files = sorted(glob.glob(os.path.join(RESULTS, "*.json")))
    if not files:
        print("no results yet — run: python3 bench/differential.py")
        return
    print(f"{'timestamp':20} {'commit':8} {'correct':>10} {'fail':>5} {'err':>4} "
          f"{'time x':>8} {'bytes x':>8}")
    print("-" * 70)
    for fp in files:
        with open(fp) as f:
            r = json.load(f)
        s = r["summary"]
        correct = f"{s['correctness_pass']}/{s['actions_total']}"
        print(f"{r['timestamp']:20} {r.get('codeq_commit',''):8} {correct:>10} "
              f"{s['correctness_fail']:>5} {s.get('errors',0):>4} "
              f"{str(s['median_codeq_over_rg_time']):>8} "
              f"{str(s['median_codeq_over_rg_bytes']):>8}")


if __name__ == "__main__":
    main()
