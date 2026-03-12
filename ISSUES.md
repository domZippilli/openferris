# Issues

## Subagent tool for skill delegation

The agent currently can't properly delegate to other skills. When asked to "run the headline scrape," it reads the SKILL.md and tries to follow it inline, which pollutes its context, burns its iteration budget, and often goes wrong (e.g. trying `gws run headline-scrape`).

### Proposal: `run_skill` tool

A tool that spawns a subagent with its own skill, system prompt, tool registry (with the skill's sieve applied), and llama.cpp slot. Returns the subagent's final response as a string.

**Parameters:** `{"skill_name": "<name>", "context": "<optional extra context>"}`

**Architecture:**
- The `run_skill` tool needs access to: skill loader, LLM backend, tool registry factory, soul/identity/user_profile
- Each subagent gets its own llama.cpp slot (`id_slot: 1`, `2`, etc.) so KV caches don't collide. The parent agent uses slot 0.
- llama.cpp must be configured with multiple slots (`-np 4` or similar). Currently may be running with 1 slot — needs config change.
- Subagent gets its own iteration budget (same MAX_ITERATIONS as parent)
- Depth limit: subagents cannot themselves spawn subagents (depth=1 max)
- The subagent's tool registry is built fresh with the skill's tool sieve applied
- Subagent does NOT get session history — it's a one-shot execution

**Open questions:**
- Should the `run_skill` tool be in the default skill's tool list only, or available to all skills?
- Should the parent see subagent tool calls in the trace (for debugging), or just the final result?
- Memory: if the subagent extracts `<memory>` tags, should those propagate to the parent?
- Concurrency: should the parent be able to launch multiple subagents in parallel? (Probably not in v1)
- What llama.cpp slot count to configure? 4 seems safe for 1 parent + a few subagents.

**llama.cpp slot configuration:**
- Start the server with `-np 4` (or `--parallel 4`) to enable 4 slots
- The `id_slot` field in the chat completion request pins to a specific slot
- Add `parallel_slots` to `LlmConfig` in config.toml (default 1 for backward compat):
  ```toml
  [llm]
  endpoint = "http://localhost:8080"
  parallel_slots = 4
  ```
- `LlamaCppBackend` takes a slot parameter; parent agent uses slot 0, subagents use 1..N
- `create_llm_backend()` in main.rs passes the slot; `run_skill` tool creates backends with different slots

---

## Agent context management for longer sessions

The agent loop appends every LLM response and tool result to the message list with no size management. For longer sessions this will overflow the LLM context window.

Two things to address:

1. **Raise iteration limit** — current cap of 20 is too low for multi-step tasks. Bump to ~50 or make configurable.

2. **Context compaction** — when the message list approaches the context limit:
   - Track token budget per message (estimate from char count or tokenizer).
   - When nearing the limit, ask the LLM to summarize the conversation so far into a single message, then continue with that summary.
   - Consider also capping individual tool results at a reasonable length (e.g. 8K chars) as a cheap first line of defense.
