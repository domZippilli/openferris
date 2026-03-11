# OpenFerris Specification

A reliable AI personal assistant that runs on Linux, built with Unix philosophy: simple components, composed together, scheduled with cron.

## Design Principles

1. **Reliability over features.** OpenFerris does specific things, on time, every time.
2. **Unix philosophy.** Small programs that do one thing well. Compose them. Use cron, not a custom scheduler.
3. **Simplicity.** No complex orchestration frameworks. Markdown files, SQLite, crontab entries.

## Architecture Overview

```
                      All clients communicate via TCP localhost
                      ─────────────────────────────────────────

┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
│ CLI      │ │ Cron     │ │ Telegram │ │ Gmail    │ │   TUI    │
│ commands │ │ jobs     │ │ Listener │ │ Listener │ │          │
└────┬─────┘ └────┬─────┘ └────┬─────┘ └────┬─────┘ └────┬─────┘
     │            │            │            │            │
     └────────────┴────────┬───┴────────────┴────────────┘
                           │  TCP localhost
                           v
              ┌────────────────────────────┐
              │      Central Daemon        │  The brain:
              │    (openferris daemon)     │  - Owns LLM session
              │                            │  - Queues requests
              │  ┌──────┐ ┌─────┐ ┌─────┐ │  - Runs agent loop
              │  │Skills│ │Tools│ │ LLM │ │  - Executes tools
              │  │(.md) │ │(Rs) │ │ API │ │  - Sends responses
              │  └──────┘ └─────┘ └─────┘ │
              └────────────────────────────┘

   ┌──────────────────────────────────────────┐
   │              Linux crontab               │  Cron jobs are just CLI
   │  0 7 * * * openferris dailybriefing      │  invocations that send
   │  0 18 * * 5 openferris weeklyreview      │  requests to the daemon
   └──────────────────────────────────────────┘
```

## Components

### 1. CLI (clap)

The binary is `openferris`. Each skill is a subcommand:

```bash
openferris dailybriefing          # Send "run dailybriefing" to the daemon
openferris daemon                 # Start the central daemon
openferris listener telegram      # Start the Telegram listener daemon
openferris listener gmail         # Start the Gmail listener daemon
openferris tui                    # Interactive terminal session with the daemon
```

**The daemon is the brain.** All LLM interaction and tool execution happens in the daemon process, which maintains a single LLM session with persistent context.

CLI skill subcommands are **thin clients**: they connect to the daemon over TCP, send a request (e.g., "run dailybriefing"), wait for the response, and print it. Cron jobs use the same path — `openferris dailybriefing` in a crontab just sends a request to the daemon like any other client.

The TUI is another client that maintains a persistent TCP connection for interactive back-and-forth conversation with the daemon/LLM.

### 2. SOUL.md

A global personality file that defines who the agent is. Loaded into every LLM call as part of the system prompt. Lives at a known location (e.g., `~/.config/openferris/SOUL.md` or workspace root).

Carried over from OpenClaw. The agent does not modify this file.

### 3. Skills

Skills follow the [AgentSkills](https://agentskills.io) format: a directory containing a `SKILL.md` with YAML frontmatter + markdown body.

```
skills/
├── daily-briefing/
│   └── SKILL.md
├── default/
│   └── SKILL.md
├── email-summary/
│   └── SKILL.md
└── ...
```

A skill definition describes:
- **What the skill does** (instructions for the LLM)
- **Which tools are available** (references to tool definitions)
- **How to compose them** (step-by-step guidance for the agent)

Skills are the LLM context loaded when a subcommand runs. The skill tells the agent what to do and which tools it can call.

### 4. Tools

Tools are capabilities provided by the system. Each tool is a Rust module with:
- A **definition file** (in a tools directory) describing the tool's interface — what it does, what inputs it takes, what it returns. This description is included in the LLM context so the agent knows how to request it.
- A **Rust implementation** that actually executes the tool.

```
tools/
├── brave_search/
│   ├── tool.md          # Description for the LLM
│   └── (Rust module)    # Implementation
├── weather/
│   ├── tool.md
│   └── (Rust module)
├── send_telegram/
│   ├── tool.md
│   └── (Rust module)
└── ...
```

Tool invocation flow (simplified, not native LLM function calling):
1. The LLM receives tool descriptions as part of its context
2. The LLM outputs a structured marker indicating which tool to call and with what parameters
3. OpenFerris parses the marker, executes the tool locally, and feeds the result back to the LLM
4. Repeat until the agent is done

The exact marker format is TBD — could be XML-style tags, JSON blocks, or similar. Must be simple enough for local models (llama.cpp) to produce reliably.

**Tool focus list:** Skills declare which tools are visible during execution. This is a focus mechanism — it reduces prompt noise and keeps the LLM on task. Since the agent can create its own skills in the workspace, it can give itself access to any registered tool. The real security boundary is the tool registry: only tools compiled into the binary exist. The agent cannot invent new capabilities.

### 5. Secrets

Service credentials stored separately from tool definitions. Gitignored.

```
~/.config/openferris/secrets/
├── brave.toml
├── telegram.toml
├── gmail.toml
└── ...
```

Tools reference secrets by service name. The runtime loads and injects them at execution time.

### 6. Scheduling (cron)

No custom scheduler. Skills are scheduled via standard Linux crontab entries:

```crontab
0 7 * * *   openferris dailybriefing
0 18 * * 5  openferris weeklyreview
```

Each skill that needs to run on a schedule gets its own crontab entry. Simple, transparent, debuggable with standard Linux tools.

### 7. Central Daemon

The single brain of OpenFerris. A long-running process (`openferris daemon`) managed as a systemd user service. If the daemon is not running, all clients (CLI, cron, TUI) fail immediately with a clear error.

`openferris daemon` does the following:
- **Owns the LLM session** — maintains persistent context (SOUL, conversation history, daily notes)
- Listens on TCP localhost for requests from all clients (CLI subcommands, cron jobs, listeners, TUI)
- Queues requests and processes them sequentially, one at a time
- Loads the appropriate skill context (or default skill for freeform messages)
- Runs the agent loop: sends prompt to LLM, parses tool calls, executes tools, feeds results back
- Sends responses back to the originating client via the TCP connection

### 8. Interface Daemons (Listeners)

Separate processes, one per channel:
- `openferris listener telegram` — listens for Telegram messages
- `openferris listener gmail` — polls/watches for new emails

Each listener:
- Connects to its respective service using credentials from secrets
- Receives incoming messages
- Forwards them to the central daemon's queue
- Receives responses from the central daemon and sends them back

Unix philosophy: each listener is good at one thing (bridging one service). Adding a new channel means writing a new listener, not modifying the core.

### 9. LLM Backend

Pluggable. The system supports multiple backends:

1. **llama.cpp** (primary) — local inference via llama.cpp server (user manages the server; OpenFerris connects to its OpenAI-compatible API). Assumes generous context window (100-200k tokens).
2. **Claude CLI** — `claude -p` for Anthropic models
3. (Future) Direct API calls to various providers

Configuration specifies which backend to use, endpoint URL, model name, etc.

```toml
# ~/.config/openferris/config.toml
[llm]
backend = "llamacpp"          # or "claude-cli", "anthropic-api"
endpoint = "http://localhost:8080"
model = "..."
```

### 10. State & Storage

- **SQLite** — for structured data: daily notes, conversation history, memory. Lives at `~/.local/share/openferris/openferris.db` or similar XDG path.
- **Markdown files** — for human-readable state where appropriate (SOUL.md, skill definitions, user-facing notes).
- **Vector DB** (future) — for semantic memory/retrieval over daily notes and conversation history.

Daily notes: the agent keeps a daily log of interactions, tasks completed, and notable events. Stored in SQLite with date-indexed entries. This lets the agent "remember" context across runs.

## Example: Daily Briefing

The `dailybriefing` skill demonstrates the full flow:

1. Cron fires `openferris dailybriefing` at 7am
2. The CLI process connects to the daemon via TCP, sends `{"skill": "dailybriefing"}`
3. The daemon loads `skills/daily-briefing/SKILL.md` into the LLM session (SOUL.md is already in context)
4. The skill says: "Get weather, get time, get news, compose a briefing, send via Telegram"
5. Available tools (per skill tool list): `weather`, `news`, `datetime`, `send_telegram`
6. The daemon runs the agent loop: LLM calls tools, daemon executes them, feeds results back
7. The LLM composes the briefing and calls `send_telegram` to deliver it
8. The daemon sends a completion response back to the CLI process, which exits

## Configuration

```toml
# ~/.config/openferris/config.toml

[user]
timezone = "America/New_York"
zip_code = "10001"

[llm]
backend = "llamacpp"
endpoint = "http://localhost:8080"

[daemon]
listen = "127.0.0.1:7700"      # TCP address for the central daemon
```

## Directory Layout

```
~/.config/openferris/
├── config.toml             # Main configuration
├── SOUL.md                 # Agent personality
├── secrets/                # Service credentials (gitignored)
│   ├── telegram.toml
│   └── ...
└── skills/                 # User-level skill overrides
    └── ...

~/.local/share/openferris/
├── openferris.db           # SQLite database
└── logs/                   # Logs

# Project/source tree
openferris/
├── src/
│   ├── main.rs             # CLI entry point (clap)
│   ├── agent/              # Agent loop (LLM interaction, tool parsing)
│   ├── skills/             # Skill loader
│   ├── tools/              # Tool implementations
│   │   ├── brave_search.rs
│   │   ├── weather.rs
│   │   └── ...
│   ├── llm/                # LLM backend abstraction
│   │   ├── mod.rs
│   │   ├── llamacpp.rs
│   │   └── claude_cli.rs
│   ├── daemon/             # Central daemon (LLM session, agent loop, queue)
│   ├── tui/                # Terminal UI client
│   ├── listeners/          # Interface daemons
│   │   ├── telegram.rs
│   │   └── gmail.rs
│   └── storage/            # SQLite, daily notes
├── skills/                 # Bundled skill definitions
│   ├── daily-briefing/
│   │   └── SKILL.md
│   └── default/
│       └── SKILL.md
├── tools/                  # Tool description files (for LLM context)
│   ├── brave_search.md
│   ├── weather.md
│   └── ...
├── SOUL.md                 # Default SOUL (copied to user config on init)
├── SPEC.md                 # This file
├── CLAUDE.md               # Claude Code instructions
├── Cargo.toml
└── Cargo.lock
```

## Resolved Decisions

- **Daemon IPC** — TCP on localhost. More robust than Unix sockets.
- **Daemon protocol** — JSON lines over TCP.
- **llama.cpp** — User-managed, already running. OpenFerris connects to it. Assume 100-200k context.
- **Tool invocation** — Structured markers parsed by OpenFerris. LLM has flexibility to compose tool calls. Per-skill tool lists control prompt focus; the tool registry is the security boundary.
- **No heartbeat** — Cron only. If a heartbeat-like check is needed later, it's just another cron job.
- **Output routing** — Skills handle their own delivery. A skill explicitly calls a delivery tool (e.g., `send_telegram`) as part of its instructions. No global output router.
- **Freeform mode** — When the daemon receives a message that doesn't match a specific skill, it loads SOUL.md + the default skill that helps the agent figure out how to help the user, with access to all tools.
- **Concurrency** — One request at a time. Queue is processed sequentially.

## Open Questions

1. **Tool marker format** — What structured format should the LLM use to call tools? Options: XML tags (`<tool name="...">params</tool>`), JSON blocks, or a custom syntax. Must be reliably producible by local models via llama.cpp. Needs experimentation.
