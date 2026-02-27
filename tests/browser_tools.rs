use anyhow::Result;
use deepseek_cli::tools::execute_tool;
use tokio::time::{sleep, Duration};
use futures_util::future::FutureExt;
use urlencoding::encode;

// This test requires a real browser and is ignored by default.
// Run with: cargo test -- --ignored
#[tokio::test]
#[ignore]
async fn test_browser_tools() -> Result<()> {
    let test_future = async {
        // Create a simple HTML page with a button that changes a div's text
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
        let data_url = format!("data:text/html;charset=utf-8,{}", encoded);

        // ----- Open the page -----
        let res = execute_tool("browser_open", &data_url).await?;
        assert!(res.contains("Opened URL"), "browser_open failed: {}", res);

        // Wait for page to load
        sleep(Duration::from_secs(2)).await;

        // Verify page loaded by checking title
        let title = execute_tool("browser_evaluate", "document.title").await?;
        assert!(title.contains("Test Page"), "Expected title to contain 'Test Page', got: {}", title);

        // Click the button
        let res = execute_tool("browser_click", "#btn").await?;
        assert!(res.contains("Clicked element"), "browser_click failed: {}", res);

        // Wait for JavaScript to execute
        sleep(Duration::from_millis(500)).await;

        // Check that the button click changed the status div
        let status = execute_tool("browser_evaluate", "document.getElementById('status').innerText").await?;
        assert!(status.contains("Clicked"), "browser_evaluate after click did not return updated text: {}", status);

        // Open a new tab
        let res = execute_tool("browser_new_tab", "about:blank").await?;
        assert!(res.contains("Opened new tab 2"), "browser_new_tab failed: {}", res);

        // List tabs
        let tabs = execute_tool("browser_list_tabs", "").await?;
        assert!(tabs.contains("1."), "browser_list_tabs missing first tab");
        assert!(tabs.contains("2."), "browser_list_tabs missing second tab");

        // Switch back to tab 1
        let res = execute_tool("browser_switch_tab", "1").await?;
        assert!(res.contains("Switched to tab 1"), "browser_switch_tab failed: {}", res);

        // Close tab 2
        let res = execute_tool("browser_close_tab", "2").await?;
        assert!(res.contains("Closed tab 2"), "browser_close_tab failed: {}", res);

        // List tabs again (should only have one)
        let tabs = execute_tool("browser_list_tabs", "").await?;
        assert_eq!(tabs.lines().count(), 1, "Expected only one tab after closing");

        Ok::<_, anyhow::Error>(())
    };

    let result = std::panic::AssertUnwindSafe(test_future).catch_unwind().await;

    // Always attempt to close the browser, ignoring errors
    let _ = execute_tool("browser_quit", "").await;

    // Propagate any panic from the test
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}