# Issues

## Agent context management for longer sessions

The agent loop appends every LLM response and tool result to the message list with no size management. For longer sessions this will overflow the LLM context window.

Two things to address:

1. **Raise iteration limit** — current cap of 20 is too low for multi-step tasks. Bump to ~50 or make configurable.

2. **Context compaction** — when the message list approaches the context limit:
   - Track token budget per message (estimate from char count or tokenizer).
   - When nearing the limit, ask the LLM to summarize the conversation so far into a single message, then continue with that summary.
   - Consider also capping individual tool results at a reasonable length (e.g. 8K chars) as a cheap first line of defense.
