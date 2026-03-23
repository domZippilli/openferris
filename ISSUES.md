# Issues

## P1 — Security: Merge Blockers

From full codebase review (2026-03-23). These form a compound attack chain:
crafted email → prompt injection → LLM writes malicious SKILL.md → cron persistence → autonomous execution.

### P1-A: Unauthenticated TCP daemon
- **File:** `daemon.rs:126`
- **Risk:** Any local process can issue commands to the daemon
- **Fix:** Switch to Unix domain socket with `chmod 600`, or add shared-secret handshake

### P1-B: Cron injection via unsanitized inputs
- **File:** `schedule.rs:60-68`
- **Risk:** `skill_name`/`cron_expr` written to crontab unsanitized → arbitrary code execution
- **Fix:** Strict regex validation — alphanumeric + hyphens for skill_name, validated fields for cron_expr

### P1-C: Prompt injection in email-reply [HIGHEST PRIORITY — externally exploitable]
- **File:** `gmail.rs:296`, `email-reply/SKILL.md`
- **Risk:** Raw email body reaches LLM with `gws`/`ask_claude` tools available → data exfiltration
- **Fix:** Remove `gws`/`ask_claude` from email-reply tool allowlist; add `[UNTRUSTED CONTENT BELOW]` delimiter

### P1-D: Workspace skill self-escalation
- **File:** `skills.rs:71-98`
- **Risk:** Agent can write_file a SKILL.md with any tool list, then schedule it
- **Fix:** Enforce maximum tool allowlist for workspace-created skills, or validate tool lists on load

### P1-E: Path traversal bypass
- **File:** `tools/files.rs:29-38`
- **Risk:** Non-existent parent causes canonicalize to fail, `starts_with` check is bypassed
- **Fix:** Manual `..` component normalization before `starts_with` check

### P1-F: Daemon crash on accept error
- **File:** `daemon.rs:126`
- **Risk:** `EMFILE`/`ECONNABORTED` propagates to main → daemon exits
- **Fix:** Catch accept errors, log, `continue`

### P1-G: `unwrap()` on crontab stdin
- **File:** `schedule.rs:29-31`
- **Risk:** Panic if pipe creation fails → daemon worker crash
- **Fix:** Replace with `Result` propagation

### P1-H: `expect()` in LlamaCppBackend::new
- **File:** `llm/llamacpp.rs:54`
- **Risk:** Panic on TLS init failure → daemon worker crash via subagent
- **Fix:** Return `Result` from constructor

---

## P2 — Reliability: Fix Before Enabling Integrations

### P2-A: Unbounded worker channel
- `daemon.rs:36` — no backpressure on slow LLM → memory exhaustion

### P2-B: Dual SQLite writers without WAL mode
- `gmail.rs:81`, `storage.rs` — `SQLITE_BUSY`, lost contact records

### P2-C: RunSkillTool hardcodes LlamaCppBackend
- `run_skill.rs:78` — backend abstraction broken

### P2-D: SSRF bypass via DNS rebinding and redirect chains
- `tools/web.rs:69-95` — internal network access

### P2-E: JSON construction via `format!` in Gmail poller
- `gmail.rs:222,149` — JSON injection, malformed API calls

### P2-F: No timeout on subprocess calls (gws, claude, crontab)
- Multiple files — daemon worker blocks indefinitely

### P2-G: Security controls untested
- SSRF protection, email authorization — implemented but unverified

---

## P3 — Hardening (Next Development Cycle)

- P3-A: Email header injection (CRLF in subject/to)
- P3-B: Empty `allowed_senders` disables all outbound authorization
- P3-C: Email address parsing susceptible to crafted display names
- P3-D: Unknown LLM backend silently falls back to llamacpp
- P3-E: Session history cloned without bound
- P3-F: `reqwest::Client` recreated per tool invocation
- P3-G: `memories.rs` not in library crate
- P3-H: `validate_path` recanonicalized on every operation
- P3-I: Blocking I/O in async contexts
- P3-J: `parse_tool_calls` edge cases untested

---

## Older Issues

### Agent context management for longer sessions

The agent loop appends every LLM response and tool result to the message list with no size management. For longer sessions this will overflow the LLM context window.

1. **Context compaction** — when nearing the limit, summarize conversation so far
2. **Tool result capping** — limit individual tool results to ~8K chars
