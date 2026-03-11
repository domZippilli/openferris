# OpenFerris

A reliable AI personal assistant that runs on Linux. Built with Unix philosophy: simple components, composed together, scheduled with cron.

OpenFerris uses a central daemon that owns the LLM session. Everything else — CLI commands, cron jobs, the TUI, chat listeners — are thin clients that send requests to the daemon over TCP.

## Prerequisites

- **Rust 1.93+** (2024 edition)
- **A running llama.cpp server** (or any OpenAI-compatible API endpoint)

## Install

```bash
git clone https://github.com/yourusername/openferris.git
cd openferris
cargo build --release
```

The binary is at `target/release/openferris`. Copy it somewhere on your `$PATH`:

```bash
cp target/release/openferris ~/.local/bin/
```

or symlink it:

```bash
ln -s ~/openferris/target/release/openferris ~/.local/bin/openferris
```

## Configure

Create the config file:

```bash
mkdir -p ~/.config/openferris
```

```bash
cat > ~/.config/openferris/config.toml << 'EOF'
[user]
timezone = "America/New_York"

[llm]
backend = "llamacpp"
endpoint = "http://localhost:8080"

[daemon]
listen = "127.0.0.1:7700"
EOF
```

Optionally, customize the agent's personality by placing a `SOUL.md` in `~/.config/openferris/`. A default is bundled into the binary.

## Quick Start

**1. Start the daemon:**

```bash
openferris daemon
```

**2. Chat interactively:**

```bash
openferris tui
```

**3. Run a skill:**

```bash
openferris run daily-briefing
```

**4. Schedule skills with cron:**

```bash
crontab -e
```

```crontab
0 7 * * *  openferris run daily-briefing
```

## Running as a systemd Service

Create a user service so the daemon starts automatically:

```bash
mkdir -p ~/.config/systemd/user
```

```bash
cat > ~/.config/systemd/user/openferris.service << 'EOF'
[Unit]
Description=OpenFerris AI Assistant Daemon
After=network.target

[Service]
ExecStart=%h/.local/bin/openferris daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now openferris
```

Check status with `systemctl --user status openferris`.

## Architecture

```
┌──────────┐ ┌──────────┐ ┌──────────┐
│ CLI      │ │ Cron     │ │   TUI    │    Thin clients
│ commands │ │ jobs     │ │          │    (send requests over TCP)
└────┬─────┘ └────┬─────┘ └────┬─────┘
     └────────────┼────────────┘
                  v
     ┌────────────────────────┐
     │    Central Daemon      │    The brain:
     │  (openferris daemon)   │    - Owns LLM session
     │                        │    - Queues requests
     │  Skills + Tools + LLM  │    - Runs agent loop
     └────────────────────────┘    - Processes one at a time
```

The daemon is the only process that talks to the LLM. All clients connect to it over TCP localhost (default `127.0.0.1:7700`), send a JSON-line request, and receive a JSON-line response.

## Context & Memory

The agent's system prompt is assembled fresh on every request from several layers:

1. **SOUL** — the agent's personality (`SOUL.md`). Loaded once at daemon startup. Customize by placing one in `~/.config/openferris/`.
2. **Memories** — long-term facts stored in `~/.local/share/openferris/MEMORIES.md`. Read fresh on every request, so new memories are immediately available. The agent saves memories automatically via `<memory>` tags, or you can save them directly in the TUI with `/remember <fact>`.
3. **Recent interactions** — the last 20 exchanges from SQLite (`~/.local/share/openferris/openferris.db`), giving the agent short-term conversational context across all interfaces.
4. **Skill prompt** — instructions from the active skill's `SKILL.md`.
5. **Tool descriptions** — filtered by the skill's tool allowlist.

Memories and interactions are shared across all interfaces (TUI, CLI, cron, future Telegram/etc.), so the agent stays coherent regardless of how you talk to it. The TUI also maintains per-session conversation history for multi-turn exchanges.

To clear history: `openferris forget [window]` (e.g. `1h`, `7d`, `all`).

## Skills

Skills are markdown files that tell the agent what to do. They follow the [AgentSkills](https://agentskills.io) format — a `SKILL.md` with YAML frontmatter:

```markdown
---
name: daily-briefing
description: Morning briefing with date, time, and a motivational note
tools:
  - datetime
---

Prepare a morning briefing for your human. Include:
1. Date and time
2. Day overview
3. Motivational note
```

The `tools` field is the **tool sieve** — it declares which tools this skill is allowed to use. The agent loop enforces this: if the LLM tries to call a tool not in the list, the call is rejected.

**Bundled skills:** `triage` (freeform conversation) and `daily-briefing`.

**Custom skills:** Place them in `~/.config/openferris/skills/<skill-name>/SKILL.md`. User skills override bundled ones with the same name.

## Tools

Tools are capabilities the agent can invoke. Each tool is a Rust module with a name, an LLM-facing description, and an `execute` function.

**Built-in tools:**

| Tool | Description |
|------|-------------|
| `datetime` | Returns current date/time in the user's configured timezone |

More tools (web search, weather, messaging) will be added as the project grows.

## Configuration Reference

```toml
# ~/.config/openferris/config.toml

[user]
timezone = "America/New_York"    # IANA timezone for the datetime tool
zip_code = "10001"               # For future weather tool

[llm]
backend = "llamacpp"             # LLM backend (currently: llamacpp)
endpoint = "http://localhost:8080"  # llama.cpp server URL
model = "my-model"               # Optional model name

[daemon]
listen = "127.0.0.1:7700"       # TCP address for the daemon
```

## Project Status

OpenFerris is in early development. The core architecture is functional:

- [x] Central daemon with TCP server and request queue
- [x] Agent loop with tool call parsing and execution
- [x] llama.cpp backend (OpenAI-compatible API)
- [x] Skill system with AgentSkills format
- [x] Tool system with per-skill sieve
- [x] CLI client and interactive TUI
- [x] Persistent memory (markdown) and interaction history (SQLite)
- [ ] Additional tools (web search, weather, messaging)
- [ ] Channel listeners (Telegram, Gmail)
- [ ] More LLM backends (Claude CLI, direct API)

## License

TBD
