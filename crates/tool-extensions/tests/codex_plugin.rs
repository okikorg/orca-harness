use orca_harness_tool_extensions::agent_plugins::{parse_codex_plugin, CodexPluginTransport};
use serde_json::json;

const ROOT: &str = "/workspace/.orca/plugins/notes";

fn manifest(extra: serde_json::Value) -> Vec<u8> {
    let mut value = json!({"name": "notes", "version": "1.0.0", "description": "Team notes"});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::to_vec(&value).unwrap()
}

const SKILL: &[u8] = b"---\nname: summarize\ndescription: Summarize notes\n---\nSteps.\n";

#[test]
fn codex_plugin_parses_the_documented_folder_layout() {
    let manifest = manifest(json!({"skills": "./skills/", "mcpServers": "./.mcp.json"}));
    let mcp = serde_json::to_vec(&json!({"mcpServers": {
        "notes-db": {"type": "stdio", "command": "./bin/server", "args": ["--ro"], "cwd": "./data", "env_vars": ["NOTES_TOKEN"]},
        "docs": {"type": "http", "url": "https://example.com/mcp", "bearer_token_env_var": "DOCS_TOKEN", "http_headers": {"X-Team": "a"}}
    }}))
    .unwrap();
    let entries: Vec<(&str, &[u8])> = vec![
        ("notes/.codex-plugin/plugin.json", &manifest),
        ("notes/.mcp.json", &mcp),
        ("notes/skills/summarize/SKILL.md", SKILL),
        ("notes/bin/server", b"#!/bin/sh\n"),
    ];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    assert_eq!(plugin.prefix, "notes/");
    assert_eq!(
        (plugin.name.as_str(), plugin.description.as_str()),
        ("notes", "Team notes")
    );
    assert_eq!(plugin.skills.len(), 1);
    assert_eq!(
        (
            plugin.skills[0].name.as_str(),
            plugin.skills[0].dir.as_str()
        ),
        ("summarize", "skills/summarize")
    );
    let servers: Vec<_> = plugin
        .mcp_servers
        .iter()
        .map(|s| (s.id.as_str(), &s.transport))
        .collect();
    assert_eq!(
        servers,
        vec![
            (
                "plugin__notes__docs",
                &CodexPluginTransport::Http {
                    url: "https://example.com/mcp".into(),
                    bearer_token_env_var: Some("DOCS_TOKEN".into()),
                    headers: [("X-Team".to_owned(), "a".to_owned())].into(),
                }
            ),
            (
                "plugin__notes__notes_db",
                &CodexPluginTransport::Stdio {
                    command: format!("{ROOT}/bin/server"),
                    args: vec!["--ro".into()],
                    cwd: format!("{ROOT}/data"),
                    env_vars: vec!["NOTES_TOKEN".into()],
                }
            ),
        ]
    );
    assert!(plugin.warnings.is_empty(), "{:?}", plugin.warnings);
}

#[test]
fn codex_plugin_accepts_a_root_manifest_and_default_components() {
    let manifest = manifest(json!({}));
    let mcp = br#"{"mcpServers": {"local": {"command": "python3", "args": ["server.py"]}}}"#;
    let entries: Vec<(&str, &[u8])> = vec![
        (".codex-plugin/plugin.json", &manifest),
        (".mcp.json", mcp),
        ("skills/summarize/SKILL.md", SKILL),
    ];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    assert_eq!(plugin.prefix, "");
    assert_eq!(plugin.skills.len(), 1);
    assert_eq!(
        plugin.mcp_servers[0].transport,
        CodexPluginTransport::Stdio {
            command: "python3".into(),
            args: vec!["server.py".into()],
            cwd: ROOT.into(),
            env_vars: vec![],
        }
    );
}

#[test]
fn codex_plugin_rejects_paths_that_escape_the_plugin() {
    let manifest = manifest(json!({}));
    for bad in ["../x", "/etc/passwd", "a/../b", "a//b", "a\\b"] {
        let entries: Vec<(&str, &[u8])> =
            vec![(".codex-plugin/plugin.json", &manifest), (bad, b"x")];
        assert!(parse_codex_plugin(&entries, ROOT).is_err(), "{bad}");
    }
    for field in ["skills", "mcpServers"] {
        for bad in ["../outside", "skills", "./a/../../b"] {
            let manifest = self::manifest(json!({ field: bad }));
            let entries: Vec<(&str, &[u8])> = vec![(".codex-plugin/plugin.json", &manifest)];
            assert!(parse_codex_plugin(&entries, ROOT).is_err(), "{field}={bad}");
        }
    }
    let manifest = self::manifest(json!({}));
    let mcp = br#"{"mcpServers": {"a": {"command": "./../../bin/sh"}, "b": {"command": "x", "cwd": "../.."}, "c": {"command": "x", "cwd": "/"}}}"#;
    let entries: Vec<(&str, &[u8])> =
        vec![(".codex-plugin/plugin.json", &manifest), (".mcp.json", mcp)];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    assert!(plugin.mcp_servers.is_empty(), "{:?}", plugin.mcp_servers);
    assert_eq!(plugin.warnings.len(), 3, "{:?}", plugin.warnings);
}

#[test]
fn codex_plugin_requires_one_manifest_and_a_valid_name() {
    let manifest = manifest(json!({}));
    let two: Vec<(&str, &[u8])> = vec![
        ("a/.codex-plugin/plugin.json", &manifest),
        ("b/.codex-plugin/plugin.json", &manifest),
    ];
    assert!(parse_codex_plugin(&two, ROOT).is_err());
    let none: Vec<(&str, &[u8])> = vec![("plugin.json", &manifest)];
    assert!(parse_codex_plugin(&none, ROOT).is_err());
    for name in ["", "../x", "a/b", "-x", "a..b"] {
        let bad = serde_json::to_vec(&json!({"name": name, "description": "d"})).unwrap();
        let entries: Vec<(&str, &[u8])> = vec![(".codex-plugin/plugin.json", &bad)];
        assert!(parse_codex_plugin(&entries, ROOT).is_err(), "{name:?}");
    }
    assert!(parse_codex_plugin(&[(".codex-plugin/plugin.json", &manifest)], "relative").is_err());
}

#[test]
fn codex_plugin_warns_on_unsupported_components_and_skips_bad_servers() {
    let manifest = manifest(json!({"hooks": "./hooks.json", "apps": "./.app.json"}));
    let mcp = br#"{"mcpServers": {
        "off": {"command": "x", "enabled": false},
        "sse": {"type": "sse", "url": "https://example.com"},
        "creds": {"type": "http", "url": "https://user:pw@example.com/mcp"},
        "timeouts": {"command": "x", "startup_timeout_sec": 5, "env": {"A": "b"}},
        "badenv": {"command": "x", "env_vars": ["1BAD"]}
    }}"#;
    let entries: Vec<(&str, &[u8])> =
        vec![(".codex-plugin/plugin.json", &manifest), (".mcp.json", mcp)];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    let ids: Vec<_> = plugin.mcp_servers.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["plugin__notes__timeouts"]);
    let scopes: Vec<_> = plugin.warnings.iter().map(|w| w.to_string()).collect();
    for expected in [
        "hooks:",
        "apps:",
        "mcpServers.sse:",
        "mcpServers.creds:",
        "mcpServers.badenv:",
        "field env ",
        "field startup_timeout_sec ",
    ] {
        assert!(
            scopes.iter().any(|w| w.contains(expected)),
            "{expected} in {scopes:?}"
        );
    }
}

#[test]
fn codex_plugin_skips_skills_without_descriptions_or_with_duplicate_names() {
    let manifest = manifest(json!({}));
    let entries: Vec<(&str, &[u8])> = vec![
        (".codex-plugin/plugin.json", &manifest),
        ("skills/a/SKILL.md", SKILL),
        ("skills/b/SKILL.md", SKILL),
        ("skills/c/SKILL.md", b"---\nname: c\n---\n"),
        ("skills/d/nested/SKILL.md", SKILL),
    ];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    assert_eq!(plugin.skills.len(), 1);
    assert_eq!(plugin.warnings.len(), 2, "{:?}", plugin.warnings);
}

#[test]
fn codex_plugin_skips_skills_with_unusable_names() {
    let manifest = manifest(json!({}));
    let entries: Vec<(&str, &[u8])> = vec![
        (".codex-plugin/plugin.json", &manifest),
        ("skills/a/SKILL.md", SKILL),
        (
            "skills/b/SKILL.md",
            b"---\nname: \"foo bar\"\ndescription: d\n---\n",
        ),
        (
            "skills/c/SKILL.md",
            b"---\nname: ../x\ndescription: d\n---\n",
        ),
        ("skills/my skill/SKILL.md", b"---\ndescription: d\n---\n"),
    ];
    let plugin = parse_codex_plugin(&entries, ROOT).unwrap();
    let names: Vec<_> = plugin.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["summarize"]);
    let warnings: Vec<_> = plugin.warnings.iter().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    for (scope, name) in [
        ("skills.b:", "\"foo bar\""),
        ("skills.c:", "\"../x\""),
        ("skills.my skill:", "\"my skill\""),
    ] {
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with(scope) && w.contains(name)),
            "{scope} {name} in {warnings:?}"
        );
    }
}
