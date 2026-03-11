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

This is a binary crate (`src/main.rs` entry point). The project is in its early stages — structure will evolve as features are added.

## Rust Edition

Uses Rust **2024 edition** (rustc 1.93+). This means newer language features and the 2024 edition's updated defaults are available.
