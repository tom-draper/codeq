# Set up codeq for your coding agent

Install the `codeq` executable first, as described in the [README](../README.md#install).
Then preview and apply the agent configuration:

```sh
codeq install --dry-run
codeq install
codeq doctor
```

`codeq install` registers the running executable's absolute path as an MCP server
and copies its bundled guide into skill directories. It checks the binary's MCP
handshake before registration and preserves unrelated servers and settings.
`codeq doctor` checks configuration, guides, and MCP startup without changing
configuration or guides. A failed check returns a nonzero exit status.

Restart configured agents to load the server and guide in a new session.
Setup is per user and applies across repositories on this machine. Remote
agent hosts need their own installation.

## Select an agent

By default, the installer configures agents whose settings or directories it
finds. To target one agent, including an agent it has not detected:

```sh
codeq install --agent cursor
```

The installer uses these locations:

| Agent value | MCP configuration | Skill guide |
| --- | --- | --- |
| `claude-code` | `~/.claude.json` | `~/.claude/skills/codeq/SKILL.md` |
| `cursor` | `~/.cursor/mcp.json` | `~/.agents/skills/codeq/SKILL.md` |
| `windsurf` | `~/.codeium/windsurf/mcp_config.json` | `~/.codeium/windsurf/skills/codeq/SKILL.md` |
| `gemini` | `~/.gemini/settings.json` | `~/.agents/skills/codeq/SKILL.md` |
| `codex` | `$CODEX_HOME/config.toml`, default `~/.codex/config.toml` | `~/.agents/skills/codeq/SKILL.md` |

The guide comes from [the repository's skill file](../.agents/skills/codeq/SKILL.md),
compiled into the executable. Installation does not append instructions to
`AGENTS.md` or `CLAUDE.md`. Whether an agent loads and follows the guide depends
on the host.

Use `codeq install --no-skill` to register only the MCP server. Doctor still
reports a missing guide after this option.

## Update the executable and guide

Keep the registered executable at its installed path. After upgrading or moving
it, or adding an agent, rerun setup:

```sh
codeq install
codeq doctor
```

Restart the agent after the update. To inspect the instructions bundled with
the executable, run `codeq guide`. To compare a saved guide with that executable:

```sh
codeq guide --check ~/.agents/skills/codeq/SKILL.md
```

A mismatch returns a nonzero exit status without changing the file. Check the
guide path for your agent in the table above.

The installer preserves customized codeq entries and guides by default.
To replace them, run `codeq install --force`. It backs up the prior content so
uninstall can restore it. Differing entries from an older, unmanaged installation
also require `--force` to replace.

Ownership records and backups live in `~/.codeq/install.json`. Keep that file
for later updates and removal. If setup is interrupted or reports a failure,
inspect the reported actions and rerun the same command. A failed run can leave
some agents configured and others unfinished.

## Remove agent configuration

Remove one agent's setup or all managed registrations and guides:

```sh
codeq uninstall --agent cursor
codeq uninstall
```

Uninstall preserves unrelated settings and content edited since installation.
It restores backed-up content when the installed content is still unchanged.
A shared guide remains until its last managed registration is removed.
Use `--no-skill` to leave guides alone.

## Register the MCP server manually

`codeq mcp` serves MCP over stdio. For a host with an `mcpServers` configuration,
add an entry like this, using the absolute path to your executable:

```json
{
	"mcpServers": {
		"codeq": {
			"command": "/absolute/path/to/codeq",
			"args": ["mcp"]
		}
	}
}
```

Set the server's working directory to the repository you want to query.
The host's configuration format determines how you set the working directory.

Query tools have names such as `codeq_map`, `codeq_explore`, and `codeq_read`.
They return the CLI's compact text with diagnostic notes appended. Use the CLI
with `--json` when you need structured results for a script.

## Choose the MCP tool set

`codeq mcp` exposes all query tools by default. To offer only routine navigation
tools, set the server's arguments to `["mcp", "--tools", "core"]` and restart it.
The core profile includes `map`, `tree`, `search`, `symbols`, `definitions`,
`explore`, `read`, `callers`, `callees`, `api`, `deps`, and `tests`.

Use `--tools all` for advanced tools such as `astgrep`, `affected`, and `context`.
The core profile rejects calls to tools it does not advertise. All commands
remain available through the CLI. MCP initialization includes brief query
guidance; `codeq guide` prints the full guide bundled with the running executable.

Compare profiles with the live agent evaluation:

```sh
python3 bench/agent_eval.py --codeq-tools core --dry-run
python3 bench/agent_eval.py --codeq-tools all --dry-run
```

The dry run checks setup without model calls. Remove `--dry-run` to evaluate
task completion with a configured Claude Code installation.

## MCP queries and cancellation

The MCP server runs queries in process, reusing repository connections and
caches. Every query checks freshness. Edits and index refreshes from other
processes invalidate cached results.

The default is four query workers, each with its own connection. Set
`--workers 1` for sequential query execution, or choose up to 16 workers.
Ping and tool discovery remain responsive while queries run. Responses can
arrive out of order; match them by request ID. JSON-RPC batches return one array
after their requests finish.

The queue holds at most 32 waiting queries. If it is full, the server returns an
error asking the client to retry after an active query completes.
`notifications/cancelled` skips queued work and interrupts active SQLite
statements. Source scans already in progress can finish internally; their
cancelled results are discarded. Closing stdin drains accepted requests.

## Check additional guide copies

`codeq doctor` also checks additional guides under `~/.codex/skills/codeq` and
`~/.cursor/skills/codeq`. A custom `CODEX_HOME` replaces `~/.codex` in this check.
An outdated copy can give an agent different instructions from the managed guide.
Doctor reports its path without changing it. Compare it with
`codeq guide --check <path>`, then update or remove that copy manually.
