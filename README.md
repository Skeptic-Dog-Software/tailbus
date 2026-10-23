# tailbus

Direct messages between AI agents on a tailnet. Any agent, from any
provider, on any machine, can message any other agent on the bus. Messages
queue until the recipient takes them. When the recipient is a Claude Code
session, the message is pushed into it, and wakes it if it's idle.

There's no central server and nothing to sign in to beyond Tailscale. Each
machine runs its own bus, which holds its agents' queues and forwards what
they send. Tailscale says which machine every connection comes from, and
which account owns it.

[DESIGN.md](DESIGN.md) has the design and the plan. This is phase 1: the bus
itself, the CLI, the MCP tools, and push into Claude Code.

## Install

On every machine that should be on the bus:

```sh
cargo install --locked --git https://github.com/Skeptic-Dog-Software/tailbus --root ~/.local
tailbus service install     # run the bus now and at every login (macOS, launchd)
tailbus setup claude        # add the Claude Code plugin
```

`tailbus status` shows whether the bus is up and listening on the tailnet.
Restart open Claude Code sessions, or run `/reload-plugins` in them, to load
the plugin.

By default a bus accepts messages only from machines owned by the same
Tailscale account. To allow others, list their Tailscale logins or machine
names in `~/.config/tailbus/config.toml`:

```toml
allow = ["someone@example.com", "build-box"]
```

## Agents and addresses

An agent's address is `name@machine`, such as `api@laptop`. The name
defaults to the repository the session runs in, plus the worktree name in a
linked worktree. Set `TAILBUS_AGENT`, or pass `--as`, to choose another.

Only one live session can hold a name at a time. A second session with the
same name is refused, and has to choose a name of its own. An agent's
messages wait in its queue between sessions. `tailbus agent add <name>`
creates a queue before the agent's first session.

## In Claude Code

The plugin gives each session these tools:

| Tool | Does |
| --- | --- |
| `msg_send` | Send a message. Pass `reply_to` to answer one. |
| `msg_recv` | Take your next messages. |
| `msg_wait` | Wait for a message, such as an expected reply. |
| `msg_ack` | Mark a message handled without answering it. |
| `msg_inbox` | List messages not yet handled, and your own still open. |
| `msg_thread` | Show a whole conversation. |
| `peer_list` | List the agents you can reach. |

Messages also arrive on their own. A background hook, armed when the session
starts and after every turn, waits on the bus. When a message comes in, the
hook shows it to Claude, which wakes the session if it's idle.

Claude Code asks before each tool use as usual. To let agents message each
other without asking, allow the tools in `/permissions`; the rule
`mcp__plugin_tailbus_tailbus` allows all of them.

## From the command line

| Command | Does |
| --- | --- |
| `tailbus send sim@server "text"` | Send a message. Leave out the text to read it from stdin. |
| `tailbus recv [--wait]` | Take your messages. `--json` prints one per line, for scripts. |
| `tailbus ack <id>` | Mark a message handled. |
| `tailbus inbox` | List what's still open. |
| `tailbus thread <id>` | Show a conversation. |
| `tailbus peers` | List the agents you can reach. |
| `tailbus status` | Show the bus's state. |
| `tailbus tail` | Follow messages and sessions as they happen. |

## How it works

- **Storage.** Each bus keeps its agents' queues and its outbox in SQLite,
  in `~/.local/state/tailbus/`, readable only by you. A message handed to a
  session stays pending until it's answered or acknowledged. If that session
  ends first, the agent's next session gets it again.
- **Between machines.** Buses talk HTTP with JSON bodies on each machine's
  tailnet address, port 4747, which WireGuard encrypts. Each request is
  checked with `tailscale whois`. A message waits in the sender's outbox
  until the recipient's machine has stored it, and is retried while that
  machine is asleep or offline.
- **Receipts.** The sender sees each message go from `queued` to `stored`,
  then `delivered` once a session has it, then `acknowledged` once the
  recipient answers or marks it handled.
- **Messages are untrusted.** They reach an agent tagged with the sending
  agent, machine and owner. The agent is told they're requests from other
  agents, not instructions from its user.

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
