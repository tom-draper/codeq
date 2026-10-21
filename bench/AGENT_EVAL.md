# Live agent evaluation

`agent_eval.py` compares task completion with generic file tools and Codeq MCP.
It supports a small local smoke fixture and pinned tasks on real repositories.

## Check without model calls

Run the local scoring and isolation tests:

```sh
python3 -m unittest discover -s bench -p test_agent_eval.py
```

For real tasks, populate `bench/.corpus/requests` and `bench/.corpus/express`
using `python3 bench/corpus.py sync`, or supply an existing corpus with `--corpus`.
The full commit IDs in the manifest must exist locally. Preparation uses
`git archive`, so corpus working-tree edits do not affect task source.

Verify source pins and edit oracles:

```sh
python3 bench/agent_eval.py --tasks bench/agent_eval_tasks/tasks.json --self-check
```

Self-check needs no Codeq binary, Claude installation, or provider connection.
It verifies answer evidence hashes. Each edit's original behavior must fail its
external oracle, and its supplied known-good patch must pass.

Build Codeq, then inspect the complete run plan and MCP schemas:

```sh
python3 bench/agent_eval.py --tasks bench/agent_eval_tasks/tasks.json \
  --codeq target/debug/codeq --dry-run
```

The dry run reports the planned call count and preflight checks without model
calls. It tolerates a missing agent executable. CI runs local scoring and source
isolation tests without downloading the corpus or calling models.

## Run the real repository tasks

Install and authenticate Claude Code. Pin a model, then run:

```sh
python3 bench/agent_eval.py --tasks bench/agent_eval_tasks/tasks.json \
  --profiles generic core all --codeq target/release/codeq --model sonnet \
  --output /tmp/codeq-agent-eval.json
```

The default real matrix has six tasks and three profiles, making 18 model calls.
These calls incur provider cost. `--timeout-seconds` and `--max-turns` bound each
call. Source and oracle checks run before the first model call. The process exits
nonzero if any run fails validation or the agent fails.

Every profile has identical generic list, literal-search, read, and write tools.
`core` and `all` add the corresponding Codeq MCP tools. Claude's built-in tools
are disabled. Generic traversal skips Git metadata, Codeq indexes, and Python
bytecode caches. No shell or test-execution tool is exposed to the agent.
Profile order rotates deterministically across tasks.

Without `--tasks`, the original two-task local fixture remains available.
`--codeq-tools core` selects generic versus core for that fixture. Explicit
`--profiles` selects a different matrix for either suite.

## Tasks and scoring

The real manifest covers these tasks:

| Task | Repository | Check |
| --- | --- | --- |
| Predict settings merging | Requests | Structured JSON answer |
| Find settings-merging implementation | Requests | Defining path and symbol |
| Select header-parser and encoding tests | Requests | Sorted test identifiers |
| Predict disabled ETag behavior | Express | Exact answer |
| Support application MIME types ending in `+json` | Requests | Runtime behavior and allowed edits |
| Rename the Content-Type helper across source and tests | Requests | Runtime behavior and syntax references |

Each task/profile gets a fresh archive of the pinned commit. Expected answers,
oracles, and solution patches stay outside the agent workspace. Answer tasks
must leave source unchanged. Edit tasks must change exactly their declared files.
Equivalent implementations can pass. Oracles run in separate Python processes
with a timeout and bytecode generation disabled.

The edit oracles load selected real function definitions through Python's AST.
They require no third-party packages and do not use Codeq to extract the code
being judged. They test helper behavior, not full Requests integration. The
rename oracle also checks production and test references.

Task preparation, snapshots, and scoring live in `agent_eval_tasks.py`. The runner
owns agent invocation and event collection. This boundary allows independent
oracle checks without a provider and keeps task-specific rules out of the run
loop. New task kinds require an explicit parser branch rather than plugins.

The smoke fixture checks an integer answer and an edit that skips cancelled
items. Its edit checker runs the fixture tests, requires the final status, and
rejects changes to tests or other source files without prescribing code spelling.

## Reports and limits

Reports include the manifest digest, pinned commits, source digests, agent and
model versions, tool schemas, final answers, validation diagnostics, tool calls
and results, stderr, elapsed time, and model token usage. Usage selects one event
representation instead of adding streamed and message totals together.

These are curated tasks, not representative coverage of every repository or
language. Answer evidence hashes bind reviewed expectations to source. Self-check
does not independently rediscover those answers. Passing a helper oracle does
not prove full package compatibility. Repeat live runs before drawing conclusions
about profile or model quality. Dry runs and self-checks establish no completion
rate, token advantage, or speedup.
