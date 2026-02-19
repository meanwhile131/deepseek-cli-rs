use anyhow::Result;
use deepseek_api::{DeepSeekAPI, StreamChunk};
use futures_util::{pin_mut, StreamExt};
use std::env;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

mod tools;
use tools::SYSTEM_PROMPT;
use colored::*;

#[tokio::main]
async fn main() -> Result<()> {
    let token = env::var("DEEPSEEK_TOKEN")
        .expect("DEEPSEEK_TOKEN environment variable must be set");

    let api = DeepSeekAPI::new(token).await?;
    let chat = api.create_chat().await?;
    let chat_id = chat.id.clone();
    println!("Chat created with ID: {}", chat_id);
    println!("System prompt loaded. Type your messages (type '/exit' to quit):");

    let mut parent_id: Option<i64> = None;
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    loop {
        eprint!("{}", "> ".cyan().bold());
        io::stderr().flush().await?;
        let line = match lines.next_line().await? {
            Some(l) => l,
            None => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" {
            break;
        }

        // Prepend system prompt only on the very first message
        let prompt = if parent_id.is_none() {
            format!("{}\n\nUser: {}", SYSTEM_PROMPT, trimmed)
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
        pin_mut!(stream);

        let mut final_message = None;
        let mut thinking_started = false;
        let mut content_started = false;
        while let Some(chunk) = stream.next().await {
            match chunk? {
                StreamChunk::Thinking(thought) => {
                    if !thinking_started {
                        eprintln!("{}", "--- Thinking ---".yellow());
                        thinking_started = true;
                    }
                    eprint!("{}", thought.dimmed());
                }
                StreamChunk::Content(text) => {
                    if !content_started {
                        if thinking_started {
                            eprintln!("\n{}", "--- End of thinking ---".yellow());
                        }
                        println!("{}", "--- Response ---".green());
                        content_started = true;
                    }
                    print!("{}", text.bright_white());
                }
                StreamChunk::Message(msg) => {
                    if thinking_started && !content_started {
                        eprintln!("\n{}", "--- End of thinking ---".yellow());
                    }
                    final_message = Some(msg);
                    println!(); // newline after content
                }
            }
        }

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
                match tools::execute_tool(&tool_name, &full_arg).await {
                    Ok(output) => results.push(format!("TOOL RESULT for {}:\n{}", tool_name, output)),
                    Err(e) => results.push(format!("TOOL {} failed: {}", tool_name, e)),
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
            pin_mut!(stream2);

            let mut final_msg2 = None;
            let mut thinking_started = false;
            let mut content_started = false;
            while let Some(chunk) = stream2.next().await {
                match chunk? {
                    StreamChunk::Thinking(thought) => {
                        if !thinking_started {
                            eprintln!("{}", "--- Thinking ---".yellow());
                            thinking_started = true;
                        }
                        eprint!("{}", thought.dimmed());
                    }
                    StreamChunk::Content(text) => {
                        if !content_started {
                            if thinking_started {
                                eprintln!("\n{}", "--- End of thinking ---".yellow());
                            }
                            println!("{}", "--- Response ---".green());
                            content_started = true;
                        }
                        print!("{}", text.bright_white());
                        io::stdout().flush().await?;
                    io::stdout().flush().await?;
                    }
                    StreamChunk::Message(msg) => {
                        if thinking_started && !content_started {
                            eprintln!("\n{}", "--- End of thinking ---".yellow());
                        }
                        final_msg2 = Some(msg);
                        println!();
                    }
                }
            }

            if let Some(msg) = final_msg2 {
                current_msg = msg;
                parent_id = current_msg.message_id;
            } else {
                break;
            }
        }
    }

    Ok(())
}
