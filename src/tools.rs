use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::LazyLock;
use tokio::fs;
use tokio::process::Command;

// Tool handler: a function that takes a string argument and returns a boxed future.
// We use a trait object to allow closures.
type ToolHandler = Box<dyn for<'a> Fn(&'a str) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> + Send + Sync>;

struct Tool {
    description: &'static str,
    handler: ToolHandler,
}

// Tool implementations (async functions that take &str and return Result<String>)
async fn list_files_handler(arg: &str) -> Result<String> {
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

async fn read_file_handler(arg: &str) -> Result<String> {
    let content = fs::read_to_string(arg).await?;
    Ok(content)
}

async fn create_directory_handler(arg: &str) -> Result<String> {
    fs::create_dir_all(arg).await?;
    Ok(format!("Directory created: {}", arg))
}

async fn apply_search_replace_handler(arg: &str) -> Result<String> {
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

async fn run_command_handler(arg: &str) -> Result<String> {
    let output = Command::new("sh").arg("-c").arg(arg).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    let mut result = format!("EXIT_CODE:{}\n", exit_code);
    if !stdout.is_empty() {
        result.push_str("stdout:\n");
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !stdout.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str("stderr:\n");
        result.push_str(&stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        result.push_str("Command executed successfully (no output)");
    }
    Ok(result)
}

// Registry of all available tools
static TOOLS: LazyLock<HashMap<&'static str, Tool>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert(
        "list_files",
        Tool {
            description: "list_files <directory> : lists all files and directories in the given directory (non‑recursive)",
            handler: Box::new(|s| Box::pin(list_files_handler(s))),
        },
    );
    m.insert(
        "read_file",
        Tool {
            description: "read_file <file_path> : outputs the text contents of a file",
            handler: Box::new(|s| Box::pin(read_file_handler(s))),
        },
    );
    m.insert(
        "create_directory",
        Tool {
            description: "create_directory <dir> : creates a directory (and any missing parents)",
            handler: Box::new(|s| Box::pin(create_directory_handler(s))),
        },
    );
    m.insert(
        "apply_search_replace",
        Tool {
            description: "apply_search_replace <file_path> : applies one or more search/replace blocks to a file.\n  The blocks must be placed on the lines following the tool line, using the markers:\n      <<<<<<< SEARCH\n      (text to search for)\n      =======\n      (replacement text)\n      >>>>>>> REPLACE\n  Multiple blocks can be concatenated; each will be applied sequentially.\n  The search must match exactly, including whitespace and indentation.",
            handler: Box::new(|s| Box::pin(apply_search_replace_handler(s))),
        },
    );
    m.insert(
        "run_command",
        Tool {
            description: "run_command <command_string> : runs a shell command (using sh -c) and returns its stdout/stderr. Use with caution.",
            handler: Box::new(|s| Box::pin(run_command_handler(s))),
        },
    );
    m
});

// Build the system prompt dynamically from the tool registry
pub static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    let header = "You are an assistant that can use the following tools to interact with the current directory.\nTo use a tool, output a line starting with \"TOOL:\" followed by the tool name and its argument(s). For tools that require multiple pieces of data, the argument(s) may span multiple lines.\nYou can include multiple tool invocations in one response; they will be executed sequentially.\n\nIMPORTANT: Do NOT simulate or guess the tool results. Only output the tool invocations. After you output them, you will receive a new message containing the actual results (each prefixed with \"TOOL RESULT for <tool>:\"). Then you can continue the conversation based on those real results. Never include your own interpretation of what the tool would return; let the system provide the results.\n\nAdditionally, this project uses a DEEPSEEK.md file to store project context. You should read it at the start of each session (it is automatically injected). When you make changes to the project (e.g., add features, modify conventions), please update DEEPSEEK.md accordingly to keep the context accurate for future sessions.\n\nAvailable tools:\n\n";
    let mut tool_lines: Vec<String> = TOOLS
        .iter()
        .map(|(name, tool)| format!("- {} : {}", name, tool.description))
        .collect();
    tool_lines.sort(); // consistent order
    header.to_string() + &tool_lines.join("\n")
});

pub async fn execute_tool(name: &str, arg: &str) -> Result<String> {
    match TOOLS.get(name) {
        Some(tool) => (tool.handler)(arg).await,
        None => anyhow::bail!("Unknown tool: {}", name),
    }
}
