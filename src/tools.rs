use anyhow::{Result, anyhow};
use tokio::fs;
use tokio::process::Command;
use std::path::Path;

pub const SYSTEM_PROMPT: &str = r#"You are an assistant that can use the following tools to interact with the current directory.
To use a tool, output a line starting with "TOOL:" followed by the tool name and its argument(s). For tools that require multiple pieces of data, the argument must be a JSON string. Available tools:

- list_files <directory>                         : lists all files and directories in the given directory (non‑recursive)
- read_file <file_path>                           : outputs the text contents of a file
- create_directory <dir>                           : creates a directory (and any missing parents)
- edit_file <json>                                 : applies one or more search/replace blocks to a file. The JSON must have the format:
    {"file": "<file_path>", "blocks": [{"search": "...", "replace": "..."}, ...]}
    Each block will be applied sequentially to the current file content. The search must match exactly.
- run_command <command_string>                     : runs a shell command (using sh -c) and returns its stdout/stderr. Use with caution.

After using a tool, you will receive the result in the next user message, prefixed with "TOOL RESULT:".
You can then continue the conversation or use another tool.
When you have the final answer, just output it normally without any "TOOL:" line.
"#;

#[derive(serde::Deserialize)]
struct EditFileArgs {
    file: String,
    blocks: Vec<Block>,
}

#[derive(serde::Deserialize)]
struct Block {
    search: String,
    replace: String,
}

pub async fn execute_tool(name: &str, arg: &str) -> Result<String> {
    match name {
        "list_files" => {
            let path = Path::new(arg);
            if !path.is_dir() {
                anyhow::bail!("Not a directory: {}", arg);
            }
            let mut entries = fs::read_dir(path).await?;
            let mut names = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
            names.sort();
            Ok(names.join("\n"))
        }
        "read_file" => {
            let content = fs::read_to_string(arg).await?;
            Ok(content)
        }
        "create_directory" => {
            fs::create_dir_all(arg).await?;
            Ok(format!("Directory created: {}", arg))
        }
        "edit_file" => {
            let args: EditFileArgs = serde_json::from_str(arg)
                .map_err(|e| anyhow!("Invalid JSON for edit_file: {}", e))?;
            let mut content = fs::read_to_string(&args.file).await?;
            for block in &args.blocks {
                if !content.contains(&block.search) {
                    anyhow::bail!("Search string not found in {}: {:?}", args.file, block.search);
                }
                content = content.replace(&block.search, &block.replace);
            }
            fs::write(&args.file, &content).await?;
            Ok(format!("Applied {} block(s) to {}", args.blocks.len(), args.file))
        }
        "run_command" => {
            let output = Command::new("sh")
                .arg("-c")
                .arg(arg)
                .output()
                .await?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str("stdout:\n");
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push_str("\n\n");
                }
                result.push_str("stderr:\n");
                result.push_str(&stderr);
            }
            if result.is_empty() {
                result = "Command executed successfully (no output)".to_string();
            }
            Ok(result)
        }
        _ => anyhow::bail!("Unknown tool: {}", name),
    }
}