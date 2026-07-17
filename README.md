# OpenFerris

A reliable AI personal assistant that runs on Linux. Built with Unix philosophy: simple components, composed together, scheduled with cron. The guiding principle is reliability over features — do specific things, on time, every time, using markdown files, SQLite, and crontab entries instead of orchestration frameworks.

OpenFerris uses a central daemon that owns the LLM session. Everything else — CLI commands, cron jobs, the TUI, the web chat, and the Gmail listener — are thin clients that send requests to the daemon over a Unix domain socket.

## Prerequisites

- **Rust 1.93+** (2024 edition)
- **An OpenAI-compatible chat API endpoint** (for example vLLM or llama.cpp)

Some tools are optional and only work if their backing service/CLI is installed — the agent still runs fine without them, it just won't have that capability:

- `ocr_image` needs [uv](https://github.com/astral-sh/uv) (it shells out to `uv run` to fetch `rapidocr-onnxruntime` on demand)
- `gws` and the Drive download tools need the [`gws`](https://www.npmjs.com/package/@googleworkspace/cli) CLI (`npm install -g @googleworkspace/cli`) authenticated for your Google account
- `journal_logs` needs `journalctl` (i.e. running under systemd, which the services below assume)
- `ask_claude` / `ask_codex` need the `claude` / `codex` CLIs on `$PATH`
- `web_search` / `scrape_url` / `stealth_fetch` need a SearXNG / Firecrawl / Camoufox endpoint respectively — see [Configuration Reference](#configuration-reference)

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
backend = "openai_compat"
endpoint = "http://localhost:8080"
temperature = 0.6
top_k = 20
enable_thinking = true
EOF
```

```bash
chmod 600 ~/.config/openferris/config.toml
```

Keep `config.toml` mode `600`, especially when it contains service credentials.

Optionally, customize the agent's personality by placing a `SOUL.md` in `~/.config/openferris/`. A default is bundled into the binary. See [System prompt layers](#system-prompt-layers) for the other override files.

## Quick Start

**1. Start the daemon:**

```bash
openferris daemon
```

**2. Chat interactively:**

```bash
openferris tui
```

Or use the private web interface from your tailnet:

```bash
openferris web
tailscale serve --bg http://127.0.0.1:3030
```

Open the HTTPS URL printed by `tailscale serve`. The web service listens only on loopback by default; Tailscale provides tailnet access and HTTPS.

**3. Run a skill:**

```bash
openferris run daily-briefing
```

**4. Schedule skills with cron:**

```bash
openferris schedule add daily-briefing "0 7 * * *"
```

(`openferris schedule` writes/removes marked lines in your user crontab — see [CLI](#cli) below. You can also edit `crontab -e` directly if you prefer.)

## Running as systemd Services

Production runs the daemon, web chat, and Gmail listener as separate user services grouped under one target, so each restarts independently on failure:

```bash
mkdir -p ~/.config/systemd/user
```

```bash
cat > ~/.config/systemd/user/openferris-daemon.service << 'EOF'
[Unit]
Description=OpenFerris daemon
PartOf=openferris.target

[Service]
Type=simple
ExecStart=%h/.local/bin/openferris daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=openferris.target
EOF
```

```bash
cat > ~/.config/systemd/user/openferris-web.service << 'EOF'
[Unit]
Description=OpenFerris private web chat
PartOf=openferris.target
After=openferris-daemon.service

[Service]
Type=simple
ExecStart=%h/.local/bin/openferris web
Restart=on-failure
RestartSec=5

[Install]
WantedBy=openferris.target
EOF
```

```bash
cat > ~/.config/systemd/user/openferris-gmail.service << 'EOF'
[Unit]
Description=OpenFerris Gmail integration
PartOf=openferris.target

[Service]
Type=simple
ExecStart=%h/.local/bin/openferris gmail
Restart=on-failure
RestartSec=5

[Install]
WantedBy=openferris.target
EOF
```

```bash
cat > ~/.config/systemd/user/openferris.target << 'EOF'
[Unit]
Description=OpenFerris services
Wants=openferris-daemon.service openferris-web.service openferris-gmail.service

[Install]
WantedBy=default.target
EOF
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now openferris.target
```

The Gmail unit is optional and requires `[gmail]`. Check status with `systemctl --user status openferris-daemon` (or `-web`/`-gmail`). Expose the loopback web service once with `tailscale serve --bg http://127.0.0.1:3030`.

For quick manual testing without systemd, `run.sh` at the repo root builds and runs all three foreground processes together; it's a dev convenience, not something to rely on for a real deployment (no restart-on-failure, one Ctrl-C kills everything).

## Architecture

```
┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
│ CLI      │ │ Cron     │ │   TUI    │ │ Web/Gmail│   Thin clients
│ commands │ │ jobs     │ │          │ │          │   (send requests over
└────┬─────┘ └────┬─────┘ └────┬─────┘ └────┬─────┘   a Unix socket)
     └────────────┴─────────────┴────────────┘
                        v
     ┌────────────────────────────────┐
     │        Central Daemon          │    The brain:
     │      (openferris daemon)       │    - Owns the LLM session
     │                                │    - Queues requests, one at a time
     │   Skills + Tools + LLM + DB    │    - Runs the agent loop
     └────────────────────────────────┘    - Persists threads/goals/wakeups
```

The daemon is the only process that talks to the LLM. All clients connect to it over a **Unix domain socket** (not TCP), send a JSON-line request, and receive one or more JSON-line responses (progress notifications, then a final result).

The web and Gmail services are deliberately separate processes: each bridges exactly one channel, so adding an interface does not require modifying the daemon.

The socket path is `$XDG_RUNTIME_DIR/openferris.sock` by default (falls back to `~/.local/share/openferris/openferris.sock` if `$XDG_RUNTIME_DIR` is unset — e.g. under cron). Override it with `[daemon].socket` in config. The daemon also writes the socket path it actually bound to `~/.local/share/openferris/daemon.socket.path`; clients that can't reach the configured/default path (notably cron, which lacks `$XDG_RUNTIME_DIR`) fall back to reading that file. The socket file itself is created mode `0600`.

## How memory and autonomy work

Every counterparty the agent talks to — you, or anyone else it emails — gets one continuous **message thread** stored in SQLite (`~/.local/share/openferris/openferris.db`, `messages` table), independent of which channel the conversation happens over. Your web chat and TUI session share the `owner` thread; a stranger who emails the agent gets their own `email:<address>` thread. History survives daemon restarts, and outbound email is recorded in its thread too.

Longer-running work is tracked as **goal files** at `workspace/goals/<slug>.md` rather than living only in a request string. A goal file carries its status (`active`/`done`/`abandoned`), a plan, a progress log, and a `next_check` timestamp. `openferris goal <exit criteria>` (or `/goal` in the TUI/web chat) runs the `goal-pursuit` skill interactively for a bounded number of turns; the bundled `goal-runner` skill is the unattended heartbeat that picks up any goal whose `next_check` has passed. Schedule it once with:

```bash
openferris schedule add goal-runner "0 */2 * * *"
```

For a one-off, precise-time follow-up that isn't a whole goal ("remind me at 9", "check tomorrow whether they replied"), any skill can call the `set_wakeup` tool instead. The daemon polls for due wakeups roughly once a minute and fires a fresh `default`-skill run with the wakeup's note as its only instruction.

Because a model can talk itself into believing it finished something it didn't, a `<goal_status>done</goal_status>` claim during goal pursuit isn't taken at face value: the daemon makes a separate, tool-free LLM call with a strict-verifier prompt to check the claim against the exit criteria before ending the run. A rejection feeds its reason back into the next turn instead of silently accepting "done."

## System prompt layers

The agent's system prompt is assembled fresh on every request, in this order:

1. **SOUL** (`~/.config/openferris/SOUL.md`) — the agent's personality and identity (name, self-concept, style). Loaded once at daemon startup. Falls back to the bundled default if the file doesn't exist.
2. **USER** (`~/.local/share/openferris/USER.md`) — facts about you. Re-read on every request, so edits take effect without a restart. Falls back to the bundled default.
3. **Persistent context** — long-term memories (`~/.local/share/openferris/MEMORIES.md`, saved via `<memory>` tags or `/remember` in the TUI) plus a recent-interactions annex from SQLite, both read fresh on every request.
4. **Skill prompt** — the active skill's `SKILL.md` body.
5. **Tool descriptions** — filtered by the skill's `tools:` allowlist.

Note that SOUL lives under the **config** directory (`~/.config/openferris/`) while USER lives under the **data** directory (`~/.local/share/openferris/`). On top of all this, a freeform message also gets the resolved counterparty's message thread (see above) as conversation history.

## Skills

Skills are markdown files that tell the agent what to do. They follow a `SKILL.md` format with YAML frontmatter (name, description, a `tools:` allowlist) and a markdown prompt body. Full format details, examples, and the goal-file spec are in [`skills/README.md`](skills/README.md).

**Bundled skills:** `default` (freeform conversation), `daily-briefing`, `email-reply`, `goal-pursuit`, `goal-runner`.

There is no global output router: a skill that needs to deliver its result calls a delivery tool such as `send_email` explicitly as part of its instructions.

The `tools` field is a **focus mechanism, not a security boundary** — since the agent can create its own skills in the workspace, it can give itself access to any registered tool. The real security boundary is the tool registry: only tools compiled into the binary exist at all, and destructive operations within those tools (file writes outside allowed directories, `gws` deletes, unauthorized email recipients) are enforced by the tools themselves regardless of which skill invokes them.

Skill lookup order:

1. **User skills:** `~/.config/openferris/skills/<name>/SKILL.md` — you always win
2. **Workspace skills:** `~/.local/share/openferris/workspace/skills/<name>/SKILL.md` — agent-created
3. **Bundled skills** — compiled into the binary as starters

## CLI

| Command | Description |
|---|---|
| `openferris daemon` | Start the central daemon (owns the LLM session, binds the Unix socket) |
| `openferris tui` | Interactive terminal chat session with the daemon |
| `openferris run <skill>` | Run a named skill once (e.g. from cron) |
| `openferris goal [--max-turns N] <exit criteria>` | Pursue a goal over multiple bounded inference turns |
| `openferris web [--listen 127.0.0.1:3030]` | Start the private web chat service |
| `openferris gmail` | Start the Gmail listener (requires `[gmail]` in config) |
| `openferris schedule add <skill> <cron_expr>` | Add a cron entry that runs a skill on a schedule |
| `openferris schedule remove <skill>` | Remove a scheduled skill |
| `openferris schedule list` | List scheduled skills |
| `openferris test-agent [--skill <name>] <prompt>` | Run a prompt through the real agent standalone (no daemon) with a full debug trace to stderr |
| `openferris forget [window] [-y]` | Delete interaction history and/or memories in a time window (`1h`, `7d`, `30d`, `all`); prompts for confirmation unless `-y` |

## Tools

Tools are capabilities the agent can invoke. Each tool is a Rust module with a name, an LLM-facing description, and an `execute` function; the skill's `tools:` list decides which ones are visible for a given run. Some are always registered, some only when their config section is present. Invocation uses structured `<tool_call>` markers parsed out of ordinary completions rather than a native function-calling API — a format chosen to be reliably producible by local models.

**Always registered:**

| Tool | Purpose |
|---|---|
| `datetime` | Current date/time in the user's configured timezone |
| `read_file` | Read a file (sandboxed to allowed directories) |
| `write_file` | Write/create a file, including parent directories (sandboxed) |
| `list_dir` | List a directory's contents (sandboxed) |
| `ocr_image` | Run OCR on a workspace image file without loading image bytes into context (via `uv run rapidocr-onnxruntime`) |
| `fetch_url` | Fetch a web page/API as text; blocks internal/loopback addresses (SSRF-guarded, including redirects) unless the port is allowlisted |
| `schedule` | Add/remove/list cron-scheduled skill invocations |
| `gws` | Run a Google Workspace CLI (`gws`) command against Drive/Gmail/Calendar/Sheets/Docs/etc.; destructive verbs (delete/trash/send/empty/remove) are blocked by default |
| `gws.drive.download_file` | Download a small Drive image file and return it as base64 (≤1 MB) |
| `gws.drive.download_file_to_path` | Download a Drive image file straight to a workspace path without returning bytes (≤20 MB) |
| `journal_logs` | Read recent `journalctl --user` output for the OpenFerris services |
| `set_wakeup` | Schedule a one-shot future agent run (add/list/cancel) |
| `ask_claude` | Ask Claude Code for help; resumes the same conversation across calls within one run |
| `ask_codex` | Ask Codex for help; resumes the same thread across calls within one run |

**Registered only when the matching config section exists:**

| Tool | Config section | Purpose |
|---|---|---|
| `web_search` | `[search]` | Search the web via a SearXNG-compatible endpoint |
| `scrape_url` | `[firecrawl]` | Scrape a page via Firecrawl, returning clean markdown |
| `stealth_fetch` | `[camoufox]` | Fetch via Camoufox (stealth Firefox) for bot-detection-heavy sites — last resort in the `fetch_url` → `scrape_url` → `stealth_fetch` ladder |
| `send_email` | `[gmail]` | Send an email via Gmail; recipient (and any `cc`) must be in `allowed_senders` or a previously-emailed contact |

**Registered only when `[llm].parallel_slots > 1`:**

| Tool | Purpose |
|---|---|
| `run_skill` | Run another skill as a subagent (its own slot, own context) and return the result as text. Delivery tools such as `send_email` are stripped from the subagent — the caller must deliver the result itself. |

File tools (`read_file`, `write_file`, `list_dir`, `ocr_image`, `gws.drive.download_file_to_path`) are restricted to `~/.local/share/openferris/workspace/` plus any directories added in config:

```toml
[files]
allowed_directories = ["~/notes", "~/documents"]
```

## Configuration Reference

```toml
# ~/.config/openferris/config.toml

[user]
timezone = "America/New_York"    # IANA timezone for datetime, set_wakeup, etc.
emails = ["me@example.com"]       # Optional: your address(es), used to route inbound/outbound
                                   # email into the shared "owner" thread instead of a per-address one

[llm]
backend = "openai_compat"        # LLM backend (openai_compat/openai-compatible/llamacpp all map to the same backend)
endpoint = "http://localhost:8080"  # OpenAI-compatible server URL
model = "my-model"                # Optional model name
temperature = 0.6                 # Sampling temperature
top_k = 20                        # Sampling top-k
enable_thinking = true            # Pass enable_thinking through chat_template_kwargs (vLLM/Gemma-style reasoning models)
parallel_slots = 1                # >1 enables the run_skill subagent tool (parent uses slot 0, subagents use 1+)

# Optional — omit the whole [daemon] table to use the default socket path.
[daemon]
socket = "/run/user/1000/openferris.sock"  # Unix socket path; defaults to $XDG_RUNTIME_DIR/openferris.sock

# Optional.
[files]
allowed_directories = ["~/notes"] # Extra dirs the file tools may read/write, beyond the workspace

# Optional. Local ports fetch_url may reach despite the SSRF block (e.g. a local wiki).
[fetch]
allowed_local_ports = [8088]

# Optional — omit to leave all gws destructive Drive operations blocked.
[gws]
allow_drive_file_deletes = false  # Set true to allow `drive files delete`/`drive files trash`

# Optional — enables the web_search tool.
[search]
endpoint = "http://127.0.0.1:8888"  # SearXNG (or compatible) JSON search endpoint

# Optional — enables the scrape_url tool.
[firecrawl]
endpoint = "http://127.0.0.1:3002"  # Firecrawl API base

# Optional — enables the stealth_fetch tool.
[camoufox]
endpoint = "http://127.0.0.1:8765"  # Camoufox stealth-fetch API base

# Optional — enables `openferris gmail` and the send_email tool.
[gmail]
allowed_senders = ["me@example.com"]  # Addresses allowed to trigger auto-replies / be emailed
poll_interval_secs = 60               # How often to poll for new mail (default 60)
rate_limit_secs = 300                 # Minimum seconds between replies to the same thread (default 300)
always_cc = "archive@example.com"     # Optional address always CC'd on outbound mail
```

`OPENFERRIS_LLM_TEMPERATURE` and `OPENFERRIS_LLM_TOP_K` override the configured sampling values for quick experiments.

## Current Features

- Central daemon over a Unix domain socket, serializing all LLM access through one agent loop
- Streaming responses with live progress notifications to clients (TUI, web)
- Per-counterparty SQLite message threads shared across every channel, surviving restarts
- Persistent goal files with an unattended cron heartbeat (`goal-runner`) and an independent done-claim evaluator
- One-shot scheduled wakeups (`set_wakeup`) on a ~60s daemon tick
- Skill system (AgentSkills-style `SKILL.md`) with per-skill tool allowlists, user/workspace/bundled lookup layers, and subagent delegation (`run_skill`) when `parallel_slots > 1`
- Tailnet-ready web chat and a Gmail listener with allowlisting, rate limiting, and thread-aware replies
- Web tooling ladder: `fetch_url` (SSRF-guarded) → `scrape_url` (Firecrawl) → `stealth_fetch` (Camoufox), plus `web_search` (SearXNG)
- Google Workspace integration via `gws` (Drive/Gmail/Calendar/etc.) with destructive operations blocked by default
- OCR, ask_claude/ask_codex subagent tools, and journalctl log access for self-diagnosis
- Automatic context compaction when a run approaches the model's context window

## License

TBD
