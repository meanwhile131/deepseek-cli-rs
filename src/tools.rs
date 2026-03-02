use anyhow::{Result, anyhow};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt;
use once_cell::sync::OnceCell;
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use urlencoding::encode;

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
    let content = names.join("\n");
    let status = format!("Listed {} files in {}", names.len(), arg);
    Ok(ToolOutput::Text { content, status })
}

async fn read_file_handler(arg: &str) -> Result<ToolOutput> {
    if arg.contains('\n') {
        anyhow::bail!("read_file: path argument must be on a single line (no newlines)");
    }
    let content = fs::read_to_string(arg).await?;
    let status = format!("Read file at {arg}");
    Ok(ToolOutput::Text { content, status })
}

async fn create_directory_handler(arg: &str) -> Result<ToolOutput> {
    if arg.contains('\n') {
        anyhow::bail!("create_directory: path argument must be on a single line (no newlines)");
    }
    fs::create_dir_all(arg).await?;
    let status = format!("Directory created: {arg}");
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

    let mut content = fs::read_to_string(&file_path).await?;
    for (search, replace) in &blocks {
        if !content.contains(search) {
            anyhow::bail!("Search string not found in {file_path}: {search:?}");
        }
        content = content.replace(search, replace);
    }
    fs::write(&file_path, &content).await?;
    let status = format!("Applied {} block(s) to {}", blocks.len(), file_path);
    Ok(ToolOutput::StatusOnly { status })
}

async fn run_command_handler(arg: &str) -> Result<ToolOutput> {
    #[cfg(windows)]
    let output = Command::new("cmd").args(&["/c", arg]).output().await?;
    #[cfg(not(windows))]
    let output = Command::new("sh").args(["-c", arg]).output().await?;
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
    if stdout.is_empty() && stderr.is_empty() {
        result.push_str("Command executed with no output");
    }
    let status = if exit_code == 0 {
        "Command succeeded (exit code: 0)".to_string()
    } else {
        format!("Command failed (exit code: {exit_code})")
    };
    Ok(ToolOutput::Text {
        content: result,
        status,
    })
}

async fn write_file_handler(arg: &str) -> Result<ToolOutput> {
    let mut lines = arg.lines();
    let file_path = lines
        .next()
        .ok_or_else(|| anyhow!("Missing file path"))?
        .to_string();
    let content: String = lines.collect::<Vec<&str>>().join("\n");

    if let Some(parent) = Path::new(&file_path).parent() {
        fs::create_dir_all(parent).await?;
    }

    fs::write(&file_path, &content).await?;
    let status = format!("File written: {file_path}");
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
    let size = content.len();
    let status = format!("Fetched URL: {url} ({size} bytes)");
    Ok(ToolOutput::Text { content, status })
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

// Browser automation state
struct BrowserState {
    browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    pages: Vec<Page>,
    current_idx: usize,
}

impl BrowserState {
    async fn new() -> Result<Self> {
        let mut builder = BrowserConfig::builder();
        if std::env::var("DISPLAY").is_ok() {
            builder = builder.with_head();
        }
        let (browser, handler) = Browser::launch(
            builder.build().map_err(anyhow::Error::msg)?,
        )
        .await?;
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
        let status = format!("Retrieved HTML from current page ({} bytes)", content.len());
        Ok(ToolOutput::Text { content, status })
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
    m
});

// Build the system prompt dynamically from the tool registry
pub static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    let header = r#"To use a tool, output a line starting with "TOOL:" followed by the tool name and its argument(s). For tools that require multiple pieces of data, the argument(s) may span multiple lines. You may make multiple tool calls per response.
After making a tool call, you will receive the tool's result in a subsequent prompt. Do not guess information that could be obtained via a tool call; instead, use the appropriate tool to get accurate data.

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
