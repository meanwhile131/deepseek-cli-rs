# DeepSeek CLI Project

This is a Rust-based command-line interface for interacting with the DeepSeek API. It supports persistent chat sessions, tool execution, and context management.

## Features
- Interactive chat with streaming responses
- Tool calling (list_files, read_file, create_directory, apply_search_replace, run_command)
- Persistent chat sessions (resume with chat ID)
- Arrow key navigation and command history (via rustyline)
- Token loaded from env var or config file (~/.config/deepseek-cli/token or ~/.deepseek_token)
- Project context from DEEPSEEK.md (this file) automatically injected into system prompt

## Architecture
- `src/main.rs`: Main CLI loop, handles user input, streaming, and tool execution loop.
- `src/tools.rs`: Tool definitions, handlers, and system prompt generation.
- Uses `deepseek-api` crate for API communication.
- Tokio async runtime.

## Tool Usage
The assistant can invoke tools by outputting lines starting with `TOOL:` followed by tool name and arguments. Multiple tools can be called in one response; they are executed sequentially. Results are returned with `TOOL RESULT for <tool>:`. The assistant must not simulate results.

Available tools:
- list_files: lists files in a directory
- read_file: reads a file
- create_directory: creates a directory
- apply_search_replace: applies search/replace blocks to a file
- run_command: runs a shell command
- write_file: writes content to a file (overwrites if exists, creates parent directories)

## Conventions
- Always use the provided tools to interact with the filesystem and run commands.
- Keep DEEPSEEK.md updated with relevant project context, especially when adding new features or changing conventions.
- The assistant will proactively update DEEPSEEK.md when changes are made to the project (e.g., adding tools, modifying conventions).
- Use `cargo build` to test changes, and commit with `git` when appropriate.
- Aim for clean code with no clippy warnings.

## Development
To build: `cargo build`
To run: `cargo run [chat-id]`
To test: `cargo clippy`

## Recent Changes
- Refactored `src/main.rs`: split the large `main` function into smaller functions (`run_chat`, `handle_tool_calls`) to reduce line count and address `clippy::pedantic` warning.