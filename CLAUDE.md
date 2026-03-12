# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**OpenFerris** is an AI personal assistant CLI application written in Rust (edition 2024). It aims to be a simpler, more tailored alternative to OpenClaw.

## Build & Development Commands

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo run                # Run the application
cargo test               # Run all tests
cargo test <test_name>   # Run a single test by name
cargo clippy             # Lint
cargo fmt                # Format code
cargo fmt -- --check     # Check formatting without modifying
```

## Architecture

The project uses a lib+binary crate split. Core modules (agent, config, llm, tools, skills, etc.) live in `src/lib.rs` and are importable by integration tests. Binary-only modules (daemon, telegram, gmail, tui, client, memories) live in `src/main.rs` and use `openferris::` to reference lib modules.

### Testing the Agent

```bash
# Run deterministic integration tests (MockLlm, no real LLM needed)
cargo test --test agent_integration

# Run a prompt through the real agent with full debug trace
cargo run -- test-agent "What time is it?"
cargo run -- test-agent --skill daily-briefing "Run the briefing"

# test-agent defaults to debug-level logging; override with RUST_LOG
RUST_LOG=trace cargo run -- test-agent "Hello"
```

## Agent Workspace

The agent's workspace is at `~/.local/share/openferris/workspace/`. When making changes that affect the agent's tools or capabilities, update `workspace/RELEASE_NOTES.md` so the agent can stay current. The agent reads these notes via the default skill prompt.

## Rust Edition

Uses Rust **2024 edition** (rustc 1.93+). This means newer language features and the 2024 edition's updated defaults are available.
