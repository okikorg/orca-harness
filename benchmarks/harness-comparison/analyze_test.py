import json
import tempfile
import unittest
from pathlib import Path

import analyze


PRICING = {
    "input_usd_per_million": 1.0,
    "output_usd_per_million": 5.0,
    "cache_read_usd_per_million": 0.1,
    "cache_write_usd_per_million": 1.25,
}


def wrapped_line(elapsed_ms, event):
    return {
        "record": "line",
        "stream": "stdout",
        "elapsed_ms": elapsed_ms,
        "text": json.dumps(event),
    }


class AnalyzeTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.manifest = {
            "pricing": PRICING,
            "tasks": [
                {
                    "id": "lookup",
                    "category": "retrieval",
                    "mode": "read",
                    "expected": "answer=42",
                }
            ],
        }

    def tearDown(self):
        self.temporary.cleanup()

    def write_raw(self, name, harness, events):
        records = [
            {
                "record": "run_start",
                "repetition": 1,
                "task": "lookup",
                "category": "retrieval",
                "mode": "read",
                "harness": harness,
            },
            *events,
            {
                "record": "run_end",
                "exit_code": 0,
                "timed_out": False,
                "wall_ms": 900.0,
            },
            {"record": "workspace_validation", "passed": True, "changed_paths": [], "errors": []},
        ]
        path = self.root / name
        path.write_text("\n".join(json.dumps(record) for record in records) + "\n")
        return path

    def test_orca_ttft_usage_and_correctness(self):
        path = self.write_raw(
            "rep-01-lookup-orca.jsonl",
            "orca",
            [
                wrapped_line(20.0, {"type": "agent_start"}),
                wrapped_line(120.0, {"type": "reasoning_delta", "text": "search"}),
                wrapped_line(250.0, {"type": "tool_call", "tool_call_id": "a"}),
                wrapped_line(270.0, {"type": "tool_finished", "tool_call_id": "a"}),
                wrapped_line(700.0, {"type": "assistant_delta", "text": "answer=42"}),
                wrapped_line(710.0, {"type": "result", "message": "answer=42"}),
                wrapped_line(
                    720.0,
                    {
                        "type": "summary",
                        "modelSteps": 2,
                        "toolCalls": 1,
                        "modelRetries": 0,
                        "usage": {
                            "inputTokens": 100,
                            "outputTokens": 20,
                            "cacheReadTokens": 30,
                            "cacheCreateTokens": 40,
                        },
                    },
                ),
            ],
        )
        row = analyze.parse_raw(path, self.manifest)
        self.assertTrue(row["success"])
        self.assertEqual(row["ttft_ms"], 120.0)
        self.assertEqual(row["model_ttft_ms"], 100.0)
        self.assertEqual(row["time_to_answer_ms"], 700.0)
        self.assertEqual(row["total_prompt_tokens"], 170)
        self.assertEqual(row["tool_duration_sum_ms"], 20.0)
        self.assertAlmostEqual(row["input_cost_usd"], 0.0001)
        self.assertAlmostEqual(row["output_cost_usd"], 0.0001)

    def test_pi_ttft_usage_and_correctness(self):
        path = self.write_raw(
            "rep-01-lookup-pi.jsonl",
            "pi",
            [
                wrapped_line(25.0, {"type": "turn_start"}),
                wrapped_line(
                    100.0,
                    {
                        "type": "message_update",
                        "assistantMessageEvent": {"type": "thinking_delta", "delta": "search"},
                    },
                ),
                wrapped_line(220.0, {"type": "tool_execution_start", "toolCallId": "b"}),
                wrapped_line(260.0, {"type": "tool_execution_end", "toolCallId": "b"}),
                wrapped_line(
                    650.0,
                    {
                        "type": "message_update",
                        "assistantMessageEvent": {"type": "text_delta", "delta": "answer=42"},
                    },
                ),
                wrapped_line(
                    660.0,
                    {
                        "type": "message_end",
                        "message": {
                            "role": "assistant",
                            "provider": "openrouter",
                            "content": [{"type": "text", "text": "answer=42"}],
                            "usage": {"input": 2, "output": 12, "cacheRead": 50, "cacheWrite": 60},
                        },
                    },
                ),
            ],
        )
        row = analyze.parse_raw(path, self.manifest)
        self.assertTrue(row["success"])
        self.assertEqual(row["ttft_ms"], 100.0)
        self.assertEqual(row["model_ttft_ms"], 75.0)
        self.assertEqual(row["time_to_answer_ms"], 650.0)
        self.assertEqual(row["turns"], 1)
        self.assertEqual(row["tool_calls"], 1)
        self.assertEqual(row["reported_provider"], "openrouter")

    def test_omp_uses_pi_json_usage_and_tool_events(self):
        path = self.write_raw(
            "rep-01-lookup-omp.jsonl",
            "omp",
            [
                wrapped_line(20.0, {"type": "turn_start"}),
                wrapped_line(
                    90.0,
                    {
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "thinking_delta",
                            "delta": "search",
                        },
                    },
                ),
                wrapped_line(180.0, {"type": "tool_execution_start", "toolCallId": "omp-a"}),
                wrapped_line(220.0, {"type": "tool_execution_end", "toolCallId": "omp-a"}),
                wrapped_line(
                    500.0,
                    {
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "text_delta",
                            "delta": "answer=42",
                        },
                    },
                ),
                wrapped_line(
                    520.0,
                    {
                        "type": "message_end",
                        "message": {
                            "role": "assistant",
                            "provider": "openrouter",
                            "content": [{"type": "text", "text": "answer=42"}],
                            "usage": {
                                "input": 100,
                                "output": 20,
                                "cacheRead": 30,
                                "cacheWrite": 40,
                            },
                        },
                    },
                ),
            ],
        )
        row = analyze.parse_raw(path, self.manifest)
        self.assertTrue(row["success"])
        self.assertEqual(row["harness"], "omp")
        self.assertEqual(row["ttft_ms"], 90.0)
        self.assertEqual(row["time_to_first_tool_ms"], 180.0)
        self.assertEqual(row["tool_duration_sum_ms"], 40.0)
        self.assertEqual(row["cache_read_tokens"], 30)
        self.assertEqual(row["cache_write_tokens"], 40)
        self.assertEqual(row["turns"], 1)
        self.assertEqual(row["tool_calls"], 1)

    def test_claude_stream_usage_ttft_tools_and_correctness(self):
        path = self.write_raw(
            "rep-01-lookup-claude.jsonl",
            "claude",
            [
                wrapped_line(20.0, {"type": "system", "subtype": "init"}),
                wrapped_line(
                    120.0,
                    {
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_delta",
                            "delta": {"type": "thinking_delta", "thinking": "search"},
                        },
                    },
                ),
                wrapped_line(
                    240.0,
                    {
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_start",
                            "content_block": {"type": "tool_use", "id": "tool-a"},
                        },
                    },
                ),
                wrapped_line(
                    250.0,
                    {
                        "type": "assistant",
                        "message": {
                            "provider": "Amazon Bedrock",
                            "content": [{"type": "tool_use", "id": "tool-a"}],
                        },
                    },
                ),
                wrapped_line(
                    300.0,
                    {
                        "type": "user",
                        "message": {
                            "content": [{"type": "tool_result", "tool_use_id": "tool-a"}],
                        },
                    },
                ),
                wrapped_line(
                    650.0,
                    {
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_delta",
                            "delta": {"type": "text_delta", "text": "answer=42"},
                        },
                    },
                ),
                wrapped_line(
                    700.0,
                    {
                        "type": "result",
                        "subtype": "success",
                        "result": "answer=42",
                        "num_turns": 2,
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 20,
                            "cache_read_input_tokens": 30,
                            "cache_creation_input_tokens": 40,
                        },
                    },
                ),
            ],
        )
        row = analyze.parse_raw(path, self.manifest)
        self.assertTrue(row["success"])
        self.assertEqual(row["ttft_ms"], 120.0)
        self.assertEqual(row["model_ttft_ms"], 100.0)
        self.assertEqual(row["time_to_answer_ms"], 650.0)
        self.assertIsNone(row["time_to_first_tool_ms"])
        self.assertIsNone(row["time_to_first_tool_result_ms"])
        self.assertIsNone(row["tool_duration_sum_ms"])
        self.assertIsNone(row["tool_duration_max_ms"])
        self.assertEqual(row["total_prompt_tokens"], 170)
        self.assertEqual(row["tool_calls"], 1)
        self.assertEqual(row["turns"], 2)
        self.assertEqual(row["reported_provider"], "Amazon Bedrock")

    def test_claude_without_result_keeps_completed_turn_usage(self):
        path = self.write_raw(
            "rep-01-lookup-claude.jsonl",
            "claude",
            [
                wrapped_line(
                    200.0,
                    {
                        "type": "assistant",
                        "message": {
                            "content": [{"type": "tool_use", "id": "tool-a"}],
                            "usage": {
                                "input_tokens": 100,
                                "output_tokens": 20,
                                "cache_read_input_tokens": 30,
                                "cache_creation_input_tokens": 40,
                            },
                        },
                    },
                ),
                wrapped_line(
                    250.0,
                    {
                        "type": "stream_event",
                        "event": {
                            "type": "message_delta",
                            "usage": {
                                "input_tokens": 100,
                                "output_tokens": 20,
                                "cache_read_input_tokens": 30,
                                "cache_creation_input_tokens": 40,
                            },
                        },
                    },
                ),
                wrapped_line(
                    600.0,
                    {
                        "type": "assistant",
                        "message": {
                            "content": [{"type": "text", "text": "answer=42"}],
                            "usage": {
                                "input_tokens": 10,
                                "output_tokens": 5,
                                "cache_read_input_tokens": 50,
                                "cache_creation_input_tokens": 60,
                            },
                        },
                    },
                ),
                wrapped_line(
                    650.0,
                    {
                        "type": "stream_event",
                        "event": {
                            "type": "message_delta",
                            "usage": {
                                "input_tokens": 10,
                                "output_tokens": 5,
                                "cache_read_input_tokens": 50,
                                "cache_creation_input_tokens": 60,
                            },
                        },
                    },
                ),
            ],
        )
        row = analyze.parse_raw(path, self.manifest)
        self.assertEqual(row["answer"], "answer=42")
        self.assertEqual(row["input_tokens"], 110)
        self.assertEqual(row["output_tokens"], 25)
        self.assertEqual(row["cache_read_tokens"], 80)
        self.assertEqual(row["cache_write_tokens"], 100)
        self.assertEqual(row["turns"], 2)
        self.assertEqual(row["tool_calls"], 1)


if __name__ == "__main__":
    unittest.main()
