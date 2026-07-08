# Skills Format

A skill is a directory containing a `SKILL.md` file. The file has YAML frontmatter followed by a markdown prompt.

## Structure

```
skills/
└── my-skill/
    └── SKILL.md
```

## SKILL.md Format

```markdown
---
name: my-skill
description: Short description of what this skill does
tools:
  - datetime
  - read_file
  - write_file
  - list_dir
---

Your prompt here. This is the instruction the agent follows when this
skill is invoked. Be specific about what you want the agent to do.
```

## Fields

- **name** (required): Identifier for the skill, matches the directory name.
- **description** (required): Human-readable summary.
- **tools** (optional): List of tools this skill can use. Only tools listed here will be available. If omitted, no tools are available.

## Available Tools

- `datetime` — Get current date and time in the user's timezone.
- `read_file` — Read a file. Params: `{"path": "..."}`
- `write_file` — Write a file. Params: `{"path": "...", "content": "..."}`
- `list_dir` — List directory contents. Params: `{"path": "..."}`
- `ocr_image` — Extract text from an image file in the workspace without loading image bytes into context. Params: `{"path": "...", "min_confidence": <optional>, "max_items": <optional>}`.
- `fetch_url` — Fetch a web page or API endpoint. Params: `{"url": "..."}`
- `web_search` — Search the web via SearXNG metasearch. Params: `{"query": "...", "categories": <optional, default: "general">}`. Returns a JSON array of `{title, url, snippet}`, capped at 15 results. Use for discovery before fetch_url/scrape_url.
- `scrape_url` — Scrape a web page via Firecrawl and return clean LLM-ready markdown. Params: `{"url": "..."}`. Handles JavaScript-rendered pages, removes nav/chrome/ads (truncated at 50KB). Use for general web pages where you want article content; for simple/known endpoints (RSS, JSON APIs, your wiki) use fetch_url instead — it's faster.
- `stealth_fetch` — Fetch a web page through Camoufox (stealth Firefox with anti-fingerprinting) and return clean markdown. Params: `{"url": "...", "wait_ms": <optional int, 0-15000>}`. Use only when fetch_url and scrape_url are blocked, rate-limited, or returning bot-detection pages; slow (~2-10s per call) and resource-heavy, so reach for it last in the fetch_url -> scrape_url -> stealth_fetch ladder.
- `schedule` — Manage cron-based skill schedules. Params: `{"action": "add|remove|list", "skill_name": "...", "cron_expr": "..."}`
- `send_telegram` — Send a message via Telegram. Params: `{"message": "...", "chat_id": <optional>}`
- `send_email` — Send an email via Gmail. Params: `{"to": "...", "subject": "...", "body": "..."}`. Recipient must be in allowed contacts or a known contact.
- `gws` — Run a Google Workspace CLI command. Params: `{"command": "..."}`. Destructive operations (delete, trash, send, empty, remove) are blocked by default. If `[gws].allow_drive_file_deletes = true`, `drive files delete` and `drive files trash` are allowed. Use send_email to send emails.
- `gws.drive.download_file` — Download a small uploaded image file from Google Drive as base64. Params: `{"file_id": "...", "max_bytes": <optional>, "mime_type_allowlist": <optional>}`. Supports JPEG, PNG, WebP, GIF, BMP, and TIFF up to 1 MB. Prefer `gws.drive.download_file_to_path` for normal images.
- `gws.drive.download_file_to_path` — Download an uploaded image file from Google Drive to a workspace path without returning file bytes. Params: `{"file_id": "...", "destination_path": "...", "max_bytes": <optional>, "mime_type_allowlist": <optional>}`. Supports JPEG, PNG, WebP, GIF, BMP, and TIFF up to 20 MB.
- `journal_logs` — View OpenFerris service logs from journalctl. Params: `{"lines": <optional number, default 50>, "unit": <optional string, default "openferris*">, "since": <optional string, e.g. "1h", "30m", "today">}`. Returns the most recent log lines for matching systemd units; use to check service health, debug errors, or review recent activity.
- `run_skill` — Run another skill as a subagent and return its result as text. Delivery tools are disabled inside the subagent, so `run_skill` never sends email, Telegram messages, or other external delivery by itself. The caller must explicitly use `send_email`, `send_telegram`, or another delivery tool after `run_skill` returns.
- `ask_claude` — Ask Claude Code for help. Params: `{"prompt": "..."}`
- `ask_codex` — Ask Codex for help. Params: `{"prompt": "..."}`

File tools are sandboxed to allowed directories only.

## Examples

### Simple skill (no tools)

```markdown
---
name: haiku
description: Write a haiku about the current season
tools: []
---

Write a haiku about the current season. Be creative and evocative.
```

### Skill with tools

```markdown
---
name: daily-briefing
description: Morning briefing with date, time, and a motivational note
tools:
  - datetime
---

Prepare a morning briefing for your human. Include:
1. Today's date and day of the week
2. A brief overview of the day
3. A motivational note to start the day
```

### Skill that writes files

```markdown
---
name: journal
description: Write a daily journal entry
tools:
  - datetime
  - write_file
  - read_file
---

Help the user write a daily journal entry. Use the datetime tool to get
today's date, then write the entry to workspace/journal/YYYY-MM-DD.md.
If a previous entry exists for today, append to it rather than overwriting.
```

## How Skills Are Invoked

- **By name:** `openferris run my-skill`
- **Bounded goal mode:** `openferris goal --max-turns 5 <exit criteria>` or `/goal --max-turns 5 <exit criteria>` in the TUI/Telegram
- **Via cron:** `0 7 * * * openferris run daily-briefing`
- **Freeform messages** use the `default` skill automatically.

## Skill Lookup Order

1. User skills: `~/.config/openferris/skills/<name>/SKILL.md`
2. Workspace skills: `~/.local/share/openferris/workspace/skills/<name>/SKILL.md`
3. Bundled skills (compiled into the binary)

## Goals

Goals (from `/goal` or `openferris goal`) persist as files at `~/.local/share/openferris/workspace/goals/<slug>.md`, not just as an in-flight request. The file is the source of truth; the `goal-pursuit` and `goal-runner` skills read and rewrite it directly with `read_file`/`write_file` — there's no separate database record or parser to keep in sync.

### File format

```markdown
---
status: active | done | abandoned
created: <date>
next_check: <YYYY-MM-DD HH:MM or "none">
---
# Goal: <one-line goal>
## Exit criteria
...
## Plan
...
## Progress log
- <date>: what happened, what was learned, what's next
```

### Lifecycle

- **active** — still being worked. Paired with `next_check`, which controls when it's next looked at:
  - a future timestamp: come back and work it once that time has passed.
  - `none`: paused mid-run only — never leave a goal this way at the end of a run, or it's skipped forever.
- **done** — exit criteria satisfied.
- **abandoned** — no further action is possible or useful; the progress log says why.

### goal-runner cadence

`goal-pursuit` handles interactive `/goal` runs (bounded by a turn limit, in real time). `goal-runner` is the unattended heartbeat: on a cron cadence, it lists `goals/`, and for every `active` goal whose `next_check` is due, it picks up where the file left off, does the work, and rewrites the file again — messaging the owner only when something is actually worth reporting. Schedule it once with:

```
openferris schedule add goal-runner "0 */2 * * *"
```

(every 2 hours; adjust the cron expression to taste). This is what makes "I'll check back tomorrow" a real mechanism instead of an empty promise.
