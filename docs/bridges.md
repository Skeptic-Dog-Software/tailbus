# Bridges

A bridge answers its agent's messages by running a headless agent CLI once
per message and sending back what it prints. Configure one in
`config.toml` and start it with `tailbus bridge install <agent>`:

```toml
[bridges.coder]
owner = "you@example.com"
command = "/usr/local/bin/your-agent"
args = ["--print", "--session-dir", "{SESSION_DIR}"]
working_directory = "/Users/you/agent-work"
home = "/Users/you/agent-home"
path = "/usr/local/bin:/usr/bin:/bin"
```

| Setting | Means |
| --- | --- |
| `kind` | `"claude"`, `"codesmith"`, `"omp"` or `"pi"`: an agent tailbus ships defaults for, in `src/bridges/`: its args, options, environment and usage come from there, and the table says only what's this machine's. |
| `owner` | Who a message must come from: the bus's `owner`. Required. |
| `command`, `args` | What to run. `args` can use the placeholders below. |
| `extra_args` | More arguments, after `args`: what this machine adds to its kind's defaults, such as a provider's extension. |
| `working_directory` | Where it runs. Required. |
| `home`, `path` | Its `HOME` and `PATH`. Required. `home` can't be your own. |
| `session` | `"fresh"`, the default, gives every request a new session. `"per-sender"` keeps one conversation with each sender. |
| `max_concurrent` | The most runs at once. Default 1. |
| `timeout_secs` | How long a run can take. Default 600; each kind's defaults give 14400. A request can ask for less, or up to `options.max_timeout_secs`. |
| `idle_secs` | How long a run can go quiet before its process tree is woken, and, if nothing stirs, ended and tried again in a fresh run, up to 3 tries in all. A try after one that was ended, or cut off when the bridge stopped, doesn't start when the earlier try left changed files in a git working directory and the run can write: the request fails, naming them, so you can look before sending it again. Default 1200; 0 turns it off. Either way, `tailbus bridge status` shows when each running request last used CPU or changed a file, which tells a slow run from a stalled one. |
| `env`, `secrets` | More environment, and variables read from files only you can read. |
| `attachments` | `"path"`, the default, tells the agent where each attachment is. `"inline"` puts each one's text in its message, for an agent that can read only its working directory, since attachments are kept outside it. |
| `options.no_tools_args` | The arguments that run the agent with no tools at all, in place of `tools_args`, when a request asks for none. pi's are `--no-tools --no-mcp`, and Claude Code's `--tools ""`. Left out, a request for no tools is refused. |
| `log_retention_days` | How long each run's archive is kept. Default 30. See [What's kept](#whats-kept). |

| Placeholder | Is |
| --- | --- |
| `{AGENT}`, `{ID}`, `{SENDER}` | The bridge's agent, the message's ID, and who sent it. |
| `{SESSION_DIR}` | The run's session directory, which keeps sessions apart. |
| `{RUN_DIR}`, `{MESSAGE_FILE}` | A directory for this run alone, and the file holding the message. |
| `{MODEL}`, `{TOOLS}`, `{EFFORT}` | What the request asked for, where `options` allows it. |

A bridge isn't a sandbox: the agent runs as you, with what its own settings
allow.

## Keeping a bridge current

A bridge with `kind` gets its args, options, environment and usage from
tailbus's defaults for that kind, so a newer tailbus brings newer ones. Its
table says only what's this machine's, and anything it sets wins over the
defaults. A table written out in full keeps what it had. `tailbus setup
refresh` moves one whose command
is Claude Code, omp or pi to `kind` form, after backing up the config:
- It keeps what's this machine's: owner, command, home, path, working
  directory, `extra_args`, secrets, `options.workdirs`, a model
  that isn't the default, `options.models` for a kind with no default
  models, and any setting the defaults don't have.
- Flags that its `args` carried and the defaults don't, such as
  `--settings`, go to `extra_args`.
- It drops the rest, and names each key it dropped that wasn't the new
  default, so you can put back a choice you meant.

Then it restarts that bridge. The daemon says, as it starts, when a refresh
is due, and `setup refresh` names keys in a `kind` table that keep a newer
default from it.

tailbus is tested with particular versions of the agent CLIs it drives:
Claude Code 2.1.325, pi 1.5.2, and node 22.22.1 and 24.22.0. `setup refresh`
and the daemon, as it starts, ask each bridge's agent for its version. One
older than tested is something to fix; one newer is only noted.

A `codesmith` bridge must say `kind = "codesmith"`: refresh doesn't move a
table to it by its command, since `codesmith bus relay` is the same program
run another way. Each run gets its own session database
(`CODESMITH_DB`, in the session directory); for a per-sender session, set
`continue_args = ["--continue"]`. Its usage comes from `--usage-log`, one
line per model step; the files a run read aren't named yet, because CodeSmith
writes each tool call on a line of its own.

A bridge can also hand a message to a live CodeSmith session, with no kind:
`command` is `codesmith` and `args` are `["bus", "relay", "--to", "NAME",
"--from", "{SENDER}", "--id", "{ID}"]`. A repeat of the same `{ID}` gets the
first answer. CodeSmith's own docs/tailbus.md describes it.

## Tools and MCP

A request narrows the tools to some of `options.tools`, and the bridge
passes them with `tools_args` or `tools_env`, which the agent enforces.

pi connects none of the host's MCP servers unless the request names them
(`options.no_mcp_args`, `--no-mcp` for the pi kind). A request names
servers with `mcp`; the bridge's `options.mcp` lists the ones it may name,
from pi's own `mcp.json` in the bridge's home, and the call then keeps only
their tools (`options.mcp_tool`, `mcp__{SERVER}__*` for pi). Claude Code's
`--strict-mcp-config`, with no `--mcp-config`, gives none; a Claude Code
bridge that needs servers gives `--mcp-config` in its `extra_args`. omp has
no flag that turns MCP off, so its servers are what its config in the
bridge's home names.

## What's kept

Nothing a run leaves is deleted as it finishes, so you can see what each
agent did. Each request a bridge takes is kept under
`~/.local/state/tailbus/bridge/<agent>/runs/`: a folder for each try, with
the agent's session, its scratch files and what it was sent, and the run's
output and record. Once it's finished, it's packed into a `.tar.gz` named
for its time, such as `20261112T134507Z-01M7…tar.gz`, so a listing reads in
time order, and an agent that searches files later doesn't come on an
earlier run's work. `tailbus bridge log <agent> <id>` reads it, with
`--stdout` or `--stderr` for its output.

The daemon clears them once an hour, when they're 30 days old, or a
bridge's `log_retention_days`, going by the time in their names, and packs
anything finished that wasn't. Nothing else removes them. A run still open
stays however old. Everything is under the state folder, which only you can
read.
