use anyhow::{Result, anyhow};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt;
use once_cell::sync::OnceCell;
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use std::io::Write;
use std::process::Stdio;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use urlencoding::encode;

/// Check if a path should be ignored based on .gitignore
fn is_gitignored(path: &Path, git_root: &Path) -> Result<bool> {
    let gitignore = git_root.join(".gitignore");
    if !gitignore.exists() {
        return Ok(false);
    }

    let gitignore_content = std::fs::read_to_string(&gitignore)?;
    let relative_path = path.strip_prefix(git_root).map_or_else(
        |_| path.to_string_lossy().to_string(),
        |p| p.to_string_lossy().to_string(),
    );

    for line in gitignore_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let pattern = line.trim_start_matches('/');
        // Simple glob matching: handle ** and *
        if pattern.contains("**") {
            let parts: Vec<&str> = pattern.split("**").collect();
            if parts.len() == 2 {
                let prefix = parts[0].trim_end_matches('/');
                let suffix = parts[1].trim_start_matches('/');
                if (prefix.is_empty() || relative_path.starts_with(prefix))
                    && (suffix.is_empty() || relative_path.ends_with(suffix))
                {
                    return Ok(true);
                }
            }
        } else if pattern.contains('*') {
            // Simple wildcard matching
            let regex_pattern = pattern.replace('.', "\\.").replace('*', ".*");
            if let Ok(re) = regex::Regex::new(&format!("^{regex_pattern}$"))
                && re.is_match(&relative_path)
            {
                return Ok(true);
            }
        } else if relative_path == pattern || relative_path.ends_with(&format!("/{pattern}")) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Convert a path to a relative path from the current working directory
fn to_relative_path(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if path == cwd {
        return ".".to_string();
    }
    path.strip_prefix(&cwd).map_or_else(
        |_| path.to_string_lossy().to_string(),
        |p| p.to_string_lossy().to_string(),
    )
}

/// Represents the result of executing a tool.
#[derive(Debug)]
pub enum ToolOutput {
    /// Text output that may be uploaded as a file or included in a message.
    Text { content: String, status: String },
    /// Binary data (e.g., screenshot) that should be uploaded as a file.
    Binary {
        data: Vec<u8>,
        mime_type: String,
        status: String,
    },
    /// A reference to an already uploaded file.
    FileReference { file_id: String, status: String },
    /// No content, just a status message.
    StatusOnly { status: String },
}

struct Tool {
    description: &'static str,
    handler: ToolHandler,
}

type ToolHandler = Box<dyn for<'a> Fn(&'a str) -> ToolFuture<'a> + Send + Sync>;

type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>>;

async fn list_files_handler(arg: &str) -> Result<ToolOutput> {
    if arg.contains('\n') {
        anyhow::bail!("list_files: path argument must be on a single line (no newlines)");
    }
    let path = Path::new(arg);
    let display_path = to_relative_path(path);
    if !path.is_dir() {
        anyhow::bail!("Not a directory: {display_path}");
    }

    let mut entries = fs::read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            // Include all files, including hidden and gitignored ones
            names.push(name.to_string());
        }
    }
    names.sort();
    if names.is_empty() {
        Ok(ToolOutput::StatusOnly {
            status: format!("No files found in {display_path}"),
        })
    } else {
        let content = names.join("\n");
        let status = format!("Listed {} files in {}", names.len(), display_path);
        Ok(ToolOutput::Text { content, status })
    }
}

async fn read_file_handler(arg: &str) -> Result<ToolOutput> {
    // Parse arguments: path [start_line] [end_line]
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!("read_file: missing file path");
    }
    let path_str = parts[0];
    let mut start_line: Option<usize> = None;
    let mut end_line: Option<usize> = None;
    if parts.len() > 1 {
        start_line = parts[1].parse().ok();
    }
    if parts.len() > 2 {
        end_line = parts[2].parse().ok();
    }

    let path = Path::new(path_str);
    let display_path = to_relative_path(path);
    let full_content = fs::read_to_string(path).await?;

    // Split into lines
    let lines: Vec<&str> = full_content.lines().collect();
    let total_lines = lines.len();

    // Handle empty file: ignore line arguments and return empty content
    if total_lines == 0 {
        return Ok(ToolOutput::StatusOnly {
            status: format!("File is empty: {display_path}"),
        });
    }

    // Determine line range (1-indexed inclusive)
    let (start_idx, end_idx) = match (start_line, end_line) {
        (Some(s), Some(e)) => {
            if s == 0 || e == 0 {
                anyhow::bail!("Line numbers must be positive (1-indexed)");
            }
            if s > e {
                anyhow::bail!("Start line {s} is greater than end line {e}");
            }
            if s > total_lines {
                anyhow::bail!("Start line {s} exceeds total lines {total_lines}");
            }
            let start = s - 1;
            let end = e.min(total_lines);
            (start, end)
        }
        (Some(s), None) => {
            if s == 0 {
                anyhow::bail!("Start line must be positive (1-indexed)");
            }
            if s > total_lines {
                anyhow::bail!("Start line {s} exceeds total lines {total_lines}");
            }
            (s - 1, total_lines)
        }
        (None, Some(_e)) => {
            anyhow::bail!("End line provided without start line");
        }
        (None, None) => (0, total_lines),
    };

    if start_idx >= total_lines {
        // Already handled above, but safety
        anyhow::bail!("Start line out of range");
    }

    let selected_lines = &lines[start_idx..end_idx];
    let content = selected_lines.join("\n");

    if content.is_empty() {
        Ok(ToolOutput::StatusOnly {
            status: format!(
                "No lines selected (empty range) in {display_path} (lines {}-{})",
                start_idx + 1,
                end_idx
            ),
        })
    } else {
        let status = format!(
            "Read file at {display_path} (lines {}-{} of {})",
            start_idx + 1,
            end_idx,
            total_lines
        );
        Ok(ToolOutput::Text { content, status })
    }
}

async fn create_directory_handler(arg: &str) -> Result<ToolOutput> {
    if arg.contains('\n') {
        anyhow::bail!("create_directory: path argument must be on a single line (no newlines)");
    }
    let path = Path::new(arg);
    let display_path = to_relative_path(path);
    fs::create_dir_all(path).await?;
    let status = format!("Directory created: {display_path}");
    Ok(ToolOutput::StatusOnly { status })
}

async fn apply_search_replace_handler(arg: &str) -> Result<ToolOutput> {
    let mut lines = arg.lines();
    let file_path = lines
        .next()
        .ok_or_else(|| anyhow!("Missing file path"))?
        .to_string();
    let block_text: String = lines.collect::<Vec<&str>>().join("\n");

    let mut blocks = Vec::new();
    let mut remaining = block_text.as_str();
    while let Some(search_start) = remaining.find("<<<<<<< SEARCH") {
        let after_search = &remaining[search_start + 15..];
        let search_end = after_search
            .find("=======")
            .ok_or_else(|| anyhow!("Missing ======="))?;
        let search = after_search[..search_end].trim().to_string();

        let after_eq = &after_search[search_end + 7..];
        let replace_end = after_eq
            .find(">>>>>>> REPLACE")
            .ok_or_else(|| anyhow!("Missing >>>>>>> REPLACE"))?;
        let replace = after_eq[..replace_end].trim().to_string();

        blocks.push((search, replace));
        remaining = &after_eq[replace_end + 15..];
    }

    if blocks.is_empty() {
        anyhow::bail!("No valid search/replace blocks found");
    }

    let file_path_buf = PathBuf::from(&file_path);
    let display_path = to_relative_path(&file_path_buf);
    let mut content = fs::read_to_string(&file_path).await?;
    for (search, replace) in &blocks {
        if !content.contains(search) {
            anyhow::bail!("Search string not found in {display_path}: {search:?}");
        }
        content = content.replace(search, replace);
    }
    fs::write(&file_path, &content).await?;
    let status = format!("Applied {} block(s) to {}", blocks.len(), display_path);
    Ok(ToolOutput::StatusOnly { status })
}

async fn run_command_handler(arg: &str) -> Result<ToolOutput> {
    let timeout_duration = Duration::from_secs(300); // 5 minutes

    #[cfg(windows)]
    let mut cmd = Command::new("cmd");
    #[cfg(not(windows))]
    let mut cmd = Command::new("sh");

    #[cfg(windows)]
    let cmd = cmd.args(&["/c", arg]);
    #[cfg(not(windows))]
    let cmd = cmd.args(["-c", arg]);

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .env("PYTHONUNBUFFERED", "1")
        .spawn()?;

    let (stdout_lines, stderr_lines, exit_code) = read_command_output(&mut child, timeout_duration).await?;

    let result = format_command_output(&stdout_lines, &stderr_lines);
    let status = if exit_code == 0 {
        "Command succeeded (exit code: 0)".to_string()
    } else {
        format!("Command failed (exit code: {exit_code})")
    };
    Ok(ToolOutput::Text { content: result, status })
}

async fn read_command_output(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<(Vec<String>, Vec<String>, i32)> {
    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut exit_code = 0;

    let read_loop = async {
        loop {
            if stdout_done && stderr_done {
                break;
            }
            tokio::select! {
                line = stdout_reader.next_line(), if !stdout_done => {
                    match line {
                        Ok(Some(l)) => {
                            println!("[stdout] {l}");
                            let _ = std::io::stdout().flush();
                            stdout_lines.push(l);
                        }
                        Ok(None) => { stdout_done = true; }
                        Err(e) => return Err(anyhow!("Error reading stdout: {e}")),
                    }
                }
                line = stderr_reader.next_line(), if !stderr_done => {
                    match line {
                        Ok(Some(l)) => {
                            eprintln!("[stderr] {l}");
                            let _ = std::io::stderr().flush();
                            stderr_lines.push(l);
                        }
                        Ok(None) => { stderr_done = true; }
                        Err(e) => return Err(anyhow!("Error reading stderr: {e}")),
                    }
                }
                result = child.wait() => {
                    let status = result?;
                    exit_code = status.code().unwrap_or(-1);
                    // Continue reading remaining lines; the streams will close naturally.
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    tokio::time::timeout(timeout, read_loop)
        .await
        .map_err(|_| anyhow::anyhow!("Command timed out after {} seconds", timeout.as_secs()))?
        .map_err(|e| anyhow::anyhow!("Command execution failed: {e}"))?;

    Ok((stdout_lines, stderr_lines, exit_code))
}

fn format_command_output(stdout_lines: &[String], stderr_lines: &[String]) -> String {
    let stdout_str = stdout_lines.join("\n");
    let stderr_str = stderr_lines.join("\n");

    let mut result = String::new();
    if !stdout_str.is_empty() {
        result.push_str("stdout:\n");
        result.push_str(&stdout_str);
    }
    if !stderr_str.is_empty() {
        if !stdout_str.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str("stderr:\n");
        result.push_str(&stderr_str);
    }
    if stdout_str.is_empty() && stderr_str.is_empty() {
        result.push_str("Command executed with no output");
    }
    result
}

async fn write_file_handler(arg: &str) -> Result<ToolOutput> {
    let mut lines = arg.lines();
    let file_path = lines
        .next()
        .ok_or_else(|| anyhow!("Missing file path"))?
        .to_string();

    // Collect remaining lines into a vector
    let remaining_lines: Vec<&str> = lines.collect();

    // Find the first line that is exactly "--"
    let delimiter_pos = remaining_lines.iter().position(|&line| line == "--");

    let content = match delimiter_pos {
        Some(pos) => {
            // Content is lines before the delimiter (excluding the delimiter line)
            remaining_lines[..pos].join("\n")
        }
        None => {
            // No delimiter -> use all lines as content
            remaining_lines.join("\n")
        }
    };

    let path_buf = PathBuf::from(&file_path);
    let display_path = to_relative_path(&path_buf);

    if let Some(parent) = path_buf.parent() {
        fs::create_dir_all(parent).await?;
    }

    fs::write(&file_path, &content).await?;
    let status = format!("File written: {display_path}");
    Ok(ToolOutput::StatusOnly { status })
}

async fn fetch_url_handler(arg: &str) -> Result<ToolOutput> {
    let url = arg.trim();
    if url.is_empty() {
        anyhow::bail!("URL cannot be empty");
    }
    let response = reqwest::get(url).await?;
    let status_code = response.status();
    if !status_code.is_success() {
        anyhow::bail!("HTTP error {status_code}: {url}");
    }
    let content = response.text().await?;
    if content.is_empty() {
        let status = format!("Fetched URL (empty response): {url}");
        Ok(ToolOutput::StatusOnly { status })
    } else {
        let size = content.len();
        let status = format!("Fetched URL: {url} ({size} bytes)");
        Ok(ToolOutput::Text { content, status })
    }
}

async fn search_web_handler(arg: &str) -> Result<ToolOutput> {
    let query = arg.trim();
    if query.is_empty() {
        anyhow::bail!("Search query cannot be empty");
    }
    let encoded = encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded}");

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| anyhow!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("Network error while searching: {e}"))?;
    let status_code = response.status();
    let html = response
        .text()
        .await
        .map_err(|e| anyhow!("Failed to read response body: {e}"))?;

    if !status_code.is_success() {
        let lower = html.to_lowercase();
        if lower.contains("anomaly-modal") {
            anyhow::bail!("Search engine is blocking the request. Please try again later.");
        }
        anyhow::bail!("HTTP error {status_code} while searching");
    }

    let document = Html::parse_document(&html);
    let result_selector =
        Selector::parse("div.result").map_err(|e| anyhow!("Invalid result selector: {e}"))?;
    let title_selector =
        Selector::parse("a.result__a").map_err(|e| anyhow!("Invalid title selector: {e}"))?;
    let url_selector =
        Selector::parse("a.result__a").map_err(|e| anyhow!("Invalid URL selector: {e}"))?;
    let snippet_selector = Selector::parse("a.result__snippet")
        .map_err(|e| anyhow!("Invalid snippet selector: {e}"))?;

    let base_url = reqwest::Url::parse(&url).map_err(|e| anyhow!("Invalid base URL: {e}"))?;
    let mut results = Vec::new();
    for result in document.select(&result_selector) {
        let title_elem = result.select(&title_selector).next();
        let url_elem = result.select(&url_selector).next();
        let snippet_elem = result.select(&snippet_selector).next();

        let title = title_elem
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        let href = url_elem.and_then(|e| e.value().attr("href")).unwrap_or("");
        let absolute_url = base_url
            .join(href)
            .ok()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let snippet = snippet_elem
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();

        if !title.is_empty() && !absolute_url.is_empty() {
            results.push(format!(
                "Title: {}\nURL: {}\nSnippet: {}\n---",
                title.trim(),
                absolute_url,
                snippet.trim()
            ));
        }
    }

    let content = if results.is_empty() {
        if html.contains("No results") || html.contains("no results found") {
            "No results found for the query.".to_string()
        } else {
            "No results could be extracted from the search page. The page structure may have changed.".to_string()
        }
    } else {
        results.join("\n")
    };
    let status = if results.is_empty() {
        "Executed tool: search_web - found 0 results".to_string()
    } else {
        format!(
            "Executed tool: search_web - found {} results",
            results.len()
        )
    };
    Ok(ToolOutput::Text { content, status })
}

// ============================================================================
// Git Integration Tools
// ============================================================================

/// Find the git root directory from a starting path
fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    let mut current = start_path.to_path_buf();
    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

async fn git_status_handler(arg: &str) -> Result<ToolOutput> {
    let working_dir = if arg.trim().is_empty() {
        std::env::current_dir()?
    } else {
        Path::new(arg.trim()).to_path_buf()
    };

    let git_root = find_git_root(&working_dir)
        .ok_or_else(|| anyhow!("Not a git repository: {}", working_dir.display()))?;

    let display_git_root = to_relative_path(&git_root);

    let output = Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(&git_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("git status failed: {stderr}");
    }

    let content = if stdout.is_empty() {
        "Working tree clean".to_string()
    } else {
        stdout.to_string()
    };

    let status = format!("Git status for {display_git_root}");
    Ok(ToolOutput::Text { content, status })
}

async fn git_diff_handler(arg: &str) -> Result<ToolOutput> {
    let args: Vec<&str> = arg.split_whitespace().collect();
    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).ok_or_else(|| anyhow!("Not a git repository"))?;

    let mut cmd = Command::new("git");
    cmd.arg("diff").current_dir(&git_root);

    // Parse optional args: --staged, --cached, or a commit range
    for a in args {
        if a.starts_with('-') || a.contains("..") {
            cmd.arg(a);
        }
    }

    let output = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("git diff failed: {stderr}");
    }

    let content = if stdout.is_empty() {
        "No changes".to_string()
    } else {
        stdout.to_string()
    };

    let status = "Git diff generated".to_string();
    Ok(ToolOutput::Text { content, status })
}

async fn git_log_handler(arg: &str) -> Result<ToolOutput> {
    let limit = arg.trim().parse::<usize>().unwrap_or(10);
    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).ok_or_else(|| anyhow!("Not a git repository"))?;

    let output = Command::new("git")
        .args([
            "log",
            &format!("-{limit}"),
            "--pretty=format:%h - %s (%an, %ar)",
        ])
        .current_dir(&git_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("git log failed: {stderr}");
    }

    if stdout.is_empty() {
        Ok(ToolOutput::StatusOnly {
            status: format!("No commits found (limit {limit})"),
        })
    } else {
        let status = format!("Showed last {} commits", stdout.lines().count());
        Ok(ToolOutput::Text {
            content: stdout.to_string(),
            status,
        })
    }
}

async fn git_commit_handler(arg: &str) -> Result<ToolOutput> {
    let raw_message = arg.trim();
    let message =
        if raw_message.starts_with('"') && raw_message.ends_with('"') && raw_message.len() >= 2 {
            &raw_message[1..raw_message.len() - 1]
        } else {
            raw_message
        };
    if message.is_empty() {
        anyhow::bail!("Commit message cannot be empty");
    }

    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).ok_or_else(|| anyhow!("Not a git repository"))?;

    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(&git_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("git commit failed: {stderr}");
    }

    let status = format!("Committed: {message}");
    Ok(ToolOutput::StatusOnly {
        status: format!("{status}\n{}", stdout.trim()),
    })
}

async fn git_add_handler(arg: &str) -> Result<ToolOutput> {
    let pathspec = arg.trim();
    if pathspec.is_empty() {
        anyhow::bail!("Pathspec cannot be empty. Use '.' for all changes.");
    }

    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).ok_or_else(|| anyhow!("Not a git repository"))?;

    let output = Command::new("git")
        .args(["add", pathspec])
        .current_dir(&git_root)
        .output()
        .await?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("git add failed: {stderr}");
    }

    let status = format!("Staged: {pathspec}");
    Ok(ToolOutput::StatusOnly { status })
}

// ============================================================================
// Codebase Search Tool (ripgrep)
// ============================================================================

async fn search_codebase_handler(arg: &str) -> Result<ToolOutput> {
    let parts: Vec<&str> = arg.trim().splitn(2, ' ').collect();
    let pattern = parts
        .first()
        .ok_or_else(|| anyhow!("Search pattern required"))?;
    let mut path = ".";
    let mut file_type = None;
    let mut max_results = 50;

    // Parse optional arguments
    if parts.len() > 1 {
        let opts = parts[1];
        for opt in opts.split_whitespace() {
            if let Some(p) = opt.strip_prefix("--path=") {
                path = p;
            } else if let Some(ft) = opt.strip_prefix("--type=") {
                file_type = Some(ft);
            } else if let Some(n) = opt.strip_prefix("--max=") {
                max_results = n.parse().unwrap_or(50);
            }
        }
    }

    let mut cmd = Command::new("rg");
    cmd.args([
        "--color",
        "never",
        "--no-heading",
        "--line-number",
        "--context",
        "1",
        "-m",
        &max_results.to_string(),
        pattern,
        path,
    ]);

    if let Some(ft) = file_type {
        cmd.arg("--type");
        cmd.arg(ft);
    }

    let output = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg returns 1 if no matches found, which is not an error
    if output.status.code() != Some(0) && output.status.code() != Some(1) {
        anyhow::bail!("search_codebase failed: {stderr}");
    }

    let content = if stdout.is_empty() {
        format!("No matches found for pattern: {pattern}")
    } else {
        stdout.to_string()
    };

    let status = format!("Searched for '{pattern}' in {path}");
    Ok(ToolOutput::Text { content, status })
}

// ============================================================================
// Test Runner Tool
// ============================================================================

/// Detect the project type and return the appropriate test command
fn detect_test_command(project_root: &Path) -> Option<(&'static str, Vec<&'static str>)> {
    // Rust (Cargo.toml)
    if project_root.join("Cargo.toml").exists() {
        return Some(("cargo", vec!["test"]));
    }
    // Node.js (package.json)
    if project_root.join("package.json").exists() {
        return Some(("npm", vec!["test"]));
    }
    // Python (pytest or unittest)
    if project_root.join("pytest.ini").exists()
        || project_root.join("pyproject.toml").exists()
        || project_root.join("setup.py").exists()
    {
        return Some(("pytest", vec![]));
    }
    // Python with unittest
    if project_root.join("tests").is_dir() {
        return Some(("python", vec!["-m", "unittest", "discover"]));
    }
    // Go (go.mod)
    if project_root.join("go.mod").exists() {
        return Some(("go", vec!["test", "./..."]));
    }
    None
}

async fn run_tests_handler(arg: &str) -> Result<ToolOutput> {
    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).unwrap_or_else(|| working_dir.clone());

    // Check for custom command in args
    let custom_cmd = arg.trim();
    let (cmd_name, cmd_args) = if custom_cmd.is_empty() {
        // Auto-detect
        detect_test_command(&git_root).ok_or_else(|| {
            anyhow!("Could not detect project type. Specify test command manually.")
        })?
    } else {
        // Parse custom command
        let parts: Vec<&str> = custom_cmd.split_whitespace().collect();
        (
            parts.first().copied().unwrap_or("cargo"),
            parts[1..].to_vec(),
        )
    };

    let output = Command::new(cmd_name)
        .args(&cmd_args)
        .current_dir(&git_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let mut result = String::new();
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

    if result.is_empty() {
        let status = if exit_code == 0 {
            format!(
                "Tests passed with no output ({} cmd: {})",
                cmd_name,
                cmd_args.join(" ")
            )
        } else {
            format!("Tests failed with no output (exit code: {exit_code})")
        };
        Ok(ToolOutput::StatusOnly { status })
    } else {
        let status = if exit_code == 0 {
            format!("Tests passed ({} cmd: {})", cmd_name, cmd_args.join(" "))
        } else {
            format!("Tests failed (exit code: {exit_code})")
        };
        Ok(ToolOutput::Text {
            content: result,
            status,
        })
    }
}

// ============================================================================
// Project Context Tool
// ============================================================================

async fn get_project_context_handler(_arg: &str) -> Result<ToolOutput> {
    use std::fmt::Write;

    let working_dir = std::env::current_dir()?;
    let git_root = find_git_root(&working_dir).unwrap_or_else(|| working_dir.clone());

    let display_git_root = to_relative_path(&git_root);

    let mut context = String::new();

    // Project type detection
    let project_type = if git_root.join("Cargo.toml").exists() {
        "Rust (Cargo)"
    } else if git_root.join("package.json").exists() {
        "Node.js (npm)"
    } else if git_root.join("pyproject.toml").exists() || git_root.join("setup.py").exists() {
        "Python"
    } else if git_root.join("go.mod").exists() {
        "Go"
    } else if git_root.join("pom.xml").exists() {
        "Java (Maven)"
    } else if git_root.join("build.gradle").exists() || git_root.join("build.gradle.kts").exists() {
        "Java (Gradle)"
    } else {
        "Unknown"
    };
    writeln!(context, "Project Type: {project_type}").unwrap();

    let root_name = git_root
        .file_name().map_or_else(|| display_git_root.clone(), |name| name.to_string_lossy().to_string());
    writeln!(context, "Project Root: {root_name}").unwrap();

    // Git info
    let git_output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&git_root)
        .output()
        .await;
    if let Ok(output) = git_output {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            writeln!(context, "Git Branch: {branch}").unwrap();
        }
    }

    // Directory structure (top level)
    context.push_str("\nTop-level structure:\n");
    if let Ok(mut entries) = fs::read_dir(&git_root).await {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // Skip hidden files/dirs
            }
            // Skip gitignored files
            if is_gitignored(&entry.path(), &git_root).unwrap_or(false) {
                continue;
            }
            let is_dir = entry.path().is_dir();
            if is_dir {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        for dir in dirs {
            writeln!(context, "  📁 {dir}").unwrap();
        }
        for file in files {
            writeln!(context, "  📄 {file}").unwrap();
        }
    }

    // Key config files
    let key_files = [
        "README.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "docker-compose.yml",
        ".github/workflows",
    ];
    let mut found_files = Vec::new();
    for kf in &key_files {
        let full_path = git_root.join(kf);
        if full_path.exists() {
            found_files.push(kf.to_string());
        }
    }
    if !found_files.is_empty() {
        context.push_str("\nKey files:\n");
        for f in found_files {
            writeln!(context, "  - {f}").unwrap();
        }
    }

    let status = format!("Project context for {root_name}");
    Ok(ToolOutput::Text {
        content: context,
        status,
    })
}

// ============================================================================
// Browser automation state
// ============================================================================

struct BrowserState {
    browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    pages: Vec<Page>,
    current_idx: usize,
}

impl BrowserState {
    async fn new() -> Result<Self> {
        let builder = BrowserConfig::builder();
        let (browser, handler) =
            Browser::launch(builder.build().map_err(anyhow::Error::msg)?).await?;
        let handler_task = tokio::spawn(handler.for_each(|_| async {}));
        let page = browser.new_page("about:blank").await?;
        Ok(Self {
            browser,
            handler_task,
            pages: vec![page],
            current_idx: 0,
        })
    }

    fn current_page(&self) -> &Page {
        &self.pages[self.current_idx]
    }

    fn current_page_mut(&mut self) -> &mut Page {
        &mut self.pages[self.current_idx]
    }
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        self.handler_task.abort();
    }
}

static BROWSER_STATE: OnceCell<Arc<Mutex<Option<BrowserState>>>> = OnceCell::new();

fn get_browser_state() -> Arc<Mutex<Option<BrowserState>>> {
    BROWSER_STATE
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

async fn ensure_browser_initialized() -> Result<Arc<Mutex<Option<BrowserState>>>> {
    let state_arc = get_browser_state();
    let mut guard = state_arc.lock().await;
    if guard.is_none() {
        *guard = Some(BrowserState::new().await?);
    }
    Ok(state_arc.clone())
}

// Browser tool handlers
fn browser_open_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let url = arg.trim();
        if url.is_empty() {
            return Err(anyhow!("URL cannot be empty"));
        }
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        state.current_page_mut().goto(url).await?;
        let status = format!("Opened URL: {url}");
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_click_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let selector = arg.trim();
        if selector.is_empty() {
            return Err(anyhow!("CSS selector cannot be empty"));
        }
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();

        // Find element - fail immediately if not found
        let element = state
            .current_page()
            .find_element(selector)
            .await
            .map_err(|_| anyhow!("Element '{selector}' not found"))?;

        // Click element
        element
            .click()
            .await
            .map_err(|e| anyhow!("Error clicking element: {e}"))?;

        let status = format!("Clicked element: {selector}");
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_type_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let mut parts = arg.splitn(2, ' ');
        let selector = parts
            .next()
            .ok_or_else(|| anyhow!("Missing selector"))?
            .trim();
        let text = parts.next().ok_or_else(|| anyhow!("Missing text"))?.trim();
        if selector.is_empty() || text.is_empty() {
            return Err(anyhow!("Selector and text are required"));
        }
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();

        let element = state
            .current_page()
            .find_element(selector)
            .await
            .map_err(|_| anyhow!("Element '{selector}' not found"))?;
        element.type_str(text).await?;
        let status = format!("Typed '{text}' into {selector}");
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_get_html_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        let content = state.current_page().content().await?;
        if content.is_empty() {
            Ok(ToolOutput::StatusOnly {
                status: "Page HTML is empty".to_string(),
            })
        } else {
            let status = format!("Retrieved HTML from current page ({} bytes)", content.len());
            Ok(ToolOutput::Text { content, status })
        }
    })
}

fn browser_go_back_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        state
            .current_page()
            .evaluate("window.history.back()")
            .await?;
        let status = "Navigated back".to_string();
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_refresh_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        state
            .current_page()
            .evaluate("window.location.reload()")
            .await?;
        let status = "Page refreshed".to_string();
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_evaluate_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let js = arg.trim();
        if js.is_empty() {
            return Err(anyhow!("JavaScript code cannot be empty"));
        }
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        let result = state.current_page().evaluate(js).await?;
        let result_value = result.value();
        let result_str = serde_json::to_string(&result_value)
            .unwrap_or_else(|_| "<serialization error>".to_string());
        let status = format!("Evaluation result: {result_str}");
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_new_tab_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let url = arg.trim();
        let url = if url.is_empty() { "about:blank" } else { url };
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        match timeout(Duration::from_secs(30), state.browser.new_page(url)).await {
            Ok(result) => {
                let new_page =
                    result.map_err(|e| anyhow::anyhow!("Failed to open new page: {e}"))?;
                state.pages.push(new_page);
                let new_idx = state.pages.len() - 1;
                state.current_idx = new_idx;
                let status = format!("Opened new tab {} with URL: {}", new_idx + 1, url);
                Ok(ToolOutput::StatusOnly { status })
            }
            Err(_) => Err(anyhow::anyhow!("Timeout opening new tab after 30 seconds")),
        }
    })
}

fn browser_close_tab_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        if state.pages.len() <= 1 {
            return Err(anyhow!("Cannot close the last tab"));
        }
        let idx = if arg.trim().is_empty() {
            state.current_idx
        } else {
            let idx = arg
                .trim()
                .parse::<usize>()
                .map_err(|_| anyhow!("Invalid tab index"))?
                .checked_sub(1)
                .ok_or_else(|| anyhow!("Tab index must be >= 1"))?;
            if idx >= state.pages.len() {
                return Err(anyhow!("Tab index out of range"));
            }
            idx
        };
        state.pages.remove(idx);
        if state.current_idx >= idx {
            if state.current_idx == idx {
                state.current_idx = state.current_idx.saturating_sub(1);
            } else {
                state.current_idx -= 1;
            }
        }
        let status = format!("Closed tab {}", idx + 1);
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_switch_tab_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let idx = arg
            .trim()
            .parse::<usize>()
            .map_err(|_| anyhow!("Invalid tab index"))?
            .checked_sub(1)
            .ok_or_else(|| anyhow!("Tab index must be >= 1"))?;
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        if idx >= state.pages.len() {
            return Err(anyhow!("Tab index out of range"));
        }
        state.current_idx = idx;
        let status = format!("Switched to tab {}", idx + 1);
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_list_tabs_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let guard = state_arc.lock().await;
        let state = guard.as_ref().unwrap();
        let mut list = Vec::new();
        for (i, page) in state.pages.iter().enumerate() {
            let url_opt = page.url().await?;
            let url_str = url_opt.unwrap_or_else(|| "<no url>".to_string());
            let current_marker = if i == state.current_idx {
                " <-- current"
            } else {
                ""
            };
            list.push(format!("{}. {}{}", i + 1, url_str, current_marker));
        }
        let content = list.join("\n");
        let status = format!("Listed {} open tabs", list.len());
        Ok(ToolOutput::Text { content, status })
    })
}

fn browser_quit_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        if let Some(state_arc) = BROWSER_STATE.get() {
            let mut guard = state_arc.lock().await;
            if let Some(mut state) = guard.take() {
                let _ = state.browser.close().await;
                // handler_task will be dropped, aborting it
                let status = "Browser closed".to_string();
                return Ok(ToolOutput::StatusOnly { status });
            }
        }
        let status = "No browser was open".to_string();
        Ok(ToolOutput::StatusOnly { status })
    })
}

fn browser_wait_for_navigation_handler(arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let timeout_secs = arg.trim().parse::<u64>().unwrap_or(30);
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        match timeout(
            Duration::from_secs(timeout_secs),
            state.current_page().wait_for_navigation(),
        )
        .await
        {
            Ok(Ok(_)) => {
                let status = "Page finished navigation".to_string();
                Ok(ToolOutput::StatusOnly { status })
            }
            Ok(Err(e)) => Err(anyhow!("Error during navigation: {e}")),
            Err(_) => Err(anyhow!(
                "Timeout waiting for navigation after {timeout_secs} seconds"
            )),
        }
    })
}

fn browser_screenshot_handler(_arg: &str) -> ToolFuture<'_> {
    Box::pin(async move {
        let state_arc = ensure_browser_initialized().await?;
        let mut guard = state_arc.lock().await;
        let state = guard.as_mut().unwrap();
        let png_data = state
            .current_page()
            .screenshot(ScreenshotParams::default())
            .await?;
        let status = format!("Captured screenshot ({} bytes)", png_data.len());
        Ok(ToolOutput::Binary {
            data: png_data,
            mime_type: "image/png".to_string(),
            status,
        })
    })
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
            description: "read_file <file_path> [start_line] [end_line] : outputs the text contents of a file. Both start_line and end_line are 1-indexed inclusive. If only start_line is given, reads from that line to the end. If neither given, reads entire file.",
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
            description: "write_file <file_path> : writes the provided content to the file, creating any necessary parent directories. If the file exists, it is overwritten. The content should follow the file path on subsequent lines. To add commentary without writing it to the file, place the commentary after a line containing only '--'. Everything before that line (excluding the '--' line) is written; everything after is ignored.",
            handler: Box::new(|s| Box::pin(write_file_handler(s))),
        },
    );
    m.insert(
        "search_web",
        Tool {
            description: "search_web <query> : performs a web search using DuckDuckGo and returns a list of results with titles, URLs, and snippets. DO NOT quote the query string.",
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
    m.insert(
        "browser_open",
        Tool {
            description: "browser_open <url> : Opens a URL in a visible Chrome/Chromium browser window.",
            handler: Box::new(|s| Box::pin(browser_open_handler(s))),
        },
    );
    m.insert(
        "browser_click",
        Tool {
            description: "browser_click <selector> : Clicks an element matching the CSS selector.",
            handler: Box::new(|s| Box::pin(browser_click_handler(s))),
        },
    );
    m.insert(
        "browser_type",
        Tool {
            description: "browser_type <selector> <text> : Types the specified text into an input field identified by the CSS selector.",
            handler: Box::new(|s| Box::pin(browser_type_handler(s))),
        },
    );
    m.insert(
        "browser_get_html",
        Tool {
            description: "browser_get_html : Returns the HTML content of the current page.",
            handler: Box::new(|s| Box::pin(browser_get_html_handler(s))),
        },
    );
    m.insert(
        "browser_go_back",
        Tool {
            description: "browser_go_back : Navigates back in the browser history.",
            handler: Box::new(|s| Box::pin(browser_go_back_handler(s))),
        },
    );
    m.insert(
        "browser_refresh",
        Tool {
            description: "browser_refresh : Reloads the current page.",
            handler: Box::new(|s| Box::pin(browser_refresh_handler(s))),
        },
    );
    m.insert(
        "browser_evaluate",
        Tool {
            description: "browser_evaluate <javascript> : Executes JavaScript code in the browser page and returns the result.",
            handler: Box::new(|s| Box::pin(browser_evaluate_handler(s))),
        },
    );
    m.insert(
        "browser_new_tab",
        Tool {
            description: "browser_new_tab [url] : Opens a new browser tab. If URL is provided, navigates to it; otherwise opens about:blank.",
            handler: Box::new(|s| Box::pin(browser_new_tab_handler(s))),
        },
    );
    m.insert(
        "browser_close_tab",
        Tool {
            description: "browser_close_tab [index] : Closes the specified tab (1-based). If no index provided, closes the current tab. Cannot close the last tab.",
            handler: Box::new(|s| Box::pin(browser_close_tab_handler(s))),
        },
    );
    m.insert(
        "browser_switch_tab",
        Tool {
            description: "browser_switch_tab <index> : Switches to the tab with the given 1-based index.",
            handler: Box::new(|s| Box::pin(browser_switch_tab_handler(s))),
        },
    );
    m.insert(
        "browser_list_tabs",
        Tool {
            description: "browser_list_tabs : Lists all open tabs with their URLs and indicates the current tab.",
            handler: Box::new(|s| Box::pin(browser_list_tabs_handler(s))),
        },
    );
    m.insert(
        "browser_quit",
        Tool {
            description: "browser_quit : Closes the browser and all tabs, shutting down the browser process.",
            handler: Box::new(|s| Box::pin(browser_quit_handler(s))),
        },
    );
    m.insert(
        "browser_wait_for_navigation",
        Tool {
            description: "browser_wait_for_navigation [timeout] : Waits for the current page to finish loading. Optional timeout in seconds (default 30).",
            handler: Box::new(|s| Box::pin(browser_wait_for_navigation_handler(s))),
        },
    );
    m.insert(
        "browser_screenshot",
        Tool {
            description: "browser_screenshot : Provides you with a screenshot of the current page.",
            handler: Box::new(|s| Box::pin(browser_screenshot_handler(s))),
        },
    );
    // Git tools
    m.insert(
        "git_status",
        Tool {
            description: "git_status [directory] : Shows git status for the repository. If no directory specified, uses current directory.",
            handler: Box::new(|s| Box::pin(git_status_handler(s))),
        },
    );
    m.insert(
        "git_diff",
        Tool {
            description: "git_diff [options] : Shows git diff. Supports --staged, --cached, or commit ranges like HEAD~1.",
            handler: Box::new(|s| Box::pin(git_diff_handler(s))),
        },
    );
    m.insert(
        "git_log",
        Tool {
            description: "git_log [count] : Shows recent commits. Default is 10 commits.",
            handler: Box::new(|s| Box::pin(git_log_handler(s))),
        },
    );
    m.insert(
        "git_commit",
        Tool {
            description: "git_commit <message> : Creates a git commit with the given message.",
            handler: Box::new(|s| Box::pin(git_commit_handler(s))),
        },
    );
    m.insert(
        "git_add",
        Tool {
            description: "git_add <pathspec> : Stages changes. Use '.' for all changes.",
            handler: Box::new(|s| Box::pin(git_add_handler(s))),
        },
    );
    // Codebase search
    m.insert(
        "search_codebase",
        Tool {
            description: "search_codebase <pattern> [--path=dir] [--type=lang] [--max=N] : Searches code using ripgrep. Default max 50 results.",
            handler: Box::new(|s| Box::pin(search_codebase_handler(s))),
        },
    );
    // Test runner
    m.insert(
        "run_tests",
        Tool {
            description: "run_tests [custom_cmd] : Runs project tests. Auto-detects Rust/Node/Python/Go. Optional custom command.",
            handler: Box::new(|s| Box::pin(run_tests_handler(s))),
        },
    );
    // Project context
    m.insert(
        "get_project_context",
        Tool {
            description: "get_project_context : Returns project type, git branch, directory structure, and key files.",
            handler: Box::new(|s| Box::pin(get_project_context_handler(s))),
        },
    );
    m
});

// Build the system prompt dynamically from the tool registry
pub static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    let header = r#"You are an AI assistant that uses tools to get accurate information and complete tasks.
Always use tools to gather information rather than making assumptions.

## Tool Usage
To use a tool, output a line starting with "TOOL:" followed by the tool name and its argument(s).
- For tools that require multiple pieces of data, the argument(s) may span multiple lines
- You may make multiple tool calls per response
- After making a tool call, you will receive the tool's result in a subsequent prompt
- Do not guess information that could be obtained via a tool call; use the appropriate tool instead
- Do not include any other text before or after the tool call(s)
- If a tool call fails, read the error message and correct the call if needed
- **Do not add quotes around arguments unless required for escaping.** Most arguments (file paths, URLs, selectors, search queries) should be passed as plain text without extra quotes. For example, use `TOOL: search_web climate change` not `TOOL: search_web "climate change"`.

## Best Practices
- Use `get_project_context` at the start to understand the project structure
- Use `git_status` before making changes to understand the current state
- Use `search_codebase` to find relevant code before making edits
- Use `run_tests` after making changes to verify correctness
- Use `git_diff` to review changes before committing

Available tools:
"#;
    let mut tool_lines: Vec<String> = TOOLS
        .iter()
        .map(|(name, tool)| format!("- {} : {}", name, tool.description))
        .collect();
    tool_lines.sort(); // consistent order
    header.to_string() + &tool_lines.join("\n")
});

/// Executes a tool by name with the given argument.
///
/// # Errors
/// Returns an error if the tool is unknown or if the tool's handler fails.
pub async fn execute_tool(name: &str, arg: &str) -> Result<ToolOutput> {
    match TOOLS.get(name) {
        Some(tool) => (tool.handler)(arg).await,
        None => anyhow::bail!("Unknown tool: {name}"),
    }
}
