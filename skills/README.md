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
- `fetch_url` — Fetch a web page or API endpoint. Params: `{"url": "..."}`
- `schedule` — Manage cron-based skill schedules. Params: `{"action": "add|remove|list", "skill_name": "...", "cron_expr": "..."}`
- `send_telegram` — Send a message via Telegram. Params: `{"message": "...", "chat_id": <optional>}`
- `gws` — Run a Google Workspace CLI command. Params: `{"command": "..."}`. Destructive operations (delete, trash, send) are blocked.

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
- **Via cron:** `0 7 * * * openferris run daily-briefing`
- **Freeform messages** use the `default` skill automatically.

## Skill Lookup Order

1. User skills: `~/.config/openferris/skills/<name>/SKILL.md`
2. Workspace skills: `~/.local/share/openferris/workspace/skills/<name>/SKILL.md`
3. Bundled skills (compiled into the binary)
