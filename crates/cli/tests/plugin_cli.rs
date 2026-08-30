use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "orcacode-plugin-cli-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self {
            config: root.join("config"),
            root,
        }
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_orcacode"))
            .args(args)
            .current_dir(cwd)
            .env("ORCA_CONFIG_DIR", &self.config)
            .env("ORCA_THEME", "definitely-invalid")
            .output()
            .unwrap()
    }

    fn plugin(&self, name: &str) -> PathBuf {
        let root = self.root.join(name);
        fs::create_dir_all(&root).unwrap();
        write_manifest(&root, name);
        root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_manifest(root: &Path, name: &str) {
    fs::write(
        root.join("plugin.json"),
        format!(
            r#"{{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "{name}",
  "version": "0.1.0",
  "description": "fixture"
}}
"#
        ),
    )
    .unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "stderr: {}", stderr(output));
}

#[test]
fn install_list_inspect_toggle_and_uninstall_preserve_owned_data() {
    let fixture = Fixture::new("state");
    fs::create_dir_all(&fixture.config).unwrap();
    fs::write(
        fixture.config.join("config.json"),
        r#"{"future":{"kept":true}}"#,
    )
    .unwrap();
    let alpha = fixture.plugin("alpha-plugin");
    let zeta = fixture.plugin("zeta-plugin");

    assert_success(&fixture.run(
        &fixture.root,
        &["plugin", "install", zeta.to_str().unwrap()],
    ));
    assert_success(&fixture.run(
        &fixture.root,
        &["plugin", "install", alpha.to_str().unwrap()],
    ));
    let list = fixture.run(&fixture.root, &["plugin", "list"]);
    assert_success(&list);
    let list = stdout(&list);
    assert!(list.find("alpha-plugin").unwrap() < list.find("zeta-plugin").unwrap());
    assert!(list.contains("disabled"));

    let inspect = fixture.run(&fixture.root, &["plugin", "inspect", "alpha-plugin"]);
    assert_success(&inspect);
    assert!(stdout(&inspect).contains(alpha.canonicalize().unwrap().to_str().unwrap()));
    let config_path = fixture.config.join("config.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    let non_canonical = alpha.join("..").join("alpha-plugin");
    saved["plugins"]["alpha-plugin"]["root"] = serde_json::json!(non_canonical.to_str().unwrap());
    saved["plugins"]["alpha-plugin"]["futureEntry"] = serde_json::json!("kept");
    fs::write(&config_path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();

    assert_success(&fixture.run(&fixture.root, &["plugin", "enable", "alpha-plugin"]));
    let enabled: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(enabled["plugins"]["alpha-plugin"]["enabled"], true);
    assert_eq!(
        enabled["plugins"]["alpha-plugin"]["root"],
        alpha.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(enabled["plugins"]["alpha-plugin"]["futureEntry"], "kept");
    assert!(!fixture.config.join("plugin-data/alpha-plugin").exists());
    assert_success(&fixture.run(&fixture.root, &["plugin", "disable", "alpha-plugin"]));
    let disabled: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(disabled["plugins"]["alpha-plugin"]["enabled"], false);
    assert_success(&fixture.run(
        &fixture.root,
        &["plugin", "install", alpha.to_str().unwrap()],
    ));

    let conflicting = fixture.root.join("conflicting-alpha");
    fs::create_dir_all(&conflicting).unwrap();
    write_manifest(&conflicting, "alpha-plugin");
    let collision = fixture.run(
        &fixture.root,
        &["plugin", "install", conflicting.to_str().unwrap()],
    );
    assert!(!collision.status.success());
    assert!(stderr(&collision).contains("different root"));

    let data = fixture.config.join("plugin-data/alpha-plugin");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("state.txt"), "kept").unwrap();
    let uninstall = fixture.run(&fixture.root, &["plugin", "uninstall", "alpha-plugin"]);
    assert_success(&uninstall);
    assert!(stdout(&uninstall).contains(data.to_str().unwrap()));
    assert!(alpha.exists());
    assert_eq!(fs::read_to_string(data.join("state.txt")).unwrap(), "kept");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.config.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["future"]["kept"], true);
    assert!(saved["plugins"].get("alpha-plugin").is_none());
}

#[test]
fn validate_is_static_but_test_handshakes_and_lists_tools() {
    let fixture = Fixture::new("probe");
    let plugin = fixture.plugin("probe-plugin");
    let marker = plugin.join("executed");
    let hook_marker = plugin.join("hook-executed");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(plugin.join("plugin.json")).unwrap()).unwrap();
    manifest["futureField"] = serde_json::json!(true);
    fs::write(
        plugin.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let script = plugin.join("server.sh");
    fs::create_dir_all(plugin.join("skills/probe-workflow")).unwrap();
    fs::write(
        plugin.join("skills/probe-workflow/SKILL.md"),
        "---\nname: probe-workflow\ndescription: Verify plugin skill discovery\n---\n\nUse the probe workflow.\n",
    )
    .unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\nread init\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"fixture\",\"version\":\"1\"}}}}}}'\nread initialized\nread tools\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[{{\"name\":\"echo\",\"description\":\"echo\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}'\nread rest\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::write(
        plugin.join("mcp.json"),
        r#"{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "echo": {"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh"],"cwd":"${PLUGIN_ROOT}"},
    "second": {"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh"],"cwd":"${PLUGIN_ROOT}"}
  }
}
"#,
    )
    .unwrap();
    fs::create_dir_all(plugin.join("io.github.okikorg.orcacode")).unwrap();
    fs::write(
        plugin.join("hook.sh"),
        format!(
            "#!/bin/sh\ntouch '{}'\nread input\nprintf '%s' '{{}}'\n",
            hook_marker.display()
        ),
    )
    .unwrap();
    fs::write(
        plugin.join("io.github.okikorg.orcacode/hooks.json"),
        r#"{"version":1,"hooks":{"before_model":[{"command":"sh","args":["${PLUGIN_ROOT}/hook.sh"]}]}}"#,
    )
    .unwrap();

    let validate = fixture.run(
        &fixture.root,
        &["plugin", "validate", plugin.to_str().unwrap()],
    );
    assert_success(&validate);
    assert!(stdout(&validate).contains("warning:"));
    assert!(stdout(&validate).contains("skill: probe-workflow"));
    assert!(!marker.exists(), "static validation executed plugin code");
    assert!(
        !hook_marker.exists(),
        "static validation executed hook code"
    );
    let data = fixture.config.join("plugin-data/probe-plugin");
    assert!(!data.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let probe = fixture.run(&fixture.root, &["plugin", "test", plugin.to_str().unwrap()]);
    assert_success(&probe);
    assert!(marker.exists());
    assert!(hook_marker.exists());
    assert!(stdout(&probe).contains("hook before_model #1: valid"));
    assert!(stdout(&probe).contains("skill probe-workflow: valid"));
    assert!(stdout(&probe).contains("mcp__plugin__probe_plugin__echo__echo"));
    assert!(stdout(&probe).contains("mcp__plugin__probe_plugin__second__echo"));
    assert!(data.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&data).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn plugin_test_without_stdio_servers_creates_no_plugin_data() {
    let fixture = Fixture::new("no-stdio");
    let plugin = fixture.plugin("metadata-only-plugin");

    let probe = fixture.run(&fixture.root, &["plugin", "test", plugin.to_str().unwrap()]);

    assert_success(&probe);
    assert!(!fixture
        .config
        .join("plugin-data/metadata-only-plugin")
        .exists());
}

#[test]
fn plugin_must_lead_the_invocation_and_path_defaults_to_current_directory() {
    let fixture = Fixture::new("boundary");
    let plugin = fixture.plugin("current-plugin");
    let validate = fixture.run(&plugin, &["plugin", "validate"]);
    assert_success(&validate);
    assert!(stdout(&validate).contains("current-plugin"));

    let with_run_flag = fixture.run(&plugin, &["--theme", "mono", "plugin", "list"]);
    assert_success(&with_run_flag);

    let misplaced = fixture.run(&plugin, &["not-plugin", "plugin", "list"]);
    assert!(!misplaced.status.success());
    assert!(stderr(&misplaced).contains("unknown flag: not-plugin"));
}

#[test]
fn python_and_typescript_scaffolds_have_expected_layout_and_do_not_overwrite() {
    let fixture = Fixture::new("scaffolds");
    let python = fixture.run(
        &fixture.root,
        &["plugin", "init", "my-python.plugin", "--py"],
    );
    assert_success(&python);
    let py = fixture.root.join("my-python.plugin");
    for path in [
        "plugin.json",
        "mcp.json",
        "pyproject.toml",
        "README.md",
        ".gitignore",
        "skills/.gitkeep",
        "io.github.okikorg.orcacode/hooks.json",
        "src/my_python_plugin/__init__.py",
        "src/my_python_plugin/server.py",
        "tests/test_server.py",
    ] {
        assert!(py.join(path).is_file(), "missing Python scaffold {path}");
    }
    assert_success(&fixture.run(&fixture.root, &["plugin", "validate", py.to_str().unwrap()]));
    let py_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(py.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(py_mcp["mcpServers"]["echo"]["type"], "stdio");
    assert_eq!(py_mcp["mcpServers"]["echo"]["command"], "uv");
    let python_readme = fs::read_to_string(py.join("README.md")).unwrap();
    for guidance in [
        "Orcacode does not itself run dependency installers",
        "generated `uv` child may resolve dependencies into `${PLUGIN_DATA}`",
        "first `orcacode plugin test` or enabled start",
        "For offline use",
        "before enabling",
    ] {
        assert!(
            python_readme.contains(guidance),
            "missing guidance: {guidance}"
        );
    }

    let ts = fixture.run(
        &fixture.root,
        &["plugin", "init", "my-ts-plugin", "--typescript"],
    );
    assert_success(&ts);
    let ts = fixture.root.join("my-ts-plugin");
    for path in [
        "plugin.json",
        "mcp.json",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "README.md",
        ".gitignore",
        "skills/.gitkeep",
        "io.github.okikorg.orcacode/hooks.json",
        "src/index.ts",
        "test/server.test.ts",
    ] {
        assert!(
            ts.join(path).is_file(),
            "missing TypeScript scaffold {path}"
        );
    }
    assert_success(&fixture.run(&fixture.root, &["plugin", "validate", ts.to_str().unwrap()]));
    let ts_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(ts.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(ts_mcp["mcpServers"]["echo"]["type"], "stdio");
    assert_eq!(ts_mcp["mcpServers"]["echo"]["command"], "node");
    assert_eq!(
        ts_mcp["mcpServers"]["echo"]["args"][0],
        "${PLUGIN_ROOT}/dist/server.mjs"
    );
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(ts.join("package-lock.json")).unwrap()).unwrap();
    assert_eq!(lock["lockfileVersion"], 3);
    let package: serde_json::Value =
        serde_json::from_slice(&fs::read(ts.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        lock["packages"][""]["dependencies"],
        package["dependencies"]
    );
    assert_eq!(
        lock["packages"][""]["devDependencies"],
        package["devDependencies"]
    );
    assert!(package["dependencies"]["@modelcontextprotocol/sdk"]
        .as_str()
        .unwrap()
        .starts_with("^1."));
    assert!(package["devDependencies"].get("vitest").is_some());
    assert!(package["devDependencies"].get("esbuild").is_some());
    for dependency in package["dependencies"]
        .as_object()
        .unwrap()
        .keys()
        .chain(package["devDependencies"].as_object().unwrap().keys())
    {
        assert!(
            lock["packages"]
                .get(format!("node_modules/{dependency}"))
                .is_some(),
            "lock is missing {dependency}"
        );
    }
    assert_eq!(package["engines"]["node"], ">=20");
    for (path, engine) in [
        ("node_modules/@modelcontextprotocol/sdk", ">=18"),
        ("node_modules/vitest", "^18.0.0 || >=20.0.0"),
        (
            "node_modules/vitest/node_modules/vite",
            "^18.0.0 || >=20.0.0",
        ),
        ("node_modules/vite-node", "^18.0.0 || >=20.0.0"),
        (
            "node_modules/vite-node/node_modules/vite",
            "^18.0.0 || >=20.0.0",
        ),
        ("node_modules/esbuild", ">=18"),
        ("node_modules/typescript", ">=14.17"),
    ] {
        assert_eq!(lock["packages"][path]["engines"]["node"], engine, "{path}");
    }

    let no_build = fixture.run(&fixture.root, &["plugin", "test", ts.to_str().unwrap()]);
    assert!(!no_build.status.success());
    assert!(stderr(&no_build).contains("build"), "{}", stderr(&no_build));

    fs::write(ts.join("keep.txt"), "untouched").unwrap();
    let overwrite = fixture.run(&fixture.root, &["plugin", "init", "my-ts-plugin", "--ts"]);
    assert!(!overwrite.status.success());
    assert_eq!(
        fs::read_to_string(ts.join("keep.txt")).unwrap(),
        "untouched"
    );

    fs::create_dir_all(fixture.root.join("empty-plugin")).unwrap();
    assert_success(&fixture.run(
        &fixture.root,
        &["plugin", "init", "empty-plugin", "--python"],
    ));
    let invalid = fixture.run(&fixture.root, &["plugin", "init", "Bad_Name", "--py"]);
    assert!(!invalid.status.success());
}

#[test]
fn enable_revalidates_before_persisting_the_state() {
    let fixture = Fixture::new("revalidate");
    let plugin = fixture.plugin("changing-plugin");
    assert_success(&fixture.run(
        &fixture.root,
        &["plugin", "install", plugin.to_str().unwrap()],
    ));
    fs::write(plugin.join("plugin.json"), "not json").unwrap();

    let enable = fixture.run(&fixture.root, &["plugin", "enable", "changing-plugin"]);
    assert!(!enable.status.success());
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.config.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["plugins"]["changing-plugin"]["enabled"], false);
}
