use anyhow::{Result, anyhow};
use deepseek_api::{DeepSeekAPI, StreamChunk, models::Message};
use futures_util::{Stream, StreamExt, pin_mut};
use std::env;
use std::io::Write;
use std::path::Path;

use tokio::fs;

mod tools;
use colored::*;
use tools::{SYSTEM_PROMPT, execute_tool};
use rustyline::{DefaultEditor, error::ReadlineError};
use std::sync::{Arc, Mutex};

async fn handle_stream<S>(stream: S) -> Result<Option<Message>>
where
    S: Stream<Item = Result<StreamChunk>>,
{
    pin_mut!(stream);
    let mut final_message = None;
    let mut thinking_started = false;
    let mut content_started = false;
    while let Some(chunk) = stream.next().await {
        match chunk? {
            StreamChunk::Thinking(thought) => {
                if !thinking_started {
                    println!("{}", "--- Thinking ---".yellow());
                    thinking_started = true;
                }
                print!("{}", thought.dimmed());
                std::io::stdout().flush()?;
            }
            StreamChunk::Content(text) => {
                if !content_started {
                    if thinking_started {
                        println!("\n{}", "--- End of thinking ---".yellow());
                    }
                    println!("{}", "--- Response ---".green());
                    content_started = true;
                }
                print!("{}", text.bright_white());
                std::io::stdout().flush()?;
            }
            StreamChunk::Message(msg) => {
                if thinking_started && !content_started {
                    println!("\n{}", "--- End of thinking ---".yellow());
                }
                final_message = Some(msg);
                println!(); // newline after content
            }
        }
    }
    Ok(final_message)
}

async fn load_token() -> Result<String> {
    // Try environment variable first
    if let Ok(token) = env::var("DEEPSEEK_TOKEN") {
        return Ok(token);
    }

    // Try config file locations
    let paths = [
        dirs::config_dir().map(|d| d.join("deepseek-cli/token")),
        dirs::home_dir().map(|h| h.join(".deepseek_token")),
    ];

    for path_opt in paths.iter().flatten() {
        if path_opt.exists() {
            let content = fs::read_to_string(path_opt).await?;
            let token = content.trim().to_string();
            if !token.is_empty() {
                println!("Loaded token from {}", path_opt.display());
                return Ok(token);
            }
        }
    }

    Err(anyhow!(
        "DEEPSEEK_TOKEN environment variable not set and no token file found in:\n\
         - ~/.config/deepseek-cli/token\n\
         - ~/.deepseek_token\n\
         Please create one with your API token."
    ))
}

async fn read_deepseek_context() -> Result<Option<String>> {
    let path = Path::new("DEEPSEEK.md");
    if path.exists() {
        let content = fs::read_to_string(path).await?;
        if !content.trim().is_empty() {
            return Ok(Some(content));
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    let token = load_token().await?;

    let api = DeepSeekAPI::new(token).await?;

    let args: Vec<String> = env::args().collect();
    let (chat_id, mut parent_id) = if args.len() > 1 {
        let id = args[1].clone();
        println!("Resuming chat with ID: {}", &id);
        // For simplicity, we do not fetch previous messages.
        // New messages will be sent to the same chat, but may not be threaded to previous ones.
        let chat =  api.get_chat_info(&id).await?;
        (id, chat.current_message_id)
    } else {
        let chat = api.create_chat().await?;
        let id = chat.id;
        println!("Chat created with ID: {}", id);
        (id, None)
    };
    println!("System prompt loaded. Type your messages (type '/exit' to quit):");

    // Setup rustyline editor for line editing with arrow keys
    let rl = Arc::new(Mutex::new(DefaultEditor::new()?));
    // Load history if exists
    {
        let mut rl_guard = rl.lock().unwrap();
        let _ = rl_guard.load_history(".deepseek_history");
    }

    loop {
        // Use rustyline for line editing with arrow keys
        let rl_clone = rl.clone();
        let prompt = format!("{}", "> ".cyan().bold());
        let line_result = tokio::task::spawn_blocking(move || {
            let mut rl_guard = rl_clone.lock().unwrap();
            rl_guard.readline(&prompt)
        }).await;

        let line = match line_result {
            Ok(Ok(l)) => l,
            Ok(Err(ReadlineError::Eof)) | Ok(Err(ReadlineError::Interrupted)) => break,
            Ok(Err(e)) => {
                eprintln!("Input error: {}", e);
                continue;
            }
            Err(e) => {
                eprintln!("Spawn blocking error: {}", e);
                continue;
            }
        };

        // Add to history
        if let Err(e) = rl.lock().unwrap().add_history_entry(&line) {
            eprintln!("Failed to add history entry: {}", e);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" {
            break;
        }

        // Prepend system prompt only on the very first message
        let prompt = if parent_id.is_none() {
            let mut base = SYSTEM_PROMPT.to_string();
            if let Some(ctx) = read_deepseek_context().await? {
                base.push_str("\n\nProject context from DEEPSEEK.md:\n");
                base.push_str(&ctx);
            }
            format!("{}\n\nUser: {}", base, trimmed)
        } else {
            trimmed.to_string()
        };

        // Stream the assistant's response
        let stream = api.complete_stream(
            chat_id.clone(),
            prompt,
            parent_id,
            true, // search
            true, // thinking
        );
        let final_message = handle_stream(stream).await?;
        let mut current_msg = match final_message {
            Some(msg) => msg,
            None => {
                eprintln!("No final message received");
                continue;
            }
        };
        parent_id = current_msg.message_id;

        // Automatic tool‑calling loop
        loop {
            let lines: Vec<&str> = current_msg.content.lines().collect();
            let mut i = 0;
            let mut invocations = Vec::new();

            while i < lines.len() {
                let line = lines[i].trim();
                if let Some(stripped) = line.strip_prefix("TOOL:") {
                    // Found a tool invocation start
                    let tool_line = stripped.trim(); // after "TOOL:"
                    // Split tool_line into name and optional first argument
                    let mut tool_parts = tool_line.splitn(2, ' ');
                    let tool_name = tool_parts.next().unwrap_or("").to_string();
                    let first_arg = tool_parts.next().unwrap_or("").to_string();

                    // Collect subsequent lines until next TOOL: or end
                    let mut body_lines = Vec::new();
                    i += 1;
                    while i < lines.len() && !lines[i].trim().starts_with("TOOL:") {
                        body_lines.push(lines[i]);
                        i += 1;
                    }
                    // body_lines contains the raw lines (preserve newlines)
                    let body = body_lines.join("\n");

                    // Combine first_arg and body into the full argument string
                    let full_arg = if body.is_empty() {
                        first_arg
                    } else {
                        format!("{}\n{}", first_arg, body)
                    };
                    invocations.push((tool_name, full_arg));
                } else {
                    i += 1;
                }
            }

            if invocations.is_empty() {
                break;
            }

            // Execute all requested tools
            let mut results = Vec::new();
            for (tool_name, full_arg) in invocations {
                match execute_tool(&tool_name, &full_arg).await {
                    Ok(output) => {
                        // Print a concise status to the console
                        let status = match tool_name.as_str() {
                            "read_file" => {
                                let path = full_arg.lines().next().unwrap_or("?");
                                format!("Read file at {}", path)
                            }
                            "apply_search_replace" => {
                                // output is already concise: "Applied X block(s) to Y"
                                output.clone()
                            }
                            "list_files" => {
                                let count = output.lines().count();
                                let dir = full_arg.lines().next().unwrap_or("?");
                                format!("Listed {} files in {}", count, dir)
                            }
                            "create_directory" => {
                                output.clone() // already concise
                            }
                            "run_command" => {
                                // Extract exit code from output if present
                                let exit_code = if output.starts_with("EXIT_CODE:") {
                                    if let Some(line) = output.lines().next() {
                                        line.strip_prefix("EXIT_CODE:").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1)
                                    } else { -1 }
                                } else { -1 };
                                if exit_code == 0 {
                                    "Command succeeded (exit code: 0)".to_string()
                                } else {
                                    format!("Command failed (exit code: {})", exit_code)
                                }
                            }
                            _ => format!("Executed tool: {}", tool_name),
                        };
                        println!("{}", status.cyan());
                        results.push(format!("TOOL RESULT for {}:\n{}", tool_name, output));
                    }
                    Err(e) => {
                        eprintln!("{}", format!("Tool {} failed: {}", tool_name, e).red());
                        results.push(format!("TOOL {} failed: {}", tool_name, e));
                    }
                }
            }

            // Send tool results back as a new user message
            let next_prompt = results.join("\n\n") + "\n\nContinue with the next step or provide the final answer.";
            let stream2 = api.complete_stream(
                chat_id.clone(),
                next_prompt,
                parent_id,
                true, // search
                true, // thinking
            );
            if let Some(msg) = handle_stream(stream2).await? {
                current_msg = msg;
                parent_id = current_msg.message_id;
            } else {
                break;
            }
        }
    }

    // Save history
    if let Err(e) = rl.lock().unwrap().save_history(".deepseek_history") {
        eprintln!("Failed to save history: {}", e);
    }

    Ok(())
}
