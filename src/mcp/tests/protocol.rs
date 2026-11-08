//! The MCP framing: requests and replies over lines, versions and progress.

use super::*;

async fn exchange(tools: Tools, requests: &[Value]) -> Vec<Value> {
    let (mut client, server) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server);
    let serving = tokio::spawn(serve(
        Arc::new(tools),
        BufReader::new(server_read),
        server_write,
    ));
    for request in requests {
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
    }
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut lines = BufReader::new(client_read).lines();
    let expected = requests
        .iter()
        .filter(|request| request.get("id").is_some())
        .count();
    let mut responses: Vec<Value> = Vec::new();
    // Notifications the server sends arrive in order among the responses.
    while responses
        .iter()
        .filter(|message| message.get("id").is_some())
        .count()
        < expected
    {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        responses.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    client_write.shutdown().await.unwrap();
    drop(client_write);
    drop(lines);
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
    responses
}

#[tokio::test]
async fn the_server_speaks_mcp_over_lines() {
    let controller = controller().await;
    let project = project();
    let responses = exchange(
        tools(&controller, project.path()),
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": "four", "method": "tools/call", "params": {"name": "list_models", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
        ],
    )
    .await;
    let by_id = |id: Value| {
        responses
            .iter()
            .find(|response| response["id"] == id)
            .unwrap_or_else(|| panic!("no response to {id}"))
            .clone()
    };
    let init = by_id(json!(1));
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "poundlock");
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("delegate_task")
    );
    assert_eq!(
        by_id(json!(2))["result"]["tools"].as_array().unwrap().len(),
        9
    );
    assert_eq!(by_id(json!(3))["result"], json!({}));
    let call = by_id(json!("four"));
    assert_eq!(call["result"]["isError"], false);
    assert_eq!(call["result"]["content"][0]["type"], "text");
    assert_eq!(
        call["result"]["structuredContent"]["models"][0]["name"],
        "coder"
    );
    assert_eq!(by_id(json!(5))["error"]["code"], -32601);
}

#[tokio::test]
async fn an_unknown_protocol_version_gets_the_newest() {
    let controller = controller().await;
    let project = project();
    let responses = exchange(
        tools(&controller, project.path()),
        &[json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}})],
    )
    .await;
    assert_eq!(
        responses[0]["result"]["protocolVersion"],
        PROTOCOL_VERSIONS[0]
    );
}

#[tokio::test]
async fn a_long_wait_reports_progress_before_its_result() {
    let controller = controller().await;
    let project = project();
    let tools = tools(&controller, project.path());
    let started = tools
        .call("delegate_task", delegate_args(project.path()))
        .await;
    let id = started.structured["job_id"].as_str().unwrap().to_owned();
    let messages = exchange(
        tools,
        &[json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": "wait_for_job",
                "arguments": {"job_id": id, "timeout_seconds": 1},
                "_meta": {"progressToken": "wait-1"}
            }
        })],
    )
    .await;
    let progress: Vec<&Value> = messages
        .iter()
        .filter(|message| message["method"] == "notifications/progress")
        .collect();
    assert!(!progress.is_empty(), "{messages:?}");
    assert_eq!(progress[0]["params"]["progressToken"], "wait-1");
    assert_eq!(progress[0]["params"]["progress"], 1);
    assert!(
        progress[0]["params"]["message"]
            .as_str()
            .unwrap()
            .contains("is submitted")
    );
    let result = messages.last().unwrap();
    assert_eq!(
        result["id"], 7,
        "the result comes after every progress line"
    );
    assert_eq!(result["result"]["structuredContent"]["finished"], false);
}
