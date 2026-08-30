use std::fs;
use std::path::{Path, PathBuf};

use orca_harness_tool_extensions::agent_plugins::validate_agent_plugin_name;

use super::Language;

struct File<'a> {
    path: String,
    body: &'a str,
}

pub(super) fn create(name: &str, language: Language) -> Result<PathBuf, String> {
    validate_agent_plugin_name(name).map_err(|error| error.to_string())?;
    let target = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .join(name);
    reject_non_empty(&target)?;
    fs::create_dir_all(&target).map_err(|error| format!("cannot create scaffold: {error}"))?;
    let package = python_package(name);
    let files = match language {
        Language::Python => python_files(),
        Language::TypeScript => typescript_files(),
    };
    for file in files {
        write_template(&target, &file.path, file.body, name, &package)?;
    }
    Ok(target)
}

fn reject_non_empty(target: &Path) -> Result<(), String> {
    if !target.exists() {
        return Ok(());
    }
    if !target.is_dir() {
        return Err(format!(
            "scaffold target is not a directory: {}",
            target.display()
        ));
    }
    if target
        .read_dir()
        .map_err(|error| format!("cannot inspect scaffold target: {error}"))?
        .next()
        .is_some()
    {
        return Err(format!(
            "scaffold target is not empty: {}",
            target.display()
        ));
    }
    Ok(())
}

fn write_template(
    target: &Path,
    relative: &str,
    template: &str,
    name: &str,
    package: &str,
) -> Result<(), String> {
    let path = target.join(relative.replace("{{PACKAGE}}", package));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create scaffold path: {error}"))?;
    }
    let body = template
        .replace("{{NAME}}", name)
        .replace("{{PACKAGE}}", package);
    fs::write(&path, body).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn python_package(name: &str) -> String {
    let mut package = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !package.is_empty() {
                package.push('_');
            }
            package.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    if package.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        package.insert(0, '_');
    }
    package
}

fn python_files() -> Vec<File<'static>> {
    vec![
        File {
            path: "plugin.json".into(),
            body: include_str!("templates/plugin.json"),
        },
        File {
            path: "mcp.json".into(),
            body: include_str!("templates/python-mcp.json"),
        },
        File {
            path: "pyproject.toml".into(),
            body: include_str!("templates/pyproject.toml"),
        },
        File {
            path: "README.md".into(),
            body: include_str!("templates/python-readme.md"),
        },
        File {
            path: ".gitignore".into(),
            body: include_str!("templates/python.gitignore"),
        },
        File {
            path: "src/{{PACKAGE}}/__init__.py".into(),
            body: "",
        },
        File {
            path: "src/{{PACKAGE}}/server.py".into(),
            body: include_str!("templates/python-server.py"),
        },
        File {
            path: "tests/test_server.py".into(),
            body: include_str!("templates/python-test.py"),
        },
    ]
}

fn typescript_files() -> Vec<File<'static>> {
    vec![
        File {
            path: "plugin.json".into(),
            body: include_str!("templates/plugin.json"),
        },
        File {
            path: "mcp.json".into(),
            body: include_str!("templates/typescript-mcp.json"),
        },
        File {
            path: "package.json".into(),
            body: include_str!("templates/package.json"),
        },
        File {
            path: "package-lock.json".into(),
            body: include_str!("templates/package-lock.json"),
        },
        File {
            path: "tsconfig.json".into(),
            body: include_str!("templates/tsconfig.json"),
        },
        File {
            path: "README.md".into(),
            body: include_str!("templates/typescript-readme.md"),
        },
        File {
            path: ".gitignore".into(),
            body: include_str!("templates/typescript.gitignore"),
        },
        File {
            path: "src/index.ts".into(),
            body: include_str!("templates/typescript-server.ts"),
        },
        File {
            path: "test/server.test.ts".into(),
            body: include_str!("templates/typescript-test.ts"),
        },
    ]
}
