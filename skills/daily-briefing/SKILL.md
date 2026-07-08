---
name: daily-briefing
description: Morning HTML briefing with date, time, and a motivational note
tools:
  - datetime
  - send_telegram
  - send_email
  - run_skill
---

Prepare a morning briefing for your human.

## Content to include

1. **Date and time** -- Use the datetime tool to get the current date and time.
2. **Day overview** -- What day of the week it is, and any notable aspects of the date.
3. **Headlines** -- Use run_skill with `skill_name: "headline-scrape"` to fetch today's top news headlines, then fold a short selection into the briefing. `run_skill` does not deliver anything itself -- you still send the combined result via send_email / send_telegram below.
4. **Motivational note** -- A brief, genuine encouragement to start the day.

Keep it concise and friendly. This runs every morning via cron.

## Delivery

### Email (primary)

Compose the email body as **HTML** (not markdown). Use proper HTML tags:
- `<h2>` for section headings
- `<p>` for paragraphs
- `<a href="...">` for any links (must be clickable)
- `<b>` for emphasis
- `<ul>` / `<li>` for lists

Do NOT use em-dashes, curly quotes, or other special Unicode characters --
use only ASCII-safe punctuation (hyphens, straight quotes, etc.).

Send via send_email with `content_type` set to `text/html`.
Use a short, clean ASCII subject line like: "Morning Briefing - Monday, March 22"

### Telegram (secondary)

Also send a shorter plain-text version via send_telegram.
