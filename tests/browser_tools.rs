use anyhow::Result;
use deepseek_cli::tools::{ToolOutput, execute_tool};
use futures_util::future::FutureExt;
use tokio::time::{Duration, sleep};
use urlencoding::encode;

// Helper: create data URL
fn create_test_data_url() -> String {
    let html_content = r#"
    <html>
    <head><title>Test Page</title></head>
    <body>
        <div id="status">Initial</div>
        <button id="btn" onclick="document.getElementById('status').innerText='Clicked'">Click me</button>
    </body>
    </html>
    "#;
    let encoded = encode(html_content);
    format!("data:text/html;charset=utf-8,{encoded}")
}

async fn open_and_verify_title(data_url: &str) -> Result<()> {
    let res = execute_tool("browser_open", data_url).await?;
    let ToolOutput::StatusOnly { status } = &res else {
        panic!("Expected StatusOnly, got {res:?}")
    };
    assert!(status.contains("Opened URL"), "browser_open failed: {res:?}");

    sleep(Duration::from_secs(2)).await;

    let title = execute_tool("browser_evaluate", "document.title").await?;
    let ToolOutput::StatusOnly { status: eval_status } = &title else {
        panic!("Expected StatusOnly, got {title:?}")
    };
    assert!(
        eval_status.contains("Test Page"),
        "Expected title to contain 'Test Page', got: {title:?}"
    );
    Ok(())
}

async fn click_button_and_verify() -> Result<()> {
    let res = execute_tool("browser_click", "#btn").await?;
    let ToolOutput::StatusOnly { status } = &res else {
        panic!("Expected StatusOnly, got {res:?}")
    };
    assert!(status.contains("Clicked element"), "browser_click failed: {res:?}");

    sleep(Duration::from_millis(500)).await;

    let eval_res = execute_tool(
        "browser_evaluate",
        "document.getElementById('status').innerText",
    )
    .await?;
    let ToolOutput::StatusOnly { status: eval_status } = &eval_res else {
        panic!("Expected StatusOnly, got {eval_res:?}")
    };
    assert!(
        eval_status.contains("Clicked"),
        "browser_evaluate after click did not return updated text: {eval_res:?}"
    );
    Ok(())
}

async fn test_tab_management() -> Result<()> {
    // Open new tab
    let res = execute_tool("browser_new_tab", "about:blank").await?;
    let ToolOutput::StatusOnly { status } = &res else {
        panic!("Expected StatusOnly, got {res:?}")
    };
    assert!(status.contains("Opened new tab 2"), "browser_new_tab failed: {res:?}");

    // List tabs (should have 2)
    let tabs = execute_tool("browser_list_tabs", "").await?;
    let ToolOutput::Text { content: tabs_content, .. } = &tabs else {
        panic!("Expected Text, got {tabs:?}")
    };
    assert!(tabs_content.contains("1."), "browser_list_tabs missing first tab");
    assert!(tabs_content.contains("2."), "browser_list_tabs missing second tab");

    // Switch back to tab 1
    let res = execute_tool("browser_switch_tab", "1").await?;
    let ToolOutput::StatusOnly { status } = &res else {
        panic!("Expected StatusOnly, got {res:?}")
    };
    assert!(status.contains("Switched to tab 1"), "browser_switch_tab failed: {res:?}");

    // Close tab 2
    let res = execute_tool("browser_close_tab", "2").await?;
    let ToolOutput::StatusOnly { status } = &res else {
        panic!("Expected StatusOnly, got {res:?}")
    };
    assert!(status.contains("Closed tab 2"), "browser_close_tab failed: {res:?}");

    // List tabs again (should only have one)
    let tabs = execute_tool("browser_list_tabs", "").await?;
    let ToolOutput::Text { content: tabs_content, .. } = &tabs else {
        panic!("Expected Text, got {tabs:?}")
    };
    assert_eq!(
        tabs_content.lines().count(),
        1,
        "Expected only one tab after closing"
    );
    Ok(())
}

// This test requires a real browser and may be slow.
#[tokio::test]
async fn test_browser_tools() -> Result<()> {
    let test_future = async {
        let data_url = create_test_data_url();
        open_and_verify_title(&data_url).await?;
        click_button_and_verify().await?;
        test_tab_management().await?;
        Ok::<_, anyhow::Error>(())
    };

    let result = std::panic::AssertUnwindSafe(test_future)
        .catch_unwind()
        .await;

    // Always attempt to close the browser, ignoring errors
    let _ = execute_tool("browser_quit", "").await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
