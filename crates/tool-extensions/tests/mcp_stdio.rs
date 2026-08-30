//! End-to-end over a real child process: a canned `sh` MCP server that
//! answers the client's fixed handshake ids (1: initialize, 2:
//! tools/list, 3: the first tools/call).

#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use orca_harness_core::{
    CancellationToken, Context, Model, ModelError, ModelResponse, Tool, ToolContext, ToolSchema,
};
use orca_harness_tool_extensions::mcp::{
    McpCatalog, McpClient, McpError, McpModel, ProcessEnvironment, StdioLaunch,
};
use serde_json::json;

const FAKE_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
read _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hello back"}]}}'
"#;

const PAGED_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"paged","version":"0.0.0"}}}'
read _initialized
read _first
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"one","inputSchema":{"type":"object"}}],"nextCursor":"two"}}'
read _second
case "$_second" in *'"cursor":"two"'*) ;; *) exit 2 ;; esac
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"two","inputSchema":{"type":"object"}}]}}'
"#;

const CANCEL_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"cancel","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","inputSchema":{"type":"object"}}]}}'
read _call
sleep 10
"#;

const STRUCTURED_LAUNCH_SERVER: &str = r#"#!/bin/sh
[ "$1" = '--literal=argument with spaces;$(not-a-shell)' ] || exit 41
[ "$PWD" = "$EXPECTED_CWD" ] || exit 42
[ "$LAUNCH_VALUE" = "from-launch" ] || exit 43
[ "$PLUGIN_ROOT" = "/plugins/example" ] || exit 44
[ "$PLUGIN_DATA" = "/data/example" ] || exit 45
[ -n "$PATH" ] || exit 46
[ -z "${ORCA_MCP_AMBIENT_SECRET+x}" ] || exit 47
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"structured","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
"#;

const INHERITED_ENV_SERVER: &str = r#"#!/bin/sh
[ "$ORCA_MCP_COMPAT_MARKER" = "inherited" ] || exit 51
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"compat","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
"#;

const CLEANUP_SERVER: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$1"
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"cleanup","version":"0.0.0"}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","inputSchema":{"type":"object"}}]}}'
read _call
exec sleep 10
"#;

fn environment_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn one_tool_server(server: &str, remote: &str) -> String {
    format!(
        r#"#!/bin/sh
read _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"{server}","version":"0.0.0"}}}}}}'
read _initialized
read _list
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{remote}","inputSchema":{{"type":"object"}}}}]}}}}'
"#
    )
}

fn script(name: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orca-mcp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.sh");
    std::fs::write(&path, contents).unwrap();
    path
}

fn ctx(tool_name: &str) -> ToolContext {
    ToolContext {
        call_id: "call-1".into(),
        tool_name: tool_name.into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn structured_stdio_launch_uses_literal_args_cwd_and_sanitized_env() {
    let _environment = environment_lock().lock().unwrap();
    let path = script("structured-launch", STRUCTURED_LAUNCH_SERVER);
    let cwd = path.parent().unwrap().join("working-directory");
    std::fs::create_dir_all(&cwd).unwrap();
    let expected_cwd = cwd.canonicalize().unwrap();
    let original_secret = std::env::var_os("ORCA_MCP_AMBIENT_SECRET");
    std::env::set_var("ORCA_MCP_AMBIENT_SECRET", "must-not-reach-child");

    let launch = StdioLaunch {
        command: "/bin/sh".into(),
        args: vec![
            path.display().to_string(),
            "--literal=argument with spaces;$(not-a-shell)".into(),
        ],
        env: BTreeMap::from([
            ("EXPECTED_CWD".into(), expected_cwd.display().to_string()),
            ("LAUNCH_VALUE".into(), "from-launch".into()),
            ("PLUGIN_DATA".into(), "/data/example".into()),
            ("PLUGIN_ROOT".into(), "/plugins/example".into()),
        ]),
        cwd: Some(cwd),
        environment: ProcessEnvironment::Sanitized,
    };
    let result = McpClient::connect_stdio("structured", &launch).await;
    match original_secret {
        Some(value) => std::env::set_var("ORCA_MCP_AMBIENT_SECRET", value),
        None => std::env::remove_var("ORCA_MCP_AMBIENT_SECRET"),
    }
    result.unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn compatibility_connect_preserves_the_inherited_environment() {
    let _environment = environment_lock().lock().unwrap();
    let path = script("compatibility-environment", INHERITED_ENV_SERVER);
    let original_marker = std::env::var_os("ORCA_MCP_COMPAT_MARKER");
    std::env::set_var("ORCA_MCP_COMPAT_MARKER", "inherited");

    let result = McpClient::connect("compat", &format!("sh {}", path.display())).await;
    match original_marker {
        Some(value) => std::env::set_var("ORCA_MCP_COMPAT_MARKER", value),
        None => std::env::remove_var("ORCA_MCP_COMPAT_MARKER"),
    }
    result.unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn deadline_closes_the_connection_instead_of_leaving_a_stale_exchange() {
    let path = script("deadline", CANCEL_SERVER);
    let connection = McpClient::connect("deadline", &format!("sh {}", path.display()))
        .await
        .unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("deadline".into(), connection).unwrap();
    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    selector
        .call(
            json!({ "name": "mcp__deadline__wait" }),
            &ctx("mcp_select_tool"),
        )
        .await
        .unwrap();
    let tool = catalog.server_tools("deadline").into_iter().next().unwrap();
    let mut call_ctx = ctx("mcp__deadline__wait");
    call_ctx.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(20));
    assert_eq!(
        tool.call(json!({}), &call_ctx).await.unwrap_err().message,
        "deadline exceeded"
    );
    let second = tool
        .call(json!({}), &ctx("mcp__deadline__wait"))
        .await
        .unwrap_err();
    assert!(second.message.contains("interrupted request"), "{second:?}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn cancellation_terminates_the_spawned_mcp_child() {
    let path = script("child-cleanup", CLEANUP_SERVER);
    let pid_file = path.parent().unwrap().join("child.pid");
    let launch = StdioLaunch {
        command: "/bin/sh".into(),
        args: vec![path.display().to_string(), pid_file.display().to_string()],
        env: BTreeMap::new(),
        cwd: None,
        environment: ProcessEnvironment::Sanitized,
    };
    let connection = McpClient::connect_stdio("cleanup", &launch).await.unwrap();
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("cleanup".into(), connection).unwrap();
    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    selector
        .call(
            json!({ "name": "mcp__cleanup__wait" }),
            &ctx("mcp_select_tool"),
        )
        .await
        .unwrap();
    let tool = catalog.server_tools("cleanup").into_iter().next().unwrap();
    let call_ctx = ctx("mcp__cleanup__wait");
    let cancellation = call_ctx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
    });
    assert_eq!(
        tool.call(json!({}), &call_ctx).await.unwrap_err().message,
        "cancelled"
    );
    let pid = pid.trim().to_string();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("MCP child was not terminated after cancellation");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn connects_lists_and_calls_through_a_stdio_server() {
    let dir = std::env::temp_dir().join(format!("orca-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake-mcp.sh");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let connection = McpClient::connect("fake", &format!("sh {}", script.display()))
        .await
        .unwrap();
    let tools = connection.tools();
    assert_eq!(tools.len(), 1);

    let schema = tools[0].schema();
    assert_eq!(schema.name, "mcp__fake__echo");
    assert_eq!(schema.description, "echo back");
    assert_eq!(schema.parameters["type"], "object");

    let direct_error = tools[0]
        .call(json!({ "text": "hi" }), &ctx(&schema.name))
        .await
        .unwrap_err();
    assert!(direct_error.message.contains("mcp_select_tool"));
    let invalid_arguments = tools[0]
        .call(json!("not an object"), &ctx(&schema.name))
        .await
        .unwrap_err();
    assert_eq!(
        invalid_arguments.message,
        "MCP tool arguments must be an object"
    );

    let catalog = McpCatalog::new();
    catalog.insert("fake".into(), connection).unwrap();
    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    selector
        .call(
            json!({ "name": "mcp__fake__echo" }),
            &ctx("mcp_select_tool"),
        )
        .await
        .unwrap();
    let tool = catalog.server_tools("fake").into_iter().next().unwrap();
    let output = tool
        .call(json!({ "text": "hi" }), &ctx(&schema.name))
        .await
        .unwrap();
    assert_eq!(output, json!({ "content": "hello back" }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn search_requires_deliberate_terms_and_filters_metadata() {
    let dir = std::env::temp_dir().join(format!("orca-mcp-search-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake-mcp.sh");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let connection = McpClient::connect("fake", &format!("sh {}", script.display()))
        .await
        .unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("fake".into(), connection).unwrap();
    let search = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_search_tools")
        .unwrap();

    for input in [json!({}), json!({ "query": "" })] {
        let error = search
            .call(input, &ctx("mcp_search_tools"))
            .await
            .unwrap_err();
        assert_eq!(error.message, "missing or invalid query");
    }
    let filtered = search
        .call(json!({ "query": "echo back" }), &ctx("mcp_search_tools"))
        .await
        .unwrap();
    assert_eq!(filtered["tools"].as_array().unwrap().len(), 1);
    let absent = search
        .call(json!({ "query": "mcp" }), &ctx("mcp_search_tools"))
        .await
        .unwrap();
    assert!(absent["tools"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

struct ObservedTools(Arc<Mutex<Vec<Vec<String>>>>);

#[async_trait]
impl Model for ObservedTools {
    async fn generate(
        &self,
        _context: &Context,
        tools: &[ToolSchema],
    ) -> Result<ModelResponse, ModelError> {
        self.0
            .lock()
            .unwrap()
            .push(tools.iter().map(|schema| schema.name.clone()).collect());
        Ok(ModelResponse::final_text("ok"))
    }
}

#[tokio::test]
async fn selection_reaches_the_next_provider_request_without_core_changes() {
    let dir = std::env::temp_dir().join(format!("orca-mcp-lazy-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake-mcp.sh");
    std::fs::write(&script, FAKE_SERVER).unwrap();

    let connection = McpClient::connect("fake", &format!("sh {}", script.display()))
        .await
        .unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("fake".into(), connection).unwrap();
    let schemas: Vec<ToolSchema> = catalog
        .interface_tools()
        .into_iter()
        .chain(catalog.server_tools("fake"))
        .map(|tool| tool.schema())
        .collect();
    assert_eq!(schemas.len(), 4);

    let observed = Arc::new(Mutex::new(Vec::new()));
    let model = McpModel::new(ObservedTools(observed.clone()), catalog.clone());
    let context = Context::new();
    model.generate(&context, &schemas).await.unwrap();

    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    selector
        .call(
            json!({ "name": "mcp__fake__echo" }),
            &ctx("mcp_select_tool"),
        )
        .await
        .unwrap();
    model.generate(&context, &schemas).await.unwrap();

    let observed = observed.lock().unwrap();
    assert_eq!(
        observed[0],
        ["mcp_search_tools", "mcp_select_tool", "mcp_features"]
    );
    assert_eq!(
        observed[1],
        [
            "mcp_search_tools",
            "mcp_select_tool",
            "mcp_features",
            "mcp__fake__echo"
        ]
    );

    drop(observed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn follows_all_tool_list_pages() {
    let path = script("pages", PAGED_SERVER);
    let connection = McpClient::connect("paged", &format!("sh {}", path.display()))
        .await
        .unwrap();
    let names: Vec<String> = connection
        .tools()
        .iter()
        .map(|tool| tool.schema().name)
        .collect();
    assert_eq!(names, ["mcp__paged__one", "mcp__paged__two"]);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn catalog_rejects_cross_server_generated_name_collisions() {
    let first_script = one_tool_server("a__b", "c");
    let second_script = one_tool_server("a", "b__c");
    let first_path = script("collision-one", &first_script);
    let second_path = script("collision-two", &second_script);
    let first = McpClient::connect("a__b", &format!("sh {}", first_path.display()))
        .await
        .unwrap();
    let second = McpClient::connect("a", &format!("sh {}", second_path.display()))
        .await
        .unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("a__b".into(), first).unwrap();
    let error = catalog.insert("a".into(), second).unwrap_err();
    assert!(error.to_string().contains("duplicate MCP tool name"));
    assert_eq!(catalog.server_tools("a__b").len(), 1);
    assert!(catalog.server_tools("a").is_empty());
    let _ = std::fs::remove_dir_all(first_path.parent().unwrap());
    let _ = std::fs::remove_dir_all(second_path.parent().unwrap());
}

#[tokio::test]
async fn cancellation_closes_the_connection_instead_of_leaving_a_stale_exchange() {
    let path = script("cancel", CANCEL_SERVER);
    let connection = McpClient::connect("cancel", &format!("sh {}", path.display()))
        .await
        .unwrap();
    let catalog = McpCatalog::new();
    catalog.insert("cancel".into(), connection).unwrap();
    let selector = catalog
        .interface_tools()
        .into_iter()
        .find(|tool| tool.schema().name == "mcp_select_tool")
        .unwrap();
    selector
        .call(
            json!({ "name": "mcp__cancel__wait" }),
            &ctx("mcp_select_tool"),
        )
        .await
        .unwrap();
    let tool = catalog.server_tools("cancel").into_iter().next().unwrap();
    let call_ctx = ctx("mcp__cancel__wait");
    let cancellation = call_ctx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancellation.cancel();
    });
    assert_eq!(
        tool.call(json!({}), &call_ctx).await.unwrap_err().message,
        "cancelled"
    );
    let second = tool
        .call(json!({}), &ctx("mcp__cancel__wait"))
        .await
        .unwrap_err();
    assert!(second.message.contains("interrupted request"), "{second:?}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_command_that_exits_immediately_fails_the_handshake() {
    let Err(err) = McpClient::connect("dead", "true").await else {
        panic!("expected the handshake to fail");
    };
    assert!(
        matches!(err, McpError::Protocol(ref m) if m.contains("closed")),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn empty_and_unspawnable_commands_are_errors() {
    let Err(err) = McpClient::connect("blank", "   ").await else {
        panic!("expected an empty command to be rejected");
    };
    assert!(matches!(err, McpError::Protocol(_)), "got: {err}");

    let Err(err) = McpClient::connect("missing", "orca-no-such-binary-xyz").await else {
        panic!("expected the spawn to fail");
    };
    assert!(matches!(err, McpError::Spawn(_)), "got: {err}");
}
