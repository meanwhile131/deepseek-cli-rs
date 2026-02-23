use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::LazyLock;
use tokio::fs;
use tokio::process::Command;
use scraper::{Html, Selector};
use urlencoding::encode;

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
        anyhow::bail!("Not a directory: {arg}");
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
    Ok(format!("Directory created: {arg}"))
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
            anyhow::bail!("Search string not found in {file_path}: {search:?}");
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
    #[cfg(windows)]
    let output = Command::new("cmd")
        .args(&["/c", arg])
        .output()
        .await?;
    #[cfg(not(windows))]
    let output = Command::new("sh")
        .args(["-c", arg])
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    let mut result = format!("EXIT_CODE:{exit_code}\n");
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

async fn write_file_handler(arg: &str) -> Result<String> {
    // Split at first newline: first line is file path, rest is content
    let mut lines = arg.lines();
    let file_path = lines
        .next()
        .ok_or_else(|| anyhow!("Missing file path"))?
        .to_string();
    let content: String = lines.collect::<Vec<&str>>().join("\n");

    // Ensure parent directory exists
    if let Some(parent) = Path::new(&file_path).parent() {
        fs::create_dir_all(parent).await?;
    }

    fs::write(&file_path, &content).await?;
    Ok(format!("File written: {file_path}"))
}

// Registry of all available tools
async fn fetch_url_handler(arg: &str) -> Result<String> {
    let url = arg.trim();
    if url.is_empty() {
        anyhow::bail!("URL cannot be empty");
    }
    let response = reqwest::get(url).await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("HTTP error {status}: {url}");
    }
    let text = response.text().await?;
    Ok(text)
}

async fn search_web_handler(arg: &str) -> Result<String> {
    let query = arg.trim();
    if query.is_empty() {
        anyhow::bail!("Search query cannot be empty");
    }
    let encoded = encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded}");
    
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
        .build()
        .map_err(|e| anyhow!("Failed to create HTTP client: {e}"))?;
    
    let response = client.get(&url).send().await
        .map_err(|e| anyhow!("Network error while searching: {e}"))?;
    let status = response.status();
    let html = response.text().await
        .map_err(|e| anyhow!("Failed to read response body: {e}"))?;
    
    if !status.is_success() {
        let lower = html.to_lowercase();
        if lower.contains("captcha") || lower.contains("unusual traffic") || lower.contains("blocked") {
            anyhow::bail!("Search engine is blocking the request (possible CAPTCHA or rate limiting). Please try again later.");
        }
        anyhow::bail!("HTTP error {status} while searching");
    }
    
    let document = Html::parse_document(&html);
    let result_selector = Selector::parse("div.result")
        .map_err(|e| anyhow!("Invalid result selector: {e}"))?;
    let title_selector = Selector::parse("a.result__a")
        .map_err(|e| anyhow!("Invalid title selector: {e}"))?;
    let url_selector = Selector::parse("a.result__a")
        .map_err(|e| anyhow!("Invalid URL selector: {e}"))?;
    let snippet_selector = Selector::parse("a.result__snippet")
        .map_err(|e| anyhow!("Invalid snippet selector: {e}"))?;
    
    let base_url = reqwest::Url::parse(&url)
        .map_err(|e| anyhow!("Invalid base URL: {e}"))?;
    let mut results = Vec::new();
    for result in document.select(&result_selector) {
        let title_elem = result.select(&title_selector).next();
        let url_elem = result.select(&url_selector).next();
        let snippet_elem = result.select(&snippet_selector).next();
        
        let title = title_elem.map(|e| e.text().collect::<String>()).unwrap_or_default();
        let href = url_elem.and_then(|e| e.value().attr("href")).unwrap_or("");
        let absolute_url = base_url.join(href)
            .ok()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let snippet = snippet_elem.map(|e| e.text().collect::<String>()).unwrap_or_default();
        
        if !title.is_empty() && !absolute_url.is_empty() {
            results.push(format!("Title: {}\nURL: {}\nSnippet: {}\n---", title.trim(), absolute_url, snippet.trim()));
        }
    }
    
    if results.is_empty() {
        if html.contains("No results") || html.contains("no results found") {
            Ok("No results found for the query.".to_string())
        } else {
            Ok("No results could be extracted from the search page. The page structure may have changed.".to_string())
        }
    } else {
        Ok(results.join("\n"))
    }
}

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
            description: "run_command <command_string> : runs a shell command using the system's default shell and returns its stdout/stderr. Use with caution.",
            handler: Box::new(|s| Box::pin(run_command_handler(s))),
        },
    );
    m.insert(
        "write_file",
        Tool {
            description: "write_file <file_path> : writes the provided content to the file, creating any necessary parent directories. If the file exists, it is overwritten. The content should follow the file path on subsequent lines.",
            handler: Box::new(|s| Box::pin(write_file_handler(s))),
        },
    );
    m.insert(
        "search_web",
        Tool {
            description: "search_web <query> : performs a web search using DuckDuckGo and returns a list of results with titles, URLs, and snippets.",
            handler: Box::new(|s| Box::pin(search_web_handler(s))),
        },
    );
    m.insert(
        "fetch_url",
        Tool {
            description: "fetch_url <url> : fetches the content from the given URL and returns it as text (HTML, JSON, etc.). Useful for browsing the internet for information.",
            handler: Box::new(|s| Box::pin(fetch_url_handler(s))),
        },
    );
    m
});

// Build the system prompt dynamically from the tool registry
pub static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    let header = r#"You are an assistant that can use the following tools to interact with the current directory.
To use a tool, output a line starting with "TOOL:" followed by the tool name and its argument(s). For tools that require multiple pieces of data, the argument(s) may span multiple lines.
You can include multiple tool invocations in one response; they will be executed sequentially.

IMPORTANT: Do NOT simulate or guess the tool results. Only output the tool invocations. After you output them, you will receive a new message containing the actual results (each prefixed with "TOOL RESULT for <tool>:"). Then you can continue the conversation based on those real results. Never include your own interpretation of what the tool would return; let the system provide the results.

Workflow: Your primary task is to assist the user by providing accurate and helpful information. To achieve this, you should first determine if you need to interact with the environment. If so, output one or more tool calls (each starting with `TOOL:`) to gather the necessary data. After the tool results are returned, you can then analyze them and formulate your final answer. Do not attempt to answer questions that require external data without first using the appropriate tools.

**Important: Always prioritize retrieving up‑to‑date information.** When answering questions about software versions, libraries, commands, or any technical details that may change over time (e.g., latest releases, current documentation, API changes), use the `search_web` or `fetch_url` tools to obtain current information from official sources, package registries, or documentation sites. Do not rely solely on your internal knowledge, as it may be outdated. If you need to suggest a command or tool, verify its existence or proper usage via search before proposing it.

Additional tool usage guidelines:
- For `run_command`, provide the command as a plain string without extra quoting. The tool passes it directly to the system's default shell. If the command contains spaces or special characters, write it naturally; the shell will handle it. For multi-step commands, chain them with `&&` or `;` within the same string, but be mindful of quoting inside the command (e.g., use single quotes inside the string if needed).
- Before suggesting a command that requires specific dependencies (like `cargo` or `podman`), first check if they exist using `which` or `--version` to provide actionable feedback. If the environment lacks a tool, suggest installation steps rather than assuming it's present.
- When a tool returns an error (e.g., command not found), interpret it and suggest corrective actions, not just repeat the command. Use the results of `run_command` to decide next steps (e.g., if `cargo check` fails, report the error; if it succeeds, proceed).
- Always include the exact tool line as specified, with no extra commentary before it. The tool invocation must be the first thing on its own line starting with `TOOL:`.
- If multiple tool calls are needed, list them sequentially; do not simulate results.
- For complex commands that include quotes, remember that the tool passes the string directly to the system's default shell. If the command itself contains quotes, use a mix of single and double quotes appropriately. For example, to run `echo 'Hello World'`, write `run_command echo 'Hello World'`. The outer quotes are not needed because the tool does not add them.

Available tools:

"#;
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
        None => anyhow::bail!("Unknown tool: {name}"),
    }
}
