use anyhow::Result;
use tokio::fs;
use std::path::Path;

pub const SYSTEM_PROMPT: &str = r#"You are an assistant that can use the following tools to interact with the current directory.
To use a tool, output a line starting with "TOOL:" followed by the tool name and its single argument.
Available tools:
- list_files <directory>   : lists all files and directories in the given directory (non‑recursive)
- read_file <file_path>    : outputs the text contents of a file
- create_directory <dir>   : creates a directory (and any missing parents)

After using a tool, you will receive the result in the next user message, prefixed with "TOOL RESULT:".
You can then continue the conversation or use another tool.
When you have the final answer, just output it normally without any "TOOL:" line.
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
        _ => anyhow::bail!("Unknown tool: {}", name),
    }
}