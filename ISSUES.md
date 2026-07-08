# Issues

Originally from a full codebase review (2026-03-23). Re-triaged 2026-07-08 against
current code; most items were fixed by the April hardening passes and the July
refactor (see `.claude/REFACTOR_PLAN.md`). File references updated
(`llm/llamacpp.rs` is now `llm/openai_compat.rs`).

## Open

### P1-D: Workspace skill self-escalation
- **File:** `src/skills.rs`
- **Risk:** The agent can `write_file` a workspace SKILL.md with any tool list, then
  schedule it. `load_skill_from_str` accepts the frontmatter `tools` list verbatim —
  no policy limits which tools a workspace-created skill may claim.
- **Fix:** Enforce a maximum tool allowlist for workspace skills, or validate tool
  lists against a policy on load.

### P2-A: Unbounded worker channel
- `src/daemon.rs` — `mpsc::unbounded_channel::<QueuedRequest>()`; no backpressure if
  the LLM is slow while listeners keep enqueuing.

### P2-C: RunSkillTool hardcodes the backend
- `src/tools/run_skill.rs` — constructs `OpenAiCompatBackend` directly instead of
  going through an abstraction. Mitigated: it is currently the only backend.

### P3-B: Empty `allowed_senders` disables all outbound email authorization
- `src/email.rs` — an empty allowlist means "everyone is allowed" by design; it is
  documented in code but remains an insecure-by-default footgun.

### P3 minor hardening (unchanged)
- P3-F: `reqwest::Client` built per invocation. In `fetch_url` this is now partly
  deliberate (per-hop DNS pinning via `Client::resolve` needs a fresh client), but
  other tools could share one.
- P3-G: `memories.rs` lives in the binary, not the lib crate — untestable from
  integration tests.
- P3-H: `validate_path` recanonicalizes allowed dirs on every operation.
- P3-I: blocking `std::fs` I/O inside async tool implementations (`tools/files.rs`).
  Explicitly deferred — see "Explicitly not doing" in `.claude/REFACTOR_PLAN.md`.

## Resolved

- **P1-A (unauthenticated TCP daemon):** daemon now binds a Unix domain socket
  (`UnixListener` in `src/daemon.rs`) and publishes the path via a pointer file.
- **P1-B (cron injection):** `src/schedule.rs` has `validate_skill_name` /
  `validate_cron_expr` (strict charsets, 5-field check, forbidden-char rejection),
  with tests.
- **P1-C (email-reply prompt injection):** `skills/email-reply/SKILL.md` allowlist no
  longer includes `gws`/`ask_claude`; inbound bodies and thread history are wrapped
  in `UNTRUSTED EXTERNAL CONTENT` delimiters in `src/gmail.rs`.
- **P1-E (path traversal bypass):** `tools/files.rs` does lexical `..` normalization
  before the `starts_with` check, plus symlink recheck via canonicalized ancestors;
  the nonexistent-parent bypass case is regression-tested.
- **P1-F (daemon crash on accept error):** accept errors are logged and `continue`.
- **P1-G (crontab stdin `unwrap`):** replaced with `ok_or_else` + `Result`.
- **P1-H (`expect()` in backend constructor):** `OpenAiCompatBackend::new`
  (`src/llm/openai_compat.rs`) returns `Result`.
- **P2-B (SQLite writers without WAL):** resolved 2026-04-11 — `Storage::open` sets
  WAL + `busy_timeout=5000`; all writers go through it; concurrency regression test.
- **P2-D (SSRF via rebinding/redirects):** fixed in the 2026-07 refactor —
  `fetch_url` disables reqwest redirects, follows hops manually, revalidates
  scheme/DNS each hop, and pins the validated address via `Client::resolve`.
- **P2-E (hand-built JSON via `format!`):** send/compose paths use `serde_json` and
  sanitized headers. Residual `format!` JSON is confined to `gws --params` strings
  in `src/gmail.rs` interpolating Google-supplied IDs (message/thread/history IDs),
  not attacker-controlled text.
- **P2-F (no subprocess timeouts):** all `gws` calls go through the shared
  `src/gws_cli.rs` runner (timeout + `kill_on_drop`); crontab reads/writes have
  their own timeouts in `src/schedule.rs`.
- **P2-G (security controls untested):** now tested — SSRF IP classification,
  cc/recipient authorization, path traversal, cron injection, CRLF header injection.
- **P2-H (CLI silent exit on daemon-connect failure):** resolved 2026-04-11 — socket
  pointer fallback + terminal-failure row written to `interactions`.
- **P3-A (email header injection):** `sanitize_header` in `src/email.rs` collapses
  CR/LF in all interpolated header values, with tests.
- **P3-C (crafted display names):** `split_address_list` respects quoted display
  names; `normalize_cc` reduces cc entries to bare parsed addresses; tested.
- **P3-D (silent backend fallback):** unknown backend names now log a
  `tracing::warn!` before falling back (only one backend exists today).
- **P3-E (unbounded session history clones):** in-memory sessions were removed in
  the 2026-07 refactor (Track 1.1); threads load from SQLite through the
  `trim_history` budget.
- **P3-J (`parse_tool_calls` untested):** has a test suite in `src/agent.rs`,
  including the unclosed-`<tool_call>`/truncation case (now a `ParseError` that
  round-trips to the model).
- **Context management (older issue):** in-run compaction implemented
  (`MAX_COMPACTIONS`, hard bail when still over budget) plus per-tool result
  truncation via `truncate_for_context`.
