import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import run


class RunTest(unittest.TestCase):
    def test_harness_selection_and_four_way_order_rotate(self):
        self.assertEqual(run.harnesses("all"), ["orca", "pi", "omp", "claude"])
        self.assertEqual(run.harnesses("both"), ["orca", "pi"])
        selected = run.harnesses("all")
        self.assertEqual(
            run.balanced_order(selected, 0, 0), ["orca", "pi", "omp", "claude"]
        )
        self.assertEqual(
            run.balanced_order(selected, 0, 1), ["pi", "omp", "claude", "orca"]
        )
        self.assertEqual(
            run.balanced_order(selected, 0, 2), ["omp", "claude", "orca", "pi"]
        )
        self.assertEqual(
            run.balanced_order(selected, 0, 3), ["claude", "orca", "pi", "omp"]
        )

    def test_prompt_cache_defaults_on_and_can_be_disabled(self):
        with patch.object(sys, "argv", ["run.py"]):
            self.assertTrue(run.parse_args().prompt_cache)
        with patch.object(sys, "argv", ["run.py", "--no-prompt-cache"]):
            self.assertFalse(run.parse_args().prompt_cache)

    def test_manifest_loads_all_profiles(self):
        all_tasks = run.load_tasks(run.HERE / "tasks.json", "all", set())
        read_tasks = run.load_tasks(run.HERE / "tasks.json", "read", set())
        edit_tasks = run.load_tasks(run.HERE / "tasks.json", "edit", set())
        self.assertEqual(len(all_tasks), 16)
        self.assertEqual(len(read_tasks), 14)
        self.assertEqual(len(edit_tasks), 2)
        self.assertEqual({task["mode"] for task in read_tasks}, {"read"})
        self.assertEqual({task["mode"] for task in edit_tasks}, {"edit"})

    def test_commands_keep_shell_out_of_edit_profile(self):
        task = run.load_tasks(run.HERE / "tasks.json", "all", {"fix-retry-boundary"})[0]
        args = SimpleNamespace(
            model=run.DEFAULT_MODEL,
            effort="low",
            max_output_tokens=1536,
            max_steps=24,
            prompt_cache=False,
        )
        orca = run.build_command("orca", "/tmp/orcacode", task, Path("/tmp/work"), args)
        pi = run.build_command("pi", "/tmp/pi", task, Path("/tmp/work"), args)
        omp = run.build_command(
            "omp",
            "/tmp/omp",
            task,
            Path("/tmp/work"),
            args,
            omp_config=Path("/tmp/omp-config.yml"),
        )
        claude = run.build_command("claude", "/tmp/claude", task, Path("/tmp/work"), args)
        self.assertIn("edit_file", orca[orca.index("--tools") + 1])
        self.assertNotIn("shell", orca[orca.index("--tools") + 1])
        self.assertIn("--no-prompt-cache", orca)
        self.assertIn("edit", pi[pi.index("--tools") + 1])
        self.assertNotIn("bash", pi[pi.index("--tools") + 1])
        self.assertIn("--no-context-files", pi)
        self.assertIn("edit", omp[omp.index("--tools") + 1])
        self.assertNotIn("bash", omp[omp.index("--tools") + 1])
        self.assertIn("--no-extensions", omp)
        self.assertIn("--no-skills", omp)
        self.assertIn("--no-rules", omp)
        self.assertIn("--no-lsp", omp)
        self.assertIn("--no-pty", omp)
        self.assertIn("--auto-approve", omp)
        self.assertEqual(omp[omp.index("--config") + 1], "/tmp/omp-config.yml")
        self.assertIn("--bare", claude)
        self.assertIn("--restricted", claude)
        self.assertIn("--strict-mcp-config", claude)
        self.assertIn("--tools=Read,Grep,Glob,Edit,Write", claude)
        self.assertNotIn("Bash", claude)

        args.prompt_cache = True
        cached_orca = run.build_command("orca", "/tmp/orcacode", task, Path("/tmp/work"), args)
        self.assertIn("--prompt-cache", cached_orca)

    def test_pi_family_environment_isolates_state_and_controls_cache(self):
        args = SimpleNamespace(max_output_tokens=1536, prompt_cache=True)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            pi = run.prepare_environment("pi", root, args)
            omp = run.prepare_environment("omp", root, args)
            self.assertEqual(pi["PI_CODING_AGENT_DIR"], str(root / "pi-state"))
            self.assertEqual(omp["PI_CODING_AGENT_DIR"], str(root / "omp-state"))
            self.assertEqual(omp["HOME"], str(root / "omp-home"))
            self.assertEqual(pi["PI_CACHE_RETENTION"], "short")
            self.assertEqual(omp["PI_CACHE_RETENTION"], "short")

            args.prompt_cache = False
            uncached = run.prepare_environment("omp", root, args)
            self.assertEqual(uncached["PI_CACHE_RETENTION"], "none")

            shared_home = root / "shared-omp-home"
            shared = run.prepare_environment(
                "omp", root, args, omp_runtime_home=shared_home
            )
            self.assertEqual(shared["HOME"], str(shared_home))

    def test_claude_environment_uses_openrouter_and_isolated_state(self):
        args = SimpleNamespace(max_output_tokens=1536, prompt_cache=True)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.dict(
                "os.environ",
                {
                    "OPENROUTER_API_KEY": "benchmark-key",
                    "ANTHROPIC_API_KEY": "must-be-removed",
                    "CLAUDE_CODE_OAUTH_TOKEN": "must-be-removed",
                    "DISABLE_PROMPT_CACHING": "1",
                },
                clear=True,
            ):
                environment = run.prepare_environment("claude", root, args)
            self.assertEqual(environment["ANTHROPIC_BASE_URL"], "https://openrouter.ai/api")
            self.assertEqual(environment["ANTHROPIC_AUTH_TOKEN"], "benchmark-key")
            self.assertEqual(environment["ANTHROPIC_API_KEY"], "")
            self.assertNotIn("CLAUDE_CODE_OAUTH_TOKEN", environment)
            self.assertNotIn("DISABLE_PROMPT_CACHING", environment)
            self.assertEqual(environment["HOME"], str(root / "home"))
            self.assertEqual(environment["CLAUDE_CONFIG_DIR"], str(root / "claude-state"))
            self.assertEqual(environment["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "1536")

    def test_omp_runtime_preparation_is_isolated_and_offline(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "runtime-home"
            with patch("run.subprocess.run") as execute:
                execute.return_value = SimpleNamespace(returncode=0, stdout="", stderr="")
                run.prepare_omp_runtime("/tmp/omp", home)
            command = execute.call_args.args[0]
            environment = execute.call_args.kwargs["env"]
            self.assertEqual(command, ["/tmp/omp", "config", "--help"])
            self.assertEqual(environment["HOME"], str(home))
            self.assertEqual(environment["PI_CODING_AGENT_DIR"], str(home / "agent-state"))

    def test_omp_config_disables_context_discovery_not_openrouter(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = run.write_omp_config(Path(temporary))
            content = path.read_text(encoding="utf-8")
        for provider in run.OMP_DISCOVERY_PROVIDERS:
            self.assertIn(f"  - {provider}\n", content)
        self.assertNotIn("  - openrouter\n", content)

    def test_workspace_validator_rejects_extra_changes(self):
        task = {
            "allowed_changes": ["src/retry.rs"],
            "checks": [
                {
                    "path": "src/retry.rs",
                    "contains": ["attempts_made < max_attempts"],
                    "not_contains": ["attempts_made <= max_attempts"],
                }
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            retry = root / "src" / "retry.rs"
            retry.write_text("attempts_made <= max_attempts\n")
            before = run.snapshot(root)
            retry.write_text("attempts_made < max_attempts\n")
            (root / "unexpected.txt").write_text(json.dumps({"changed": True}))
            validation = run.validate_workspace(task, root, before)
        self.assertFalse(validation["passed"])
        self.assertEqual(validation["changed_paths"], ["src/retry.rs", "unexpected.txt"])


if __name__ == "__main__":
    unittest.main()
