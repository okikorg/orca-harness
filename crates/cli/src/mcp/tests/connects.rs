use super::*;

#[tokio::test]
async fn malformed_existing_registration_is_rejected_before_spawn() {
    let fixture = Fixture::new("invalid-launch");
    let servers = McpServers::with_plugin_snapshot(PluginSnapshot::empty());
    let marker = fixture.root.join("must-not-exist");
    for (name, command) in [
        ("--transport", format!("touch {}", marker.display())),
        ("openrouter", "--url https://example.com/mcp".into()),
    ] {
        let (desired, _) = servers.desired_servers(&[crate::config::McpServer {
            name: name.into(),
            command,
            enabled: true,
        }]);
        let result = servers.connect(&desired[0]).await;
        assert!(matches!(result, Err(ref error) if error.contains("invalid")));
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn agent_catalog_snapshot_does_not_discover_unregistered_tools() {
    let fixture = Fixture::new("catalog-snapshot");
    let servers = gated_servers(&fixture, 1);
    let before = servers.catalog().snapshot();
    release(&fixture, 0);
    servers.reload().await;
    let ready = servers.catalog().snapshot();
    assert_eq!(before.tools().len(), 3, "old agent keeps only interfaces");
    assert_eq!(ready.tools().len(), 4, "new agent registers remote tool");
    servers.catalog().remove("server_0");
    assert_eq!(ready.tools().len(), 4, "reload cannot mutate running agent");
    assert_eq!(servers.catalog().tools().len(), 3);
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture condition timed out");
}

fn gated_servers(fixture: &Fixture, count: usize) -> McpServers {
    let mut snapshot = PluginSnapshot::empty();
    for index in 0..count {
        let script = fixture.root.join(format!("server-{index}.sh"));
        fs::write(
            &script,
            format!(
                "echo $$ > '{root}/pid-{index}'\nwhile [ ! -f '{root}/release-{index}' ]; do sleep 0.01; done\n{STANDALONE_SERVER}",
                root = fixture.root.display(),
            ),
        )
        .unwrap();
        snapshot.servers.push(PluginServer {
            name: format!("server_{index}"),
            plugin: "fixture".into(),
            server: index.to_string(),
            data: fixture.data_root.clone(),
            launch: StdioLaunch {
                command: "sh".into(),
                args: vec![script.to_string_lossy().into_owned()],
                env: BTreeMap::new(),
                cwd: None,
                environment: ProcessEnvironment::Inherit,
            },
        });
    }
    McpServers::with_plugin_snapshot(snapshot)
}

fn release(fixture: &Fixture, index: usize) {
    fs::write(fixture.root.join(format!("release-{index}")), "").unwrap();
}

async fn assert_children_gone(fixture: &Fixture, count: usize) {
    for index in 0..count {
        let pid = fs::read_to_string(fixture.root.join(format!("pid-{index}"))).unwrap();
        wait_until(|| {
            !std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        })
        .await;
    }
}

#[tokio::test]
async fn connects_overlap_with_bound_and_publish_in_input_order() {
    let fixture = Fixture::new("bounded-connects");
    let servers = gated_servers(&fixture, 5);
    let control = async {
        wait_until(|| (0..4).all(|i| fixture.root.join(format!("pid-{i}")).exists())).await;
        assert!(!fixture.root.join("pid-4").exists());
        // Later connections may finish first, but must not be published first.
        for i in 1..4 {
            release(&fixture, i);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(servers.connections.read().unwrap().is_empty());
        assert!(!fixture.root.join("pid-4").exists());
        release(&fixture, 0);
        wait_until(|| fixture.root.join("pid-4").exists()).await;
        release(&fixture, 4);
    };
    let (lines, ()) = tokio::join!(servers.reload(), control);
    assert_eq!(
        lines,
        (0..5)
            .map(|i| format!("Plugin fixture MCP {i} connected · 1 tool"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        &tool_names(&servers)[3..],
        (0..5)
            .map(|i| format!("mcp__server_{i}__echo"))
            .collect::<Vec<_>>()
    );
    assert!(servers.reload().await.is_empty());
    drop(servers);
    assert_children_gone(&fixture, 5).await;
}

#[tokio::test]
async fn cancelling_reload_drops_all_in_flight_children() {
    let fixture = Fixture::new("cancel-connects");
    let servers = gated_servers(&fixture, 5);
    let mut reload = Box::pin(servers.reload());
    tokio::select! {
        _ = &mut reload => panic!("blocked servers unexpectedly connected"),
        _ = wait_until(|| (0..4).all(|i| fixture.root.join(format!("pid-{i}")).exists())) => {}
    }
    drop(reload);
    assert_children_gone(&fixture, 4).await;
    assert!(!fixture.root.join("pid-4").exists());
    assert!(servers.connections.read().unwrap().is_empty());
}

#[tokio::test]
async fn total_timeout_covers_initialize_and_tools_list_and_is_cached() {
    for stage in ["initialize", "tools/list"] {
        let fixture = Fixture::new(stage.replace('/', "-").as_str());
        let servers = gated_servers(&fixture, 1);
        if stage == "tools/list" {
            let script = fixture.root.join("server-0.sh");
            // Complete initialize, then block in tools/list instead.
            fs::write(
                script,
                format!(
                    "echo $$ > '{}/pid-0'\n{}",
                    fixture.root.display(),
                    STANDALONE_SERVER.replace("read _list", "read _list\nread _never_sent")
                ),
            )
            .unwrap();
        }
        let lines = servers
            .reload_with_timeout(Duration::from_millis(300))
            .await;
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("connection timed out after 300ms"),
            "{lines:?}"
        );
        assert!(lines[0].contains("check the server command"));
        assert!(matches!(
            servers.state("server_0"),
            Some(McpState::Failed(_))
        ));
        assert_children_gone(&fixture, 1).await;
        assert!(
            servers.reload().await.is_empty(),
            "timeout failure must be cached"
        );
    }
}

#[tokio::test]
async fn earlier_slow_server_wins_tool_collision_and_loser_retries_on_change() {
    let fixture = Fixture::new("ordered-collision");
    let mut servers = gated_servers(&fixture, 2);
    servers.plugins.servers[0].name = "a".into();
    servers.plugins.servers[1].name = "a__b".into();
    let script = fixture.root.join("server-0.sh");
    let body = fs::read_to_string(&script).unwrap();
    fs::write(
        script,
        body.replace("\"name\":\"echo\"", "\"name\":\"b__echo\""),
    )
    .unwrap();
    release(&fixture, 1);
    let control = async {
        wait_until(|| fixture.root.join("pid-1").exists()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        release(&fixture, 0);
    };
    let (lines, ()) = tokio::join!(servers.reload(), control);
    assert_eq!(lines[0], "Plugin fixture MCP 0 connected · 1 tool");
    assert!(lines[1].contains("duplicate MCP tool name"), "{lines:?}");
    assert_eq!(servers.state("a"), Some(McpState::Connected(1)));
    assert!(matches!(servers.state("a__b"), Some(McpState::Failed(_))));
    assert!(servers.reload().await.is_empty());
    servers.plugins.servers.remove(0);
    assert_eq!(
        servers.reload().await,
        ["Plugin fixture MCP 1 connected · 1 tool"]
    );
    assert_eq!(servers.state("a__b"), Some(McpState::Connected(1)));
}
