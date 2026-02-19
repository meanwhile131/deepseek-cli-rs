use anyhow::Result;
use deepseek_api::{DeepSeekAPI, StreamChunk};
use futures_util::{pin_mut, StreamExt};
use std::env;
use tokio::io::{self, AsyncBufReadExt, BufReader};

mod tools;
use tools::SYSTEM_PROMPT;

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

    while let Some(line) = lines.next_line().await? {
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
        while let Some(chunk) = stream.next().await {
            match chunk? {
                StreamChunk::Content(text) => print!("{}", text),
                StreamChunk::Thinking(thought) => eprint!("\n[thinking] {}\n", thought),
                StreamChunk::Message(msg) => {
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
        let mut tool_iterations = 0;
        const MAX_TOOL_ITER: usize = 5;
        while tool_iterations < MAX_TOOL_ITER {
            let tool_lines: Vec<&str> = current_msg.content
                .lines()
                .filter(|l| l.trim().starts_with("TOOL:"))
                .collect();

            if tool_lines.is_empty() {
                break;
            }

            // Execute all requested tools
            let mut results = Vec::new();
            for line in tool_lines {
                let line = line.trim();
                let parts: Vec<&str> = line.splitn(3, ' ').collect();
                if parts.len() < 3 {
                    results.push(format!("Error: Invalid tool line: '{}'", line));
                    continue;
                }
                let tool_name = parts[1];
                let arg = parts[2];
                match tools::execute_tool(tool_name, arg).await {
                    Ok(output) => results.push(format!("TOOL RESULT for {} {}:\n{}", tool_name, arg, output)),
                    Err(e) => results.push(format!("TOOL {} {} failed: {}", tool_name, arg, e)),
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
            while let Some(chunk) = stream2.next().await {
                match chunk? {
                    StreamChunk::Content(text) => print!("{}", text),
                    StreamChunk::Thinking(thought) => eprint!("\n[thinking] {}\n", thought),
                    StreamChunk::Message(msg) => {
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
            tool_iterations += 1;
        }

        if tool_iterations == MAX_TOOL_ITER {
            eprintln!("Reached maximum tool iterations.");
        }
    }

    Ok(())
}
