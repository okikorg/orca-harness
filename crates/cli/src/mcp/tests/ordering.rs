use super::*;

#[test]
fn desired_order_is_standalone_then_sorted_plugins_and_servers() {
    let fixture = Fixture::new("desired-order");
    let zeta = fixture.plugin("zeta-plugin", &["z-last"]);
    let alpha = fixture.plugin("alpha-plugin", &["z-server", "a-server"]);
    let snapshot = fixture.snapshot(vec![zeta, alpha]);
    let servers = McpServers::with_plugin_snapshot(snapshot);
    let standalone = vec![crate::config::McpServer {
        name: "standalone".into(),
        command: "missing".into(),
        enabled: true,
    }];

    let (desired, collisions) = servers.desired_servers(&standalone);

    assert!(collisions.is_empty());
    assert_eq!(
        desired
            .into_iter()
            .map(|server| server.name)
            .collect::<Vec<_>>(),
        [
            "standalone",
            "plugin__alpha_plugin__a_server",
            "plugin__alpha_plugin__z_server",
            "plugin__zeta_plugin__z_last",
        ]
    );
}
