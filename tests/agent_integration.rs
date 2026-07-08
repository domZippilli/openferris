use openferris::agent::Agent;
use openferris::llm::mock::MockLlm;
use openferris::protocol::AgentNotification;
use openferris::skills::Skill;
use openferris::tools::ToolRegistry;
use openferris::tools::datetime::DateTimeTool;

fn test_skill(tool_names: &[&str]) -> Skill {
    Skill {
        name: "test".into(),
        description: "test skill".into(),
        tools: tool_names.iter().map(|s| s.to_string()).collect(),
        prompt: "You are a test assistant.".into(),
    }
}

fn test_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(DateTimeTool::new("UTC".into())));
    reg
}

/// Direct answer with no tool calls.
#[tokio::test]
async fn test_direct_answer() {
    let mock = MockLlm::new(vec!["Hello! How can I help you?".into()]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let result = agent
        .run(&skill, "Hi", &[], "", "", "", None)
        .await
        .unwrap();

    assert_eq!(result.response, "Hello! How can I help you?");
    assert!(result.memories.is_empty());
}

/// Agent calls datetime tool, gets real result, then gives final answer.
#[tokio::test]
async fn test_single_tool_call() {
    let mock = MockLlm::new(vec![
        // First response: a tool call
        r#"Let me check the time.

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#
            .into(),
        // Second response: final answer using the tool result
        "The current time is Thursday afternoon.".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let result = agent
        .run(&skill, "What time is it?", &[], "", "", "", None)
        .await
        .unwrap();

    assert_eq!(result.response, "The current time is Thursday afternoon.");
}

/// Agent tries to call a tool not in the skill's allowlist.
/// The tool sieve should block it and return an error to the agent.
#[tokio::test]
async fn test_tool_sieve_blocks_disallowed() {
    let mock = MockLlm::new(vec![
        // Agent tries to use fetch_url which is not in the skill's tool list
        r#"<tool_call>
{"function": "fetch_url", "parameters": {"url": "https://example.com"}}
</tool_call>"#
            .into(),
        // After getting the error, agent gives a final answer
        "Sorry, I couldn't fetch that URL.".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]); // only datetime allowed

    let result = agent
        .run(&skill, "Fetch example.com", &[], "", "", "", None)
        .await
        .unwrap();

    assert_eq!(result.response, "Sorry, I couldn't fetch that URL.");
}

/// Memory tags are extracted and stripped from the response.
#[tokio::test]
async fn test_memory_extraction() {
    let mock = MockLlm::new(vec![
        "Got it, I'll remember that.\n\n<memory>User prefers dark mode</memory>".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&[]);

    let result = agent
        .run(&skill, "I prefer dark mode", &[], "", "", "", None)
        .await
        .unwrap();

    assert_eq!(result.memories, vec!["User prefers dark mode"]);
    assert!(!result.response.contains("<memory>"));
    assert!(result.response.contains("Got it"));
}

/// Multiple tool calls in a single response are all executed.
#[tokio::test]
async fn test_multiple_tool_calls_in_one_response() {
    let mock = MockLlm::new(vec![
        // Two tool calls in one response
        r#"Let me check twice.

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>

<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#
            .into(),
        // Final answer
        "I checked the time twice and it's consistent.".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let result = agent
        .run(&skill, "Double check the time", &[], "", "", "", None)
        .await
        .unwrap();

    assert!(result.response.contains("consistent"));
}

/// Agent exceeds the iteration limit and returns an error.
#[tokio::test]
async fn test_max_iterations_exceeded() {
    // 51 responses, all tool calls — never a final answer
    let responses: Vec<String> = (0..51)
        .map(|_| {
            r#"<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#
                .into()
        })
        .collect();

    let mock = MockLlm::new(responses);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let err = agent
        .run(&skill, "Loop forever", &[], "", "", "", None)
        .await
        .unwrap_err();

    assert!(
        format!("{}", err).contains("maximum iterations"),
        "Expected max iterations error, got: {}",
        err
    );
}

// Previously there was a `test_tool_call_tags_stripped_from_response` test
// here that relied on the agent silently swallowing a malformed tool_call
// (no `function` field) and then returning the response with stripped tags.
// Parse failures now round-trip back to the model as a `parse_error` result,
// so that scenario no longer reaches the final-answer path. `strip_tags` is
// directly covered by a unit test in `agent.rs::tests::test_strip_tags`.

/// Streaming forwards prose around a tool_call but suppresses the markup
/// itself. The first scripted response interleaves prose with a `<tool_call>`
/// block; clients should see the prose chunks but never the tool_call tags
/// or the JSON inside them. Works whether MockLlm streams in many chunks or
/// emits the response as a single chunk — the suppression logic is
/// chunking-agnostic.
#[tokio::test]
async fn test_assistant_chunks_stream_around_tool_calls() {
    let mock = MockLlm::new(vec![
        // Turn 1: prose, tool_call, more prose. Trailing space-after-prose so
        // MockLlm's word-splitter actually produces multiple chunks across
        // the tool_call boundary.
        r#"Here is the time: <tool_call>{"function": "datetime", "parameters": {}}</tool_call> done."#
            .into(),
        // Turn 2: final answer.
        "It is currently afternoon.".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentNotification>();
    let result = agent
        .run(&skill, "What time is it?", &[], "", "", "", Some(tx))
        .await
        .unwrap();
    assert_eq!(result.response, "It is currently afternoon.");

    // Drain the channel. The sender is dropped when `agent.run` returns
    // (we moved it in via `Some(tx)`), so `recv` will yield None promptly.
    let mut chunks: Vec<String> = Vec::new();
    while let Ok(n) = rx.try_recv() {
        if let AgentNotification::AssistantChunk(text) = n {
            chunks.push(text);
        }
    }
    let joined = chunks.concat();

    assert!(
        joined.contains("Here is the time:"),
        "expected leading prose in stream, got: {:?}",
        joined
    );
    assert!(
        joined.contains("done."),
        "expected trailing prose in stream, got: {:?}",
        joined
    );
    assert!(
        !joined.contains("<tool_call>"),
        "tool_call opener leaked into stream: {:?}",
        joined
    );
    assert!(
        !joined.contains("</tool_call>"),
        "tool_call closer leaked into stream: {:?}",
        joined
    );
    assert!(
        !joined.contains("datetime"),
        "tool_call body (function name) leaked into stream: {:?}",
        joined
    );
}

/// Compaction fires when the conversation exceeds the budget.
/// Strategy: a tiny n_ctx + a huge assistant response forces the budget check
/// to trigger between turns 2 and 3. If compaction fires, it consumes one
/// extra scripted response (the summary), so the final answer comes from
/// response[3] not response[2].
#[tokio::test]
async fn test_compaction_fires_when_over_budget() {
    let big_pad = "x".repeat(20_000);
    let mock = MockLlm::with_n_ctx(
        vec![
            // Turn 1: tool call
            r#"<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#
                .into(),
            // Turn 2: another tool call, but with massive padding to blow budget
            format!(
                "{}\n<tool_call>\n{{\"function\": \"datetime\", \"parameters\": {{}}}}\n</tool_call>",
                big_pad
            ),
            // Compaction's summarization call consumes this:
            "Summary: user asked for time; datetime was called twice.".into(),
            // Final answer (only reached if compaction fired and freed budget):
            "POST_COMPACTION_FINAL".into(),
        ],
        1_000, // n_ctx in tokens; threshold = 800 tokens ≈ 3200 chars
    );
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let result = agent
        .run(&skill, "What time is it?", &[], "", "", "", None)
        .await
        .unwrap();

    assert_eq!(
        result.response, "POST_COMPACTION_FINAL",
        "expected the final response after compaction; got something else, suggesting compaction did not fire"
    );
}

/// When the context is over budget on every check and MAX_COMPACTIONS (3)
/// compactions aren't enough to fix that, `run` must return an error instead
/// of silently sending an over-budget request to the LLM.
///
/// Strategy: n_ctx is tiny (10 tokens => ~9-token / ~36-char budget), so the
/// budget check is over-budget from the very first message onward — no need
/// to engineer huge padding. Walking through `Agent::run`'s loop by hand:
///   * iteration 0: `messages.len() == 2` (system + user), so `compact()`
///     short-circuits for free (its own "< 4 messages" guard) — no LLM call.
///     Then the turn's own LLM call consumes response[0] (a tool call).
///   * iteration 1: `messages.len() == 4` now, so `compact()` does real work
///     and consumes response[1] (the summary). Turn's LLM call consumes
///     response[2] (another tool call).
///   * iteration 2: same shape — compact() consumes response[3] (summary),
///     turn's LLM call consumes response[4] (another tool call).
///   * iteration 3: `compactions_done == MAX_COMPACTIONS (3)` and the context
///     is still over budget => `run` must bail here, with NO 6th scripted
///     response needed. If it instead compacted again or, worse, sent the
///     over-budget request, MockLlm would panic with "no more scripted
///     responses" once exhausted, or the test would time out waiting for a
///     final answer that never comes.
#[tokio::test]
async fn test_run_errors_when_compactions_exhausted_and_still_over_budget() {
    let tool_call = || {
        r#"<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#
            .to_string()
    };

    let mock = MockLlm::with_n_ctx(
        vec![
            tool_call(),        // iteration 0's turn
            "Summary A".into(), // iteration 1's compaction
            tool_call(),        // iteration 1's turn
            "Summary B".into(), // iteration 2's compaction
            tool_call(),        // iteration 2's turn
                                // iteration 3 must bail before consuming a 6th response
        ],
        10, // n_ctx tokens; budget = 9 tokens ≈ 36 chars — always exceeded
    );
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let err = agent
        .run(&skill, "What time is it?", &[], "", "", "", None)
        .await
        .unwrap_err();

    let msg = format!("{}", err);
    assert!(
        msg.contains("budget") && msg.contains("compaction"),
        "expected an over-budget/compactions-exhausted error, got: {}",
        msg
    );
}
