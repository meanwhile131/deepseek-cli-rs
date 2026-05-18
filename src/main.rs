use anyhow::{Result, anyhow};
use deepseek_api::{DeepSeekAPI, StreamChunk, models::Message};

use futures_util::{Stream, StreamExt, pin_mut};
use std::env;
use std::io::Write;

use colored::Colorize;
use deepseek_cli::tools;
use rustyline::{DefaultEditor, error::ReadlineError};
use std::sync::{Arc, Mutex};
use tokio::fs;
use tokio::sync::broadcast;
use tools::{SYSTEM_PROMPT, ToolOutput, execute_tool};

enum UserInput {
    Message(String),
    Exit,
    Interrupted,
}

async fn handle_stream<S>(
    stream: S,
    ctrl_rx: &mut broadcast::Receiver<()>,
) -> Result<Option<Message>>
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

async fn stream_with_retry<S, F, Fut>(
    mut stream_factory: F,
    ctrl_rx: &mut broadcast::Receiver<()>,
) -> Result<Option<Message>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<S>>,
    S: Stream<Item = Result<StreamChunk>>,
{
    let mut delay = tokio::time::Duration::from_secs(5);
    let max_delay = tokio::time::Duration::from_mins(1);
    let mut attempt = 0;
    loop {
        let stream = stream_factory().await?;
        match handle_stream(stream, ctrl_rx).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("Messages too frequent") {
                    attempt += 1;
                    eprintln!("Rate limited, retrying in {delay:?}... (attempt {attempt})");
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, max_delay);
                    continue;
                }
                return Err(e);
            }
        }
    }
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

async fn collect_user_input(rl: Arc<Mutex<DefaultEditor>>) -> UserInput {
    let prompt = format!("{}", "> ".cyan().bold());

    // Read a single line (which may contain newlines if Shift+Enter was used)
    let line = loop {
        let rl_clone = rl.clone();
        let prompt_clone = prompt.clone();
        let line_result = tokio::task::spawn_blocking(move || {
            let mut rl_guard = rl_clone.lock().unwrap();
            rl_guard.readline(&prompt_clone)
        })
        .await;

        match line_result {
            Ok(Ok(l)) => break l,
            Ok(Err(ReadlineError::Eof)) => return UserInput::Exit,
            Ok(Err(ReadlineError::Interrupted)) => {
                println!();
                return UserInput::Interrupted;
            }
            Ok(Err(e)) => {
                eprintln!("Input error: {e}");
                // continue to retry
            }
            Err(e) => {
                eprintln!("Spawn blocking error: {e}");
                // continue to retry
            }
        }
    };

    let trimmed = line.trim();
    if trimmed == "/exit" {
        UserInput::Exit
    } else if trimmed.is_empty() {
        // ignore empty input and restart
        UserInput::Interrupted
    } else {
        UserInput::Message(line)
    }
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

    // Get project context automatically
    let project_context = match tools::execute_tool("get_project_context", "").await {
        Ok(tools::ToolOutput::Text { content, .. }) => content,
        _ => "Unable to determine project context.".to_string(),
    };

    println!("System prompt loaded. Type your messages (type '/exit' to quit):");

    // Setup rustyline editor for line editing with arrow keys (in-memory history only)
    let rl = Arc::new(Mutex::new(DefaultEditor::new()?));

    run_chat(api, chat_id, parent_id, rl, project_context).await
}

async fn run_chat(
    api: DeepSeekAPI,
    chat_id: String,
    mut parent_id: Option<i64>,
    rl: Arc<Mutex<DefaultEditor>>,
    project_context: String,
) -> Result<()> {
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
        match collect_user_input(rl.clone()).await {
            UserInput::Exit => {
                println!("Chat ID: {chat_id}");
                break 'outer;
            }
            UserInput::Interrupted => {}
            UserInput::Message(full_input) => {
                handle_user_message(
                    &api,
                    &chat_id,
                    &mut parent_id,
                    &tx,
                    &rl,
                    &project_context,
                    full_input,
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn parse_tool_invocations(content: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut invocations = Vec::new();
    while i < lines.len() {
        // Allow leading whitespace before TOOL:
        let trimmed_line = lines[i].trim();

        if trimmed_line.starts_with("TOOL:") {
            // Extract the part after "TOOL:" (case-insensitive? but we'll keep exact)
            let after_tool = trimmed_line.strip_prefix("TOOL:").unwrap_or("").trim();
            let mut tool_parts = after_tool.splitn(2, ' ');
            let tool_name = tool_parts.next().unwrap_or("").to_string();
            let first_arg = tool_parts.next().unwrap_or("").to_string();

            let mut body_lines = Vec::new();
            i += 1;
            // Collect lines until we find another line that starts with "TOOL:" (ignoring leading whitespace)
            // or an escaped \TOOL: line (which is treated as literal content, not a tool separator)
            while i < lines.len() {
                let next_trimmed = lines[i].trim();
                if next_trimmed.starts_with("TOOL:") {
                    break;
                }
                // Escaped lines are literal, so we include them in the body (with backslash removed)
                if next_trimmed.starts_with("\\TOOL:") {
                    let leading_spaces = lines[i].len() - next_trimmed.len();
                    let unescaped = next_trimmed.strip_prefix("\\").unwrap_or(next_trimmed);
                    let new_line = " ".repeat(leading_spaces) + unescaped;
                    body_lines.push(new_line);
                    i += 1;
                    continue;
                }
                body_lines.push(lines[i].to_string());
                i += 1;
            }
            let body = body_lines.join("\n");

            let full_arg = if body.is_empty() {
                first_arg
            } else if first_arg.is_empty() {
                body
            } else {
                format!("{first_arg}\n{body}")
            };
            // Don't add empty tool names
            if !tool_name.is_empty() {
                invocations.push((tool_name, full_arg));
            }
        } else {
            i += 1;
        }
    }
    invocations
}

async fn process_single_tool(
    api: &DeepSeekAPI,
    tool_name: &str,
    full_arg: &str,
) -> (Option<String>, String) {
    // For single-line path tools, extract first line if multiple lines provided
    let single_line_path_tools = ["read_file", "create_directory", "list_files"];
    let arg_to_use = if single_line_path_tools.contains(&tool_name) && full_arg.contains('\n') {
        let first_line = full_arg.lines().next().unwrap_or("").trim().to_string();
        if !first_line.is_empty() {
            eprintln!("{}", format!("Note: path argument contained newlines; using only the first line: '{first_line}'").yellow());
        }
        first_line
    } else {
        full_arg.trim().to_string()
    };
    match execute_tool(tool_name, &arg_to_use).await {
        Ok(tool_output) => {
            // Print status for all variants
            let status = match &tool_output {
                ToolOutput::Text { status, .. }
                | ToolOutput::Binary { status, .. }
                | ToolOutput::FileReference { status, .. }
                | ToolOutput::StatusOnly { status } => status,
            };
            println!("{}", status.cyan());

            match tool_output {
                ToolOutput::Text { content, status } => {
                    // Return text content inline, don't upload
                    (None, format!("{status}\n\n{content}"))
                }
                ToolOutput::Binary {
                    data,
                    mime_type,
                    status,
                } => {
                    // For binary data (e.g., screenshot), upload the file
                    let filename = if mime_type == "image/png" {
                        format!("screenshot_{}.png", chrono::Utc::now().timestamp())
                    } else {
                        format!("binary_data_{}", chrono::Utc::now().timestamp())
                    };
                    match api.upload_file(data, &filename, Some(&mime_type)).await {
                        Ok(file_info) => (Some(file_info.id), status),
                        Err(e) => {
                            eprintln!("Failed to upload binary data: {e}");
                            (None, format!("Binary data captured but upload failed: {e}"))
                        }
                    }
                }
                ToolOutput::FileReference { file_id, status } => (Some(file_id), status),
                ToolOutput::StatusOnly { status } => (None, status),
            }
        }
        Err(e) => {
            eprintln!("{}", format!("Tool {tool_name} failed: {e}").red());
            (None, format!("TOOL {tool_name} failed: {e}"))
        }
    }
}

async fn handle_tool_calls(
    api: &DeepSeekAPI,
    chat_id: &str,
    current_msg: Message,
    parent_id: &mut Option<i64>,
    ctrl_rx: &mut broadcast::Receiver<()>,
) -> Result<Option<Message>> {
    let invocations = parse_tool_invocations(&current_msg.content);

    if invocations.is_empty() {
        return Ok(None);
    }

    let mut file_ids = Vec::new();
    let mut result_messages = Vec::new();

    for (tool_name, full_arg) in invocations {
        let (file_id_opt, msg) = process_single_tool(api, &tool_name, &full_arg).await;
        if let Some(file_id) = file_id_opt {
            file_ids.push(file_id);
        }
        result_messages.push(msg);
    }

    let next_prompt = format!(
        "{}\n\nContinue with the next step or provide the final answer.",
        result_messages.join("\n\n")
    );
    let new_msg = match stream_with_retry(
        || async {
            Ok(api.complete_stream(
                chat_id.to_string(),
                next_prompt.clone(),
                *parent_id,
                true,
                true,
                file_ids.clone(),
            ))
        },
        ctrl_rx,
    )
    .await
    {
        Ok(Some(msg)) => msg,
        Ok(None) => return Ok(None),
        Err(e) => {
            eprintln!("Error during tool response streaming: {e}");
            return Ok(None);
        }
    };
    *parent_id = new_msg.message_id;
    Ok(Some(new_msg))
}

async fn handle_user_message(
    api: &DeepSeekAPI,
    chat_id: &str,
    parent_id: &mut Option<i64>,
    tx: &broadcast::Sender<()>,
    rl: &Arc<Mutex<DefaultEditor>>,
    project_context: &str,
    full_input: String,
) -> Result<()> {
    if full_input.is_empty() {
        return Ok(());
    }
    // Add full input to history as a single entry
    if let Err(e) = rl.lock().unwrap().add_history_entry(&full_input) {
        eprintln!("Failed to add history entry: {e}");
    }

    // Prepend system prompt and project context only on the very first message
    let prompt = if parent_id.is_none() {
        format!(
            "{}\n\n## Current Project Context\n{}\n\nUser:\n{}",
            SYSTEM_PROMPT.as_str(),
            project_context,
            full_input
        )
    } else {
        full_input.clone()
    };

    // Stream the assistant's response
    let mut rx = tx.subscribe();
    let final_message = match stream_with_retry(
        || async {
            Ok(api.complete_stream(
                chat_id.to_string(),
                prompt.clone(),
                *parent_id,
                true,
                true,
                vec![],
            ))
        },
        &mut rx,
    )
    .await
    {
        Ok(Some(msg)) => msg,
        Ok(None) => return Ok(()),
        Err(e) => {
            eprintln!("Error during streaming: {e}");
            return Ok(());
        }
    };
    *parent_id = final_message.message_id;
    let current_msg = final_message;

    process_assistant_response(api, chat_id, current_msg, parent_id, &mut rx).await
}

async fn process_assistant_response(
    api: &DeepSeekAPI,
    chat_id: &str,
    mut current_msg: Message,
    parent_id: &mut Option<i64>,
    ctrl_rx: &mut broadcast::Receiver<()>,
) -> Result<()> {
    loop {
        // Ensure non-empty response
        while current_msg.content.trim().is_empty() {
            eprintln!(
                "{}",
                "Model returned empty response, reprompting with warning...".yellow()
            );
            let warning = "WARNING: Your previous response was empty. Please provide a meaningful response or use tools as appropriate.\n\nContinue with the next step or provide the final answer.";
            let new_msg = match stream_with_retry(
                || async {
                    Ok(api.complete_stream(
                        chat_id.to_string(),
                        warning.to_string(),
                        *parent_id,
                        true,
                        true,
                        vec![],
                    ))
                },
                ctrl_rx,
            )
            .await
            {
                Ok(Some(msg)) => msg,
                Ok(None) => return Ok(()),
                Err(e) => {
                    eprintln!("Error during streaming for empty response: {e}");
                    return Ok(());
                }
            };
            *parent_id = new_msg.message_id;
            current_msg = new_msg;
        }

        // Handle tool calls
        match handle_tool_calls(api, chat_id, current_msg, parent_id, ctrl_rx).await {
            Ok(Some(new_msg)) => {
                current_msg = new_msg;
                // continue loop
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("Error during tool call processing: {e}");
                return Ok(());
            }
        }
    }
    Ok(())
}
