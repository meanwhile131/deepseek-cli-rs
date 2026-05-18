use anyhow::Result;
use deepseek_cli::tools::{ToolOutput, execute_tool};
use std::fs;
use tempfile::TempDir;

// Test read_file on empty file with no offset/limit arguments (just the path)
#[tokio::test]
async fn test_read_file_empty_no_args() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let file_path = temp_dir.path().join("empty.txt");
    fs::write(&file_path, b"")?;
    let path_str = file_path.to_str().unwrap();

    let result = execute_tool("read_file", path_str).await?;
    match result {
        ToolOutput::StatusOnly { status } => {
            assert!(
                status.contains("empty") || status.contains("File is empty"),
                "Expected empty file status, got: {status}"
            );
            assert!(status.contains(path_str));
        }
        ToolOutput::Text { content, status } => {
            assert_eq!(content, "");
            assert!(status.contains(path_str));
        }
        other => panic!("Expected StatusOnly or Text, got {other:?}"),
    }
    Ok(())
}

// Existing test: also checks with explicit offset/limit
#[tokio::test]
async fn test_read_file_empty_with_args() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let file_path = temp_dir.path().join("empty.txt");
    fs::write(&file_path, b"")?;
    let path_str = file_path.to_str().unwrap();

    // Test with explicit start_line and end_line (should be handled gracefully)
    let result = execute_tool("read_file", &format!("{path_str} 1 10")).await?;
    match result {
        ToolOutput::StatusOnly { status } => {
            assert!(
                status.contains("empty") || status.contains("File is empty"),
                "Expected empty file status: {status}"
            );
            assert!(status.contains(path_str));
        }
        ToolOutput::Text { content, status } => {
            assert_eq!(content, "");
            assert!(status.contains(path_str));
        }
        other => panic!("Expected StatusOnly or Text, got {other:?}"),
    }
    Ok(())
}
