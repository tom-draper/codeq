# Complete answers on real repositories

Run after syncing the existing corpus with `python3 bench/corpus.py sync`:

```sh
python3 bench/real_workflows.py --codeq target/release/codeq
python3 bench/real_workflows.py --codeq /path/to/new --compare-codeq /path/to/old
python3 bench/real_workflows.py --codeq target/release/codeq --explore-tokens 100
```

`--corpus` selects an existing corpus directory. The harness refuses different
commits, tracked modifications, and changed oracle bodies. Missing repositories
must be synced first. It requires Python 3, Git, ripgrep, and sed.

## Tasks and correctness

The manifest pins five repositories and reviewed source spans, with hashes of
each required body. The fixtures cover Python methods and helpers, JavaScript
property-assigned methods, a Go redirect helper, and a Rust logging method.
Source ranges were checked against the implementations, including their closing
statements. They are static fixtures, independent of the binary under test.

Each repository supplies three tasks:

- Read an already-known exact range. Codeq and sed must return identical bytes.
- Read the complete definition around an existing hit, without a known end line.
- Find and read an implementation from a content pattern, including its complete body.

Four repositories also supply a task requesting a function and a named helper.
This compares `explore` with a narrower declaration search and source reads.
It does not ask for the whole call neighbourhood, so explore's additional
callers, callees, and imports count as overhead for this particular question.

All required bodies must be present, with every non-indentation source character
in order. A missing closing statement, modified body, or folded implementation
fails. When explore's source is incomplete, the harness reads advertised candidate
positions in one batched follow-up. That output and command count toward cost.
The 100-token run verifies this fallback for all four exploration tasks.

The baseline uses declaration-shaped regexes when symbol names are known,
rather than reading every mention. Source windows start at 20 lines on either
side of each hit, merge overlaps, and double until complete. A completeness
oracle tells the harness when to stop. Neither tool receives unknown golden
boundaries, but oracle-assisted stopping favors the baseline over an agent
that may need extra inspection. Every attempted window counts.

## Measurements and limits

Costs include canonical command input, stdout, and stderr. Executable paths are
normalized to `codeq`; shell command text measures input, excluding tool-protocol
wrappers. Tokens are `ceil(total_bytes / 4)`, an approximation. Calls count external
command invocations, not actual model tool rounds, which can batch shell commands.
Indexes are warmed separately, excluding installation and cold-index costs.

The report records source revisions, binary hashes, commands, and separate byte
counts. Any failed command or incomplete answer exits nonzero. These are fixed,
curated workflows, not a model-agent evaluation, call-graph semantic proof, or
latency benchmark. Definition inclusion checks alone cannot establish that a
reported relationship resolves to the correct runtime implementation.

## Results, 2026-10-11

[Recorded results](results/real_workflows_20261011.json) compare main at `40fa658`
with the qualifier and ambiguity changes at `fc976f9`. Debug binaries were used
for correctness and output measurements; no timings are reported. All 19 tasks
pass for both binaries.

| Task group | Improved codeq commands / approximate tokens | Main codeq commands / approximate tokens | rg + sed commands / approximate tokens |
| --- | ---: | ---: | ---: |
| Known exact ranges, 5 tasks | 5 / 884 | 5 / 884 | 5 / 873 |
| Bodies from positions, 5 tasks | 5 / 886 | 5 / 886 | 7 / 2,918 |
| Search and complete source, 5 tasks | 5 / 1,210 | 5 / 1,210 | 10 / 1,975 |
| Function and named helper, 4 tasks | 4 / 7,601 | 4 / 8,605 | 10 / 4,839 |

Position reads and source searches use fewer commands and bytes on these tasks.
Exact-range output bytes are identical, with small command-input overhead for
codeq. Deferring ambiguous source reduces exploration cost by about 12% versus
main, but explore still returns more context than these narrow helper questions
need. It costs more tokens than the baseline in every exploration fixture.

Choose position reads or source searches for a specific body. Use explore when
the question needs the wider neighbourhood. This experiment identifies remaining
exploration overhead rather than establishing a general token advantage.
