use anyhow::{Result, anyhow};
use deepseek_api::{DeepSeekAPI, StreamChunk, models::Message};

use futures_util::{Stream, StreamExt, pin_mut};
use std::env;
use std::io::Write;
use std::path::Path;

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
    println!("System prompt loaded. Type your messages (type '/exit' to quit):");

    // Setup rustyline editor for line editing with arrow keys (in-memory history only)
    let rl = Arc::new(Mutex::new(DefaultEditor::new()?));

    run_chat(api, chat_id, parent_id, rl).await
}

async fn run_chat(
    api: DeepSeekAPI,
    chat_id: String,
    mut parent_id: Option<i64>,
    rl: Arc<Mutex<DefaultEditor>>,
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
            UserInput::Exit => break 'outer,
            UserInput::Interrupted => {}
            UserInput::Message(full_input) => {
                if full_input.is_empty() {
                    continue;
                }
                // Add full input to history as a single entry
                if let Err(e) = rl.lock().unwrap().add_history_entry(&full_input) {
                    eprintln!("Failed to add history entry: {e}");
                }

                // Prepend system prompt only on the very first message
                let prompt = if parent_id.is_none() {
                    format!("{}\n\nUser:\n{}", SYSTEM_PROMPT.as_str(), full_input)
                } else {
                    full_input.clone()
                };

                // Stream the assistant's response
                let stream = api.complete_stream(
                    chat_id.clone(),
                    prompt,
                    parent_id,
                    true,   // search
                    true,   // thinking
                    vec![], // ref_file_ids
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
                        eprintln!(
                            "{}",
                            "Model returned empty response, reprompting with warning...".yellow()
                        );
                        let warning = "WARNING: Your previous response was empty. Please provide a meaningful response or use tools as appropriate.\n\nContinue with the next step or provide the final answer.";
                        let stream = api.complete_stream(
                            chat_id.clone(),
                            warning.to_string(),
                            parent_id,
                            true,
                            true,
                            vec![], // ref_file_ids
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
                    match handle_tool_calls(&api, &chat_id, current_msg, &mut parent_id, &mut rx)
                        .await?
                    {
                        Some(new_msg) => {
                            current_msg = new_msg;
                            // parent_id already updated inside handle_tool_calls
                        }
                        None => {
                            // No more tool calls, done with this assistant turn
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn upload_tool_output(
    api: &DeepSeekAPI,
    content: &str,
    tool_name: &str,
    full_arg: &str,
) -> Result<String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Generate filename based on tool type
    let filename = match tool_name {
        "read_file" => {
            // Extract original filename from the path argument
            let path_str = full_arg.lines().next().unwrap_or("");
            Path::new(path_str)
                .file_name()
                .and_then(|n| n.to_str())
                .map_or_else(
                    || {
                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_nanos();
                        format!("read_file_{timestamp}.txt")
                    },
                    ToString::to_string,
                )
        }
        "fetch_url" => {
            // Create a filename from the URL
            let url_part = full_arg.lines().next().unwrap_or("url");
            // Remove protocol and replace non-alphanumeric characters
            let url_clean = url_part
                .replace("https://", "")
                .replace("http://", "")
                .replace(|c: char| !c.is_alphanumeric() && c != '.', "_");
            format!("{url_clean}.html")
        }
        "browser_get_html" => {
            // Try to get a descriptive name from the URL or use default
            let url_part = full_arg.lines().next().unwrap_or("page");
            let sanitized = url_part.replace(|c: char| !c.is_alphanumeric() && c != '.', "_");
            format!("{sanitized}.html")
        }
        _ => {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            format!("tool_result_{tool_name}_{timestamp}.txt")
        }
    };

    let file_data = content.as_bytes().to_vec();
    let file_info = api.upload_file(file_data, &filename, None).await?;
    Ok(file_info.id)
}

fn parse_tool_invocations(content: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
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
    invocations
}

async fn process_single_tool(
    api: &DeepSeekAPI,
    tool_name: &str,
    full_arg: &str,
) -> (Option<String>, String) {
    // Validate single-line path tools
    let single_line_path_tools = ["read_file", "create_directory", "list_files"];
    if single_line_path_tools.contains(&tool_name) && full_arg.contains('\n') {
        let err_msg = format!("TOOL {tool_name} failed: path argument must be on a single line (no newlines)");
        eprintln!("{}", err_msg.red());
        return (None, err_msg);
    }
    match execute_tool(tool_name, full_arg).await {
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
                    // Tools that should upload their output as files
                    let upload_tools = [
                        "read_file",
                        "fetch_url",
                        "list_files",
                        "run_command",
                        "search_web",
                        "browser_get_html",
                    ];
                    if upload_tools.contains(&tool_name) {
                        // Upload the content
                        match upload_tool_output(api, &content, tool_name, full_arg).await {
                            Ok(file_id) => (Some(file_id), status),
                            Err(e) => {
                                eprintln!("Failed to upload tool output: {e}");
                                (None, format!("{status}\n\n{content}"))
                            }
                        }
                    } else {
                        // Just return the status as the message, no file upload
                        (None, status)
                    }
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
    let stream = api.complete_stream(
        chat_id.to_string(),
        next_prompt,
        *parent_id,
        true,
        true,
        file_ids,
    );
    let new_msg = handle_stream(stream, ctrl_rx).await?;
    if let Some(msg) = new_msg {
        *parent_id = msg.message_id;
        Ok(Some(msg))
    } else {
        Ok(None)
    }
}
