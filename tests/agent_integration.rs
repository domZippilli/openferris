use openferris::agent::Agent;
use openferris::llm::mock::MockLlm;
use openferris::skills::Skill;
use openferris::tools::datetime::DateTimeTool;
use openferris::tools::ToolRegistry;

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

    let result = agent.run(&skill, "Hi", &[], "", "", "").await.unwrap();

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
        .run(&skill, "What time is it?", &[], "", "", "")
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
        .run(&skill, "Fetch example.com", &[], "", "", "")
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
        .run(&skill, "I prefer dark mode", &[], "", "", "")
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
        .run(&skill, "Double check the time", &[], "", "", "")
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
        .run(&skill, "Loop forever", &[], "", "", "")
        .await
        .unwrap_err();

    assert!(
        format!("{}", err).contains("maximum iterations"),
        "Expected max iterations error, got: {}",
        err
    );
}

/// Tool call tags in the final response are stripped.
#[tokio::test]
async fn test_tool_call_tags_stripped_from_response() {
    // The second response contains leftover tool_call tags that should be stripped.
    let mock = MockLlm::new(vec![
        r#"<tool_call>
{"function": "datetime", "parameters": {}}
</tool_call>"#.into(),
        "Done.\n\n<tool_call>\n{\"broken\": true}\n</tool_call>\n\nAll good.".into(),
    ]);
    let agent = Agent::new(Box::new(mock), test_registry(), String::new());
    let skill = test_skill(&["datetime"]);

    let result = agent
        .run(&skill, "test", &[], "", "", "")
        .await
        .unwrap();

    assert!(!result.response.contains("<tool_call>"));
    assert!(result.response.contains("All good"));
}
