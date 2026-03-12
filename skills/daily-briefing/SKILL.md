---
name: daily-briefing
description: Morning briefing with date, time, and a motivational note
tools:
  - datetime
  - send_telegram
  - send_email
---

Prepare a morning briefing for your human. Include:

1. **Date and time** — Use the datetime tool to get the current date and time.
2. **Day overview** — What day of the week it is, and any notable aspects of the date.
3. **Motivational note** — A brief, genuine encouragement to start the day.

Keep it concise and friendly. This runs every morning via cron.
After composing the briefing, deliver it via send_telegram.
