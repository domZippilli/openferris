# Streaming responses — implementation plan

## Goal

Stream assistant text from llama.cpp through the agent and out to clients
(Telegram in particular) so users see progress while a long generation is
in flight, and so the reqwest "connection closed before request completed"
class of error becomes self-healing (steady byte flow + per-chunk handling).

## Locked decisions (2026-05-09)

1. **Trait shape**: callback-style. `chat_completion_stream(&self, msgs, on_chunk: FnMut(&str))`
   — avoids `async_trait`+`Stream` lifetime pain, simple for every consumer.
2. **Tool-call parsing**: buffer-then-parse for tool calls, pass-through stream
   for prose. Text outside `<tool_call>...</tool_call>` forwards as it arrives;
   text inside is suppressed from clients but accumulated for the parser at
   end-of-stream. No mid-stream early-stop in v1.
3. **Daemon→client wire**: extend the existing notification channel with a new
   variant for streamed chunks. Same socket, same flow as tool-call progress.

## Phase 0 — foundations (sequential, owner: main agent)

Locks the contracts so subagents can fan out without colliding.

- [ ] Add `chat_completion_stream(&self, msgs, on_chunk)` to `LlmBackend` with a
  default impl that calls `chat_completion` and emits the whole content as one
  chunk — keeps existing backends compiling unchanged.
- [ ] Add `AgentNotification { ToolProgress(String), AssistantChunk(String) }`
  in `src/protocol.rs`; change the daemon→agent progress channel from
  `mpsc::UnboundedSender<String>` to `mpsc::UnboundedSender<AgentNotification>`.
- [ ] Add `ResponseKind::AssistantChunk { text }` to the wire protocol.
- [ ] Update senders/receivers (agent.rs, daemon.rs, main.rs, telegram.rs) to
  route the new enum. Telegram and CLI keep current behavior for v1 (only
  `ToolProgress` shown).
- [ ] Build clean, all integration tests pass, daemon restarts on new binary.

## Phase 1 — parallel implementation (subagents)

Each subagent gets a self-contained brief. No inter-dependencies once Phase 0
contracts are merged.

| Agent | Brief | Files | Type |
|---|---|---|---|
| **A** | Implement real SSE streaming for llama.cpp: `"stream": true`, parse `data: {...}` lines, extract `choices[0].delta.content`, invoke callback per chunk; handle `[DONE]`, errors, `finish_reason`. | `src/llm/llamacpp.rs` | general-purpose |
| **B** | MockLlm streaming + tests: chunk scripted responses (e.g. word-at-a-time); update existing tests; add a streaming-specific test. | `src/llm/mock.rs`, `tests/agent_integration.rs` | general-purpose |
| **C** | Agent loop wiring: switch `Agent::run` to call streaming variant, accumulate into final message, fire `AssistantChunk` notifications per chunk, suppress inside `<tool_call>...</tool_call>` ranges. Existing parse-on-complete logic unchanged. | `src/agent.rs` | Plan (high blast radius) |
| **D** | Telegram edit-debouncer: handle `AssistantChunk` notifications by sending one initial message, then `editMessageText` at most every ~1.5s; finalize on stream end; respect 4096-char limit. | `src/telegram.rs` | general-purpose |

## Phase 2 — integration & polish (sequential, owner: main agent)

- End-to-end run: real llama-server, `test-agent` CLI prints incrementally,
  Telegram bot edits a live message.
- Tune Telegram debounce.
- Stretch goal: incremental tool-call detection + early-stop.

## Status

- 2026-05-09: decisions locked; Phase 0 in progress.
