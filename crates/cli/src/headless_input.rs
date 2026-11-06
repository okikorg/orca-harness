//! Inputs a headless caller hands over beside the prompt text: where the
//! prompt comes from, per-run MCP servers, the approval channel, and image
//! attachments. Parsed with the other flags; files and stdin are read only
//! once parsing succeeded, so flag errors never consume stdin.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use orca_harness_core::Image;
use orca_harness_tool_extensions::mcp::{ProcessEnvironment, StdioLaunch};
use serde::Deserialize;

/// Where the headless prompt text is read from when it is not given inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    /// `-p -`: read standard input to EOF.
    Stdin,
    /// `--prompt-file PATH`.
    File(PathBuf),
}

/// How a headless run answers gated tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalChannel {
    /// Deny them (the long-standing headless behaviour) unless
    /// `--auto-approve` or `--yolo` admitted them.
    #[default]
    Deny,
    /// Emit `approval_request` events on stdout and read decisions from
    /// stdin, one JSON object per line.
    Stdio,
}

impl ApprovalChannel {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "deny" => Ok(Self::Deny),
            "stdio" => Ok(Self::Stdio),
            other => Err(format!("--approvals expects deny or stdio, got {other:?}")),
        }
    }
}

/// One `--mcp-config` server: connected for this run only, never saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMcpServer {
    pub name: String,
    pub launch: StdioLaunch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

/// Parse `--mcp-config`: inline JSON when the value starts with `{`,
/// otherwise a path to a JSON file. Both the bare map
/// `{"name": {...}}` and the common `{"mcpServers": {"name": {...}}}`
/// wrapper are accepted. Servers keep their map order.
pub fn parse_mcp_config(value: &str) -> Result<Vec<RunMcpServer>, String> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        std::fs::read_to_string(value)
            .map_err(|error| format!("--mcp-config: cannot read {value}: {error}"))?
    };
    let root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)
        .map_err(|error| format!("--mcp-config: expected a JSON object: {error}"))?;
    let servers = match root.get("mcpServers") {
        Some(serde_json::Value::Object(inner)) if root.len() == 1 => inner.clone(),
        Some(_) if root.len() == 1 => {
            return Err("--mcp-config: mcpServers must be an object".into())
        }
        _ => root,
    };
    let mut parsed = Vec::with_capacity(servers.len());
    for (name, spec) in servers {
        let spec: ServerSpec = serde_json::from_value(spec)
            .map_err(|error| format!("--mcp-config: server {name}: {error}"))?;
        validate_server_name(&name)?;
        if spec.command.is_empty() {
            return Err(format!("--mcp-config: server {name}: command is empty"));
        }
        parsed.push(RunMcpServer {
            name,
            launch: StdioLaunch {
                command: spec.command,
                args: spec.args,
                env: spec.env,
                cwd: spec.cwd,
                environment: ProcessEnvironment::Inherit,
            },
        });
    }
    Ok(parsed)
}

/// Names become part of model-facing tool names, so they follow the same
/// rule as servers saved with `/mcp add`.
fn validate_server_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "--mcp-config: invalid server name {name:?}; use ASCII letters, digits, - and _, not starting with '-'"
        ))
    }
}

/// Read the prompt text from its source. An empty prompt is an error: a
/// caller that piped nothing almost certainly lost its input.
pub fn read_prompt(source: &PromptSource, stdin: impl Read) -> Result<String, String> {
    let mut text = String::new();
    match source {
        PromptSource::Stdin => {
            let mut stdin = stdin;
            stdin
                .read_to_string(&mut text)
                .map_err(|error| format!("cannot read the prompt from stdin: {error}"))?;
        }
        PromptSource::File(path) => {
            text = std::fs::read_to_string(path).map_err(|error| {
                format!("cannot read --prompt-file {}: {error}", path.display())
            })?;
        }
    }
    if text.trim().is_empty() {
        return Err(match source {
            PromptSource::Stdin => "the prompt read from stdin is empty".into(),
            PromptSource::File(path) => format!("--prompt-file {} is empty", path.display()),
        });
    }
    Ok(text)
}

/// The same cap the TUI applies to pasted and dropped images.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

pub fn image_media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// Load `--image` files. Relative paths resolve against the process's
/// working directory. Any unreadable or unsupported file is an error, so a
/// run never silently proceeds without an attachment the caller sent.
pub fn load_images(paths: &[PathBuf]) -> Result<Vec<Image>, String> {
    paths
        .iter()
        .map(|path| {
            let media_type = image_media_type(path).ok_or_else(|| {
                format!(
                    "--image {}: unsupported type; use png, jpg, jpeg, gif or webp",
                    path.display()
                )
            })?;
            let metadata = std::fs::metadata(path)
                .map_err(|error| format!("--image {}: {error}", path.display()))?;
            if !metadata.is_file() {
                return Err(format!("--image {}: not a file", path.display()));
            }
            if metadata.len() > MAX_IMAGE_BYTES {
                return Err(format!(
                    "--image {}: larger than {} MiB",
                    path.display(),
                    MAX_IMAGE_BYTES / (1024 * 1024)
                ));
            }
            let bytes = std::fs::read(path)
                .map_err(|error| format!("--image {}: {error}", path.display()))?;
            if bytes.is_empty() {
                return Err(format!("--image {}: file is empty", path.display()));
            }
            Ok(Image::base64(
                media_type,
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_keeps_paths_with_spaces_and_args_intact() {
        let servers = parse_mcp_config(
            r#"{"docs": {"command": "/tmp/my tools/server", "args": ["--root", "a b"], "env": {"K": "v w"}}}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].launch.command, "/tmp/my tools/server");
        assert_eq!(servers[0].launch.args, ["--root", "a b"]);
        assert_eq!(servers[0].launch.env["K"], "v w");
        assert_eq!(servers[0].launch.environment, ProcessEnvironment::Inherit);
    }

    #[test]
    fn mcp_config_accepts_the_mcp_servers_wrapper_and_files() {
        let dir = std::env::temp_dir().join(format!("orca mcp cfg {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("servers.json");
        std::fs::write(
            &file,
            r#"{"mcpServers": {"a": {"command": "x"}, "b": {"command": "y"}}}"#,
        )
        .unwrap();
        let servers = parse_mcp_config(file.to_str().unwrap()).unwrap();
        let names = servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["a", "b"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mcp_config_rejects_bad_names_fields_and_json() {
        for bad in [
            r#"{"bad name": {"command": "x"}}"#,
            r#"{"a": {"command": ""}}"#,
            r#"{"a": {"command": "x", "url": "https://e"}}"#,
            r#"{"a": {"args": []}}"#,
            r#"{"mcpServers": []}"#,
            "{not json",
        ] {
            assert!(parse_mcp_config(bad).is_err(), "{bad}");
        }
        assert!(parse_mcp_config("/definitely/missing.json").is_err());
    }

    #[test]
    fn prompt_reads_stdin_verbatim_and_rejects_empty_input() {
        let text = "-starts with a dash\nand spans lines\n";
        assert_eq!(
            read_prompt(&PromptSource::Stdin, text.as_bytes()).unwrap(),
            text
        );
        assert!(read_prompt(&PromptSource::Stdin, " \n".as_bytes()).is_err());
        assert!(read_prompt(&PromptSource::File("/definitely/missing".into()), &b""[..]).is_err());
    }

    #[test]
    fn images_load_as_base64_and_bad_files_fail_loudly() {
        let dir = std::env::temp_dir().join(format!("orca-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pixel.PNG");
        std::fs::write(&png, b"\x89PNG").unwrap();
        let images = load_images(std::slice::from_ref(&png)).unwrap();
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "iVBORw==");

        let text = dir.join("notes.txt");
        std::fs::write(&text, b"x").unwrap();
        let empty = dir.join("empty.jpg");
        std::fs::write(&empty, b"").unwrap();
        for bad in [text, empty, dir.join("missing.png"), dir.clone()] {
            assert!(
                load_images(std::slice::from_ref(&bad)).is_err(),
                "{}",
                bad.display()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn approvals_accept_only_known_channels() {
        assert_eq!(ApprovalChannel::parse("stdio"), Ok(ApprovalChannel::Stdio));
        assert_eq!(ApprovalChannel::parse("deny"), Ok(ApprovalChannel::Deny));
        assert!(ApprovalChannel::parse("tty").is_err());
    }
}
