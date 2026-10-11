#!/usr/bin/env python3
"""Compare release binaries on verified Python definition queries.

Each pair uses the same fixture. Alternate before/after, with a discarded warmup.
Cold means no index, not cold filesystem caches. Timings include process startup,
refresh, SQLite writes and query rendering. Validation is outside the timed region.
"""
import argparse
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time


def run(binary, root, symbol="target", path="sample_0.py", line=1):
    start = time.perf_counter_ns()
    result = subprocess.run([str(binary), '--root', str(root), 'definitions',
                             symbol, '--path', path, '--json', '--meta'], capture_output=True, check=True)
    elapsed = (time.perf_counter_ns() - start) / 1e6
    payload = json.loads(result.stdout)
    rows = payload[payload['rows']]
    assert len(rows) == 1, rows
    assert rows[0]['name'] == symbol and rows[0]['path'] == path, rows
    assert rows[0]['line'] == line, rows
    return elapsed, payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    parser.add_argument('--repo', type=Path, help='Also measure edits in a tracked repository snapshot')
    parser.add_argument('--runs', type=int, default=7)
    args = parser.parse_args()
    assert args.runs >= 5
    binaries = [args.before.resolve(), args.after.resolve()]
    report = {'runs_per_side': args.runs, 'failures': 0, 'cases': []}
    with tempfile.TemporaryDirectory(prefix='codeq-refresh-perf-') as tmp:
        for count in (1, 128):
            roots = [Path(tmp) / f'{count}-{side}' for side in range(2)]
            for root in roots:
                root.mkdir()
                for i in range(count):
                    name = 'target' if i == 0 else f'helper_{i}'
                    (root / f'sample_{i}.py').write_text(
                        f'def {name}(value):\n    return normalize(value) + 1\n')
            for mode in ('cold', 'edited', 'warm'):
                times = [[], []]
                for iteration in range(args.runs + 1):
                    results = []
                    for side, binary in enumerate(binaries):
                        root = roots[side]
                        if mode == 'cold':
                            shutil.rmtree(root / '.codeq', ignore_errors=True)
                        elif mode == 'edited':
                            (root / 'sample_0.py').write_text(
                                f'def target(value):\n    return normalize(value) + {iteration + 2}\n')
                        elapsed, result = run(binary, root)
                        if mode != 'warm':
                            assert result['index'].get('reindexed_files', 0) == (count if mode == 'cold' else 1), result
                        else:
                            assert result['index'].get('reindexed_files', 0) == 0, result
                        results.append(result[result['rows']])
                        if iteration:
                            times[side].append(elapsed)
                    assert results[0] == results[1], results
                case = {'files': count, 'mode': mode}
                for side, label in enumerate(('before', 'after')):
                    values = times[side]
                    case[label] = {'median_ms': statistics.median(values),
                                   'range_ms': [min(values), max(values)],
                                   'samples_ms': values}
                report['cases'].append(case)
        if args.repo:
            repo = args.repo.resolve()
            tracked = subprocess.run(['git', '-C', str(repo), 'ls-files', '-z'],
                                     capture_output=True, check=True).stdout.split(b'\0')
            paths = [Path(p.decode()) for p in tracked if p]
            roots = [Path(tmp) / f'repo-{side}' for side in range(2)]
            for root in roots:
                for path in paths:
                    (root / path).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(repo / path, root / path)
            query_path = 'bench/bench.py'
            original = (repo / query_path).read_text()
            line = original.splitlines().index('def main():') + 1
            for side, binary in enumerate(binaries):
                run(binary, roots[side], 'main', query_path, line)
            for mode in ('edited', 'warm'):
                times = [[], []]
                for iteration in range(args.runs + 1):
                    results = []
                    for side, binary in enumerate(binaries):
                        root = roots[side]
                        if mode == 'edited':
                            (root / query_path).write_text(original + f'\n# edit {iteration}\n')
                        elapsed, result = run(binary, root, 'main', query_path, line)
                        assert result['index'].get('reindexed_files', 0) == (1 if mode == 'edited' else 0)
                        results.append(result[result['rows']])
                        if iteration:
                            times[side].append(elapsed)
                    assert results[0] == results[1], results
                case = {'repo': 'tracked snapshot', 'files': len(paths), 'mode': mode}
                for side, label in enumerate(('before', 'after')):
                    values = times[side]
                    case[label] = {'median_ms': statistics.median(values),
                                   'range_ms': [min(values), max(values)],
                                   'samples_ms': values}
                report['cases'].append(case)
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
