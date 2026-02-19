use anyhow::Result;
use deepseek_api::{DeepSeekAPI, StreamChunk, models::Message};
use futures_util::{pin_mut, StreamExt, Stream};
use tokio::io::{AsyncBufReadExt, BufReader};
use std::env;
use std::io::Write;
mod tools;
use tools::SYSTEM_PROMPT;
use colored::*;

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
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        print!("{}", "> ".cyan().bold());
        std::io::stdout().flush()?;
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
            if let Some(msg) = handle_stream(stream2).await? {
                current_msg = msg;
                parent_id = current_msg.message_id;
            } else {
                break;
            }
        }
    }

    Ok(())
}
