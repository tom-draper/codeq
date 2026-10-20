# Live agent evaluation

`agent_eval.py` runs the same answer and edit tasks with generic file tools and
with Codeq MCP tools. It uses a pinned local fixture, so it does not download or
modify a third-party repository.

## Run

Install and authenticate Claude Code, then build Codeq. Run:

```sh
python3 bench/agent_eval.py --codeq target/release/codeq --model sonnet \
  --output bench/results/agent_eval.json
```

The script makes four model calls, one for each task and tool profile. Those
calls incur provider cost. `--dry-run` captures the agent version and both tool
schemas without calling a model. Use `--timeout-seconds` and `--max-turns` to
bound each call. Pin the model name when comparing runs.

## Tasks and scoring

The answer task asks for the current result of `sum_prices` on a fixed input. The
checker parses the final JSON and compares the answer with the literal expected
value. The edit task asks the agent to skip cancelled items. The checker runs
the fixture's unit tests, checks the changed source, requires the final status,
and rejects changes to the tests or other files.

Each task starts from a fresh copy for each tool profile. The report includes a
hash of the fixture, the agent and model versions, the complete MCP tool
schemas, final answer, validation results, tool calls and results, failed calls,
follow-up reads, stderr, elapsed time, and model input and output tokens. The
generic tools are a small MCP server with file listing, literal search, line
reads, and file writes. Both profiles also run with Claude's built-in tools
disabled.

## Limits

The fixture checks the runner and agent behavior on small controlled tasks. It
does not estimate performance on large repositories or establish that every
Codeq query returns correct facts. Add tasks with pinned source revisions and
independent answer and edit oracles before using the suite to compare models or
make broad quality claims. Model outputs and tool choices can vary between runs.

Token totals come from Claude Code's usage events. Elapsed time covers the full
CLI process, including model requests and tool calls. Tool schemas are recorded
as JSON so changes in the available controls can be compared directly.
