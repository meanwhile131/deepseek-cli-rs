use anyhow::{Result, anyhow};
use deepseek_api::{DeepSeekAPI, StreamChunk, models::Message};
use futures_util::{Stream, StreamExt, pin_mut};
use std::env;
use std::io::Write;
use std::path::Path;

use tokio::fs;
use tokio::sync::broadcast;
mod tools;
use colored::Colorize;
use tools::{SYSTEM_PROMPT, execute_tool};
use rustyline::{DefaultEditor, error::ReadlineError};
use std::sync::{Arc, Mutex};

async fn handle_stream<S>(stream: S, ctrl_rx: &mut broadcast::Receiver<()>) -> Result<Option<Message>>
where
    S: Stream<Item = Result<StreamChunk>>,
{
    pin_mut!(stream);
    let mut final_message = None;
    let mut thinking_started = false;
    let mut content_started = false;
    loop {
        tokio::select! {
            maybe_chunk = stream.next() => {
                match maybe_chunk {
                    Some(chunk) => {
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
                    None => break,
                }
            }
            _ = ctrl_rx.recv() => {
                println!("\n{}", "Stream interrupted by user".yellow());
                return Ok(None);
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
    let (chat_id, parent_id) = if args.len() > 1 {
        let id = args[1].clone();
        println!("Resuming chat with ID: {}", &id);
        let chat = api.get_chat_info(&id).await?;
        (id, chat.current_message_id)
    } else {
        let chat = api.create_chat().await?;
        let id = chat.id;
        println!("Chat created with ID: {id}");
        (id, None)
    };
    println!("System prompt loaded. Type your messages (type '/exit' to quit):");

    // Setup rustyline editor for line editing with arrow keys (in-memory history only)
    let rl = Arc::new(Mutex::new(DefaultEditor::new()?));

    run_chat(api, chat_id, parent_id, rl).await
}

async fn run_chat(api: DeepSeekAPI, chat_id: String, mut parent_id: Option<i64>, rl: Arc<Mutex<DefaultEditor>>) -> Result<()> {
    // Setup Ctrl+C handling using broadcast so each round gets a fresh receiver
    let (tx, _) = broadcast::channel(1);
    let tx_task = tx.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = tx_task.send(());
            }
        }
    });

    'outer: loop {
        // Use rustyline for line editing with arrow keys
        let rl_clone = rl.clone();
        let prompt = format!("{}", "> ".cyan().bold());
        let line_result = tokio::task::spawn_blocking(move || {
            let mut rl_guard = rl_clone.lock().unwrap();
            rl_guard.readline(&prompt)
        }).await;

        let line = match line_result {
            Ok(Ok(l)) => l,
            Ok(Err(ReadlineError::Eof)) => break,
            Ok(Err(ReadlineError::Interrupted)) => {
                println!(); // move to next line after ^C
                continue;
            }
            Ok(Err(e)) => {
                eprintln!("Input error: {e}");
                continue;
            }
            Err(e) => {
                eprintln!("Spawn blocking error: {e}");
                continue;
            }
        };

        // Add to history
        if let Err(e) = rl.lock().unwrap().add_history_entry(&line) {
            eprintln!("Failed to add history entry: {e}");
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
            format!("{base}\n\nUser: {trimmed}")
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
        let mut rx = tx.subscribe();
        let final_message = handle_stream(stream, &mut rx).await?;
        let Some(mut current_msg) = final_message else {
            // Stream was interrupted; return to input prompt silently
            continue;
        };
        parent_id = current_msg.message_id;

        loop {
            // Ensure non-empty response
            while current_msg.content.trim().is_empty() {
                eprintln!("{}", "Model returned empty response, reprompting with warning...".yellow());
                let warning = "WARNING: Your previous response was empty. Please provide a meaningful response or use tools as appropriate.\n\nContinue with the next step or provide the final answer.";
                let stream = api.complete_stream(
                    chat_id.clone(),
                    warning.to_string(),
                    parent_id,
                    true,
                    true,
                );
                let mut rx_inner = tx.subscribe();
                let new_msg = handle_stream(stream, &mut rx_inner).await?;
                match new_msg {
                    Some(msg) => {
                        parent_id = msg.message_id;
                        current_msg = msg;
                    }
                    None => {
                        // Stream interrupted during reprompt; go back to user input silently
                        continue 'outer;
                    }
                }
            }

            // Handle tool calls
            match handle_tool_calls(&api, &chat_id, current_msg, &mut parent_id, &mut rx).await? {
                Some(new_msg) => {
                    current_msg = new_msg;
                    // parent_id already updated inside handle_tool_calls
                    continue;
                }
                None => {
                    // No more tool calls, done with this assistant turn
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn handle_tool_calls(api: &DeepSeekAPI, chat_id: &str, current_msg: Message, parent_id: &mut Option<i64>, ctrl_rx: &mut broadcast::Receiver<()>) -> Result<Option<Message>> {
    let lines: Vec<&str> = current_msg.content.lines().collect();
    let mut i = 0;
    let mut invocations = Vec::new();

    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(stripped) = line.strip_prefix("TOOL:") {
            let tool_line = stripped.trim();
            let mut tool_parts = tool_line.splitn(2, ' ');
            let tool_name = tool_parts.next().unwrap_or("").to_string();
            let first_arg = tool_parts.next().unwrap_or("").to_string();

            let mut body_lines = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("TOOL:") {
                body_lines.push(lines[i]);
                i += 1;
            }
            let body = body_lines.join("\n");

            // Fix: avoid leading newline when first_arg is empty
            let full_arg = if body.is_empty() {
                first_arg
            } else if first_arg.is_empty() {
                body
            } else {
                format!("{first_arg}\n{body}")
            };
            invocations.push((tool_name, full_arg));
        } else {
            i += 1;
        }
    }

    if invocations.is_empty() {
        return Ok(None);
    }

    let mut results = Vec::new();
    for (tool_name, full_arg) in invocations {
        match execute_tool(&tool_name, &full_arg).await {
            Ok(output) => {
                let status = match tool_name.as_str() {
                    "read_file" => {
                        let path = full_arg.lines().next().unwrap_or("?");
                        format!("Read file at {path}")
                    }
                    "apply_search_replace" | "create_directory" => {
                        output.clone()
                    }
                    "list_files" => {
                        let count = output.lines().count();
                        let dir = full_arg.lines().next().unwrap_or("?");
                        format!("Listed {count} files in {dir}")
                    }
                    "run_command" => {
                        let exit_code = if output.starts_with("EXIT_CODE:") {
                            if let Some(line) = output.lines().next() {
                                line.strip_prefix("EXIT_CODE:").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1)
                            } else { -1 }
                        } else { -1 };
                        if exit_code == 0 {
                            "Command succeeded (exit code: 0)".to_string()
                        } else {
                            format!("Command failed (exit code: {exit_code})")
                        }
                    }
                    _ => format!("Executed tool: {tool_name}"),
                };
                println!("{}", status.cyan());
                results.push(format!("TOOL RESULT for {tool_name}:\n{output}"));
            }
            Err(e) => {
                eprintln!("{}", format!("Tool {tool_name} failed: {e}").red());
                results.push(format!("TOOL {tool_name} failed: {e}"));
            }
        }
    }

    let next_prompt = results.join("\n\n") + "\n\nContinue with the next step or provide the final answer.";
    let stream = api.complete_stream(
        chat_id.to_string(),
        next_prompt,
        *parent_id,
        true,
        true,
    );
    let new_msg = handle_stream(stream, ctrl_rx).await?;
    if let Some(msg) = new_msg {
        *parent_id = msg.message_id;
        Ok(Some(msg))
    } else {
        Ok(None)
    }
}