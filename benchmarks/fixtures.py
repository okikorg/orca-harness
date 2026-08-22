#!/usr/bin/env python3
"""Build a hermetic fixture tree for the orcacode startup benchmarks.

Startup latency is only meaningful against a known state. The developer's
real `~/.config/orcacode` has an unknown number of sessions, skills, and
MCP servers in it, so the benchmark points the binary at a tree this
script builds instead:

    <root>/
      home/                     HOME — empty, so the ~/.claude/skills
                                compatibility roots miss the way they do
                                on a fresh machine
      config/                   ORCA_CONFIG_DIR
        config.json             provider/model/theme, no keys, no MCP
        sessions/<key>/*.jsonl  recorded transcripts to list and resume
      workspace/                a workspace with no skills
      workspace-skills/         the same, with .orca/skills populated
      workspace-fresh/          for the run that creates a session, so it
                                never touches the sessions above

The session directory name is `workspace_key()` from
crates/extensions/src/session.rs, reimplemented here. It is FNV-1a over
the *canonical* workspace path, so the fixture resolves paths the same
way `workspace_scope()` does.
"""

import argparse
import json
import shutil
from pathlib import Path

FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3
MASK = (1 << 64) - 1

SESSION_FORMAT_VERSION = 1
MODEL = "qwen3.5:9b"


def workspace_key(workspace: str) -> str:
    """Port of `workspace_key` in crates/extensions/src/session.rs."""
    digest = FNV_OFFSET
    for byte in workspace.encode("utf-8"):
        digest = ((digest ^ byte) * FNV_PRIME) & MASK
    mapped = "".join(
        char.lower() if char.isascii() and char.isalnum() else "-" for char in workspace
    )
    tail = mapped.strip("-") or "ws"
    tail = tail[max(len(tail) - 24, 0) :]
    return f"{tail}-{digest:016x}"


def session_id(index: int) -> str:
    """Timestamp-prefixed the way `new_session_id` builds them, so lexical
    order is creation order and `--continue` picks the highest index."""
    return f"{1_755_000_000 + index:010d}-0bench-{index}"


def transcript(messages: int) -> list[dict]:
    """A realistic message mix: the model talks, calls a tool, reads the
    result. Every line has to deserialize into `Message`, whose default
    serde representation is externally tagged.

    Whole user/assistant/tool triplets only. A transcript ending on an
    assistant turn with unanswered tool calls is a crash artifact, and
    `SessionHandler::resume` repairs those by rewriting the file — which
    would make the benchmark mutate its own fixture between runs.
    """
    out: list[dict] = [{"System": {"content": "You are orcacode." + " " * 2048}}]
    for turn in range(messages // 3):
        out.append({"User": {"content": f"turn {turn}: what changed in the loop?"}})
        out.append(
            {
                "Assistant": {
                    "content": f"Checking the dispatcher for turn {turn}.",
                    "tool_calls": [
                        {
                            "id": f"call-{turn}",
                            "name": "read_file",
                            "arguments": {"path": "crates/harness-core/src/dispatch.rs"},
                        }
                    ],
                }
            }
        )
        out.append(
            {
                "Tool": {
                    "results": [
                        {
                            "call_id": f"call-{turn}",
                            "tool_name": "read_file",
                            "output": {"content": "pub struct Dispatcher;\n" * 32},
                            "is_error": False,
                        }
                    ]
                }
            }
        )
    return out


def write_session(path: Path, ident: str, workspace: str, messages: int) -> None:
    lines = [
        json.dumps(
            {
                "v": SESSION_FORMAT_VERSION,
                "id": ident,
                "created_at": 1_755_000_000,
                "workspace": workspace,
                "model": MODEL,
            },
            separators=(",", ":"),
        )
    ]
    lines.extend(json.dumps(message, separators=(",", ":")) for message in transcript(messages))
    # A trailing newline is what tells the loader the last line is whole;
    # without it `resume` reports a repair and rewrites the file, which
    # would make the benchmark mutate its own fixture.
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_skills(root: Path, count: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for index in range(count):
        name = f"bench-skill-{index:03d}"
        directory = root / name
        directory.mkdir(exist_ok=True)
        (directory / "SKILL.md").write_text(
            f"---\nname: {name}\n"
            f"description: Fixture skill {index}, used to measure discovery cost.\n"
            "---\n\n"
            f"# {name}\n\nInstructions for fixture skill {index}.\n"
            + "Filler so the file is a realistic size.\n" * 40,
            encoding="utf-8",
        )


def build(root: Path, sessions: int, messages: int, skills: int) -> dict[str, str]:
    if root.exists():
        shutil.rmtree(root)

    home = root / "home"
    config_dir = root / "config"
    plain = root / "workspace"
    with_skills = root / "workspace-skills"
    fresh = root / "workspace-fresh"
    for directory in (home, config_dir, plain, with_skills, fresh):
        directory.mkdir(parents=True)

    (config_dir / "config.json").write_text(
        json.dumps(
            {
                "provider": "local",
                "models": {"local": MODEL},
                "theme": "default",
            },
            separators=(",", ":"),
        )
        + "\n",
        encoding="utf-8",
    )

    # `workspace_scope` canonicalizes before hashing, and on macOS /tmp is
    # a symlink to /private/tmp — resolve or the key will not match.
    scope = str(plain.resolve())
    session_dir = config_dir / "sessions" / workspace_key(scope)
    session_dir.mkdir(parents=True)
    for index in range(sessions):
        # Only the newest is resumed; the rest exist to give `list` a
        # directory worth reading. Their headers are all that gets parsed.
        size = messages if index == sessions - 1 else 8
        write_session(session_dir / f"{session_id(index)}.jsonl", session_id(index), scope, size)

    write_skills(with_skills / ".orca" / "skills", skills)

    return {
        "home": str(home),
        "config": str(config_dir),
        "sessions": str(session_dir),
        "workspace": str(plain),
        "workspace_skills": str(with_skills),
        "workspace_fresh": str(fresh),
        "fresh_sessions": str(config_dir / "sessions" / workspace_key(str(fresh.resolve()))),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--sessions", type=int, default=32)
    parser.add_argument("--messages", type=int, default=2000)
    parser.add_argument("--skills", type=int, default=64)
    args = parser.parse_args()
    paths = build(args.root.resolve(), args.sessions, args.messages, args.skills)
    # Printed as shell assignments so startup.sh can `eval` the result.
    for key, value in paths.items():
        print(f"FIXTURE_{key.upper()}={value}")


if __name__ == "__main__":
    main()
