use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};
use tempfile::tempdir;

#[test]
fn process_handles_initialize_and_tool_listing_as_json_rpc() {
    let dir = tempdir().expect("temporary database directory");
    let database = dir.path().join("catalog.sqlite");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oracle-fusion-erp-catalog-mcp"))
        .env("ORACLE_MCP_DATABASE", &database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MCP process");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .expect("initialize request");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .expect("initialized notification");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
        )
        .expect("tools request");
    }
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout");
    let responses: Vec<Value> = BufReader::new(stdout)
        .lines()
        .map(|line| serde_json::from_str(&line.expect("response line")).expect("JSON-RPC response"))
        .collect();
    let status = child.wait().expect("wait for MCP process");

    assert!(status.success());
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "oracle-fusion-erp-catalog-mcp"
    );
    assert_eq!(responses[1]["id"], 2);
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == "search_table_structure"));
    for name in [
        "find_tables_by_column",
        "search_columns",
        "find_related_tables",
        "list_releases",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
}
