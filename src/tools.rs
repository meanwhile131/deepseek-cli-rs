use anyhow::{Result, anyhow};
use std::path::Path;
use tokio::fs;
use tokio::process::Command;

pub const SYSTEM_PROMPT: &str = r#"You are an assistant that can use the following tools to interact with the current directory.
To use a tool, output a line starting with "TOOL:" followed by the tool name and its argument(s). For tools that require multiple pieces of data, the argument(s) may span multiple lines. Available tools:

- list_files <directory>                         : lists all files and directories in the given directory (non‑recursive)
- read_file <file_path>                           : outputs the text contents of a file
- create_directory <dir>                           : creates a directory (and any missing parents)
- apply_search_replace <file_path>                  : applies one or more search/replace blocks to a file.
  The blocks must be placed on the lines following the tool line, using the markers:
      <<<<<<< SEARCH
      (text to search for)
      =======
      (replacement text)
      >>>>>>> REPLACE
  Multiple blocks can be concatenated; each will be applied sequentially.
  The search must match exactly, including whitespace and indentation.
- run_command <command_string>                     : runs a shell command (using sh -c) and returns its stdout/stderr. Use with caution.

You can request multiple tools in one response by starting each with "TOOL:" on its own line. After using tools, you will receive the result(s) in the next user message. Then you can either request more tools or exit.
To exit the tool loop and prompt the user for a message, you must output a line containing exactly "/exit" or "exit". That response mustn't contain tool calls.
"#;

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
        "apply_search_replace" => {
            // Split the argument into lines: first line is the file path, rest are the block(s)
            let mut lines = arg.lines();
            let file_path = lines
                .next()
                .ok_or_else(|| anyhow!("Missing file path"))?
                .to_string();
            let block_text: String = lines.collect::<Vec<&str>>().join("\n");

            // Parse blocks from block_text using the markers
            let mut blocks = Vec::new();
            let mut remaining = block_text.as_str();
            while let Some(search_start) = remaining.find("<<<<<<< SEARCH") {
                let after_search = &remaining[search_start + 15..]; // length of "<<<<<<< SEARCH"
                let search_end = after_search
                    .find("=======")
                    .ok_or_else(|| anyhow!("Missing ======="))?;
                let search = after_search[..search_end].trim().to_string();

                let after_eq = &after_search[search_end + 7..]; // length of "======="
                let replace_end = after_eq
                    .find(">>>>>>> REPLACE")
                    .ok_or_else(|| anyhow!("Missing >>>>>>> REPLACE"))?;
                let replace = after_eq[..replace_end].trim().to_string();

                blocks.push((search, replace));
                remaining = &after_eq[replace_end + 15..]; // length of ">>>>>>> REPLACE"
            }

            if blocks.is_empty() {
                anyhow::bail!("No valid search/replace blocks found");
            }

            let mut content = fs::read_to_string(&file_path).await?;
            for (search, replace) in &blocks {
                if !content.contains(search) {
                    anyhow::bail!("Search string not found in {}: {:?}", file_path, search);
                }
                content = content.replace(search, replace);
            }
            fs::write(&file_path, &content).await?;
            Ok(format!(
                "Applied {} block(s) to {}",
                blocks.len(),
                file_path
            ))
        }
        "run_command" => {
            let output = Command::new("sh").arg("-c").arg(arg).output().await?;
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
