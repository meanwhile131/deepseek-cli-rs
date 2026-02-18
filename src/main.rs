use anyhow::Result;
use deepseek_api::{DeepSeekAPI, StreamChunk};
use futures_util::{pin_mut, StreamExt};
use std::env;
use tokio::io::{self, AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    let token = env::var("DEEPSEEK_TOKEN")
        .expect("DEEPSEEK_TOKEN environment variable must be set");

    let api = DeepSeekAPI::new(token).await?;
    let chat = api.create_chat().await?;
    let chat_id = chat.id.clone();
    println!("Chat created with ID: {}", chat_id);
    println!("Start typing your messages (type '/exit' to quit):");

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

        println!("Sending...");
        let stream = api.complete_stream(
            chat_id.clone(),
            trimmed.to_string(),
            parent_id,
            false, // search
            false, // thinking
        );
        pin_mut!(stream);

        let mut final_message = None;
        while let Some(chunk) = stream.next().await {
            match chunk? {
                StreamChunk::Content(text) => {
                    print!("{}", text);
                }
                StreamChunk::Thinking(thought) => {
                    eprint!("\n[thinking] {}\n", thought);
                }
                StreamChunk::Message(msg) => {
                    final_message = Some(msg);
                    println!(); // newline after content
                }
            }
        }

        if let Some(msg) = final_message {
            parent_id = msg.message_id;
        } else {
            eprintln!("No final message received");
        }
    }

    Ok(())
}
