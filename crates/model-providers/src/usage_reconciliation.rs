//! Cross-provider usage reconciliation.
//!
//! Every adapter normalizes a provider's wire usage into [`Usage`], whose
//! buckets are disjoint: `input_tokens` excludes cache reads and writes. The
//! prompt side of a request is therefore always
//! `input_tokens + cache_read_tokens + cache_create_tokens`, and that sum must
//! equal what the provider billed for the prompt — the number shown on a
//! provider dashboard.
//!
//! The formula relating that sum to the wire payload differs by family, so
//! each fixture declares its own expectation rather than sharing one:
//!
//! * OpenAI family (also OpenRouter, CheaperInference, Vercel): `prompt_tokens`
//!   is cache-INCLUSIVE, so it equals the sum.
//! * Codex: `input_tokens` is cache-inclusive but the protocol reports no cache
//!   writes, so it equals input + read, with write pinned at zero.
//! * Anthropic: `input_tokens` is cache-EXCLUSIVE — it maps straight to
//!   `Usage::input_tokens`, and the prompt total is the sum of all three.
//!
//! Assertions run against the raw wire numbers, never against values routed
//! back through a saturating op, because the `saturating_sub` chain in
//! `openai::WireUsage::into_usage` is what hid both #19 and #20: an
//! over-subtraction clamps `input_tokens` to zero and the sum then silently
//! exceeds the provider's total.

use crate::openai::WireUsage;
use orca_harness_core::Usage;
use serde_json::{json, Value};

/// One provider wire payload and the prompt total it reports.
struct Fixture {
    /// Label used in failure output.
    provider: &'static str,
    /// The prompt-side number the provider bills and displays.
    reported_prompt: u64,
    reported_output: u64,
    usage: Usage,
}

impl Fixture {
    /// The harness's view of the prompt side: the three disjoint buckets.
    fn normalized_prompt(&self) -> u64 {
        self.usage.input_tokens + self.usage.cache_read_tokens + self.usage.cache_create_tokens
    }

    /// Panics with the same breakdown a reconciliation table would show.
    fn assert_reconciles(&self) {
        let sum = self.normalized_prompt();
        assert_eq!(
            sum,
            self.reported_prompt,
            "\n{} prompt tokens do not reconcile\n  \
             provider total {}\n  input          {}\n  cache read     {}\n  \
             cache write    {}\n  sum            {}\n  delta          {}\n",
            self.provider,
            self.reported_prompt,
            self.usage.input_tokens,
            self.usage.cache_read_tokens,
            self.usage.cache_create_tokens,
            sum,
            sum as i64 - self.reported_prompt as i64,
        );
        assert_eq!(
            self.usage.output_tokens, self.reported_output,
            "{} output tokens do not reconcile",
            self.provider
        );
        // Reasoning is documented as already inside output_tokens.
        assert!(
            self.usage.reasoning_tokens.unwrap_or(0) <= self.usage.output_tokens,
            "{} reports more reasoning than output",
            self.provider
        );
    }
}

/// OpenAI-compatible: `prompt_tokens` is cache-inclusive, so it IS the sum.
fn openai_family(provider: &'static str, wire: Value) -> Fixture {
    let reported_prompt = wire["prompt_tokens"].as_u64().unwrap_or(0);
    let reported_output = wire["completion_tokens"].as_u64().unwrap_or(0);
    let usage = serde_json::from_value::<WireUsage>(wire)
        .unwrap()
        .into_usage();
    Fixture {
        provider,
        reported_prompt,
        reported_output,
        usage,
    }
}

/// Codex Responses: `input_tokens` is cache-inclusive; no cache writes exist.
fn codex(wire: Value) -> Fixture {
    let reported_prompt = wire["input_tokens"].as_u64().unwrap_or(0);
    let reported_output = wire["output_tokens"].as_u64().unwrap_or(0);
    let usage = crate::openai_codex::stream::parse_usage(&wire).unwrap();
    Fixture {
        provider: "codex",
        reported_prompt,
        reported_output,
        usage,
    }
}

/// Anthropic: `input_tokens` is cache-EXCLUSIVE, so the provider's prompt
/// total is the sum of all three wire fields. Driven through the real
/// accumulator so the message_start/message_delta merge is covered too.
fn anthropic(start_usage: Value, delta_output: u64) -> Fixture {
    let reported_prompt = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .filter_map(|field| start_usage[field].as_u64())
    .sum();
    let frames = vec![
        json!({"type":"message_start","message":{"usage":start_usage}}),
        // The accumulator rejects a response with no content, so give it text.
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,
               "delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
               "usage":{"output_tokens":delta_output}}),
        json!({"type":"message_stop"}),
    ];
    let mut acc = crate::anthropic::stream::Accumulator::default();
    for frame in frames {
        acc.apply(&frame.to_string()).unwrap();
    }
    let orca_harness_core::ModelResponse::Final {
        usage: Some(usage), ..
    } = acc.finish().unwrap().response
    else {
        panic!("expected a final response carrying usage");
    };
    Fixture {
        provider: "anthropic",
        reported_prompt,
        reported_output: delta_output,
        usage,
    }
}

#[test]
fn every_provider_prompt_total_splits_into_disjoint_buckets() {
    let fixtures = vec![
        // Plain OpenAI: cached reads only, no cache-write reporting.
        openai_family(
            "openai",
            json!({"prompt_tokens":12,"completion_tokens":34,
                   "prompt_tokens_details":{"cached_tokens":5}}),
        ),
        // OpenRouter fronting Anthropic: reads and writes reported separately
        // and disjointly (the live payload from #19/#20).
        openai_family(
            "openrouter",
            json!({"prompt_tokens":19848,"completion_tokens":5,
                   "prompt_tokens_details":{"cached_tokens":9923,"cache_write_tokens":9913}}),
        ),
        // DeepSeek: no details object, cache hits in a top-level field.
        openai_family(
            "deepseek",
            json!({"prompt_tokens":1000,"completion_tokens":20,
                   "prompt_cache_hit_tokens":600}),
        ),
        // No cache participation at all.
        openai_family(
            "openai-uncached",
            json!({"prompt_tokens":12,"completion_tokens":34}),
        ),
        // Every counter zero. Paired with the nonzero rows above so the
        // matrix can still tell a correct parser from one returning defaults.
        openai_family("openai-empty", json!({})),
        codex(json!({"input_tokens":12,"output_tokens":3,
                     "input_tokens_details":{"cached_tokens":5}})),
        codex(json!({"input_tokens":100,"output_tokens":850,
                     "output_tokens_details":{"reasoning_tokens":600}})),
        anthropic(
            json!({"input_tokens":11,"output_tokens":1,
                   "cache_creation_input_tokens":20,"cache_read_input_tokens":30}),
            7,
        ),
        anthropic(json!({"input_tokens":9}), 4),
    ];
    for fixture in &fixtures {
        fixture.assert_reconciles();
    }
}

#[test]
fn anthropic_input_tokens_are_taken_as_cache_exclusive() {
    // Guards against applying the OpenAI subtraction here: an adapter that
    // subtracted reads from Anthropic's already-exclusive input_tokens would
    // report 0 instead of 11, and saturating arithmetic would hide it.
    let fixture = anthropic(
        json!({"input_tokens":11,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}),
        7,
    );
    assert_eq!(fixture.usage.input_tokens, 11);
    assert_eq!(fixture.usage.cache_read_tokens, 30);
    assert_eq!(fixture.usage.cache_create_tokens, 20);
    assert_eq!(fixture.reported_prompt, 61);
}

#[test]
fn codex_reports_no_cache_writes() {
    // Pinned so that adding cache-write support to the Responses adapter has
    // to update this test — and the reconciliation formula — deliberately.
    let fixture = codex(json!({"input_tokens":12,"output_tokens":3,
                               "input_tokens_details":{"cached_tokens":5}}));
    assert_eq!(fixture.usage.cache_create_tokens, 0);
    assert_eq!(fixture.usage.input_tokens, 7);
}

#[test]
fn openai_over_subtraction_clamps_and_breaks_reconciliation() {
    // Documents the one shape where the invariant CANNOT hold: a gateway whose
    // `cached_tokens` is itself cache-exclusive, so reads + writes exceed
    // prompt_tokens. saturating_sub clamps input to 0 and the buckets then sum
    // to more than the provider billed. Pinned rather than asserted-away: if a
    // provider ever sends this, the sum overshooting is the signal to special-
    // case that gateway, not something to silently absorb.
    let fixture = openai_family(
        "hypothetical-gateway",
        json!({"prompt_tokens":100,"completion_tokens":1,
               "prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":80}}),
    );
    assert_eq!(fixture.usage.input_tokens, 0, "clamped by saturating_sub");
    assert_eq!(fixture.normalized_prompt(), 160);
    assert!(
        fixture.normalized_prompt() > fixture.reported_prompt,
        "over-subtraction inflates the prompt total"
    );
}

#[test]
fn unrecognized_cache_field_names_land_in_input() {
    // The live failure this file was written for: a gateway that bills cache
    // reads and writes but spells the fields differently from anything the
    // adapters know. Nothing errors — the tokens are simply counted as plain
    // input, cache buckets read zero, and the prompt total still reconciles.
    // So a passing reconciliation is NOT evidence the split is right; it only
    // proves the buckets are disjoint. These assertions pin that blind spot so
    // it is visible, and so teaching a parser a new field name changes a test.
    let openai = openai_family(
        "gateway-with-anthropic-spelling",
        json!({"prompt_tokens":1000,"completion_tokens":10,
               "prompt_tokens_details":{"cache_creation_input_tokens":400}}),
    );
    openai.assert_reconciles();
    assert_eq!(
        openai.usage.input_tokens, 1000,
        "400 writes hidden in input"
    );
    assert_eq!(openai.usage.cache_read_tokens, 0);
    assert_eq!(openai.usage.cache_create_tokens, 0);

    let codex = codex(json!({"input_tokens":100,"output_tokens":5,
                             "input_tokens_details":{"cache_read_tokens":40}}));
    codex.assert_reconciles();
    assert_eq!(codex.usage.input_tokens, 100, "40 reads hidden in input");
    assert_eq!(codex.usage.cache_read_tokens, 0);

    // Anthropic names its fields in the payload itself, so a renamed field is
    // indistinguishable from an absent one there too.
    let anthropic = anthropic(json!({"input_tokens":50,"cache_read_tokens":30}), 2);
    anthropic.assert_reconciles();
    assert_eq!(anthropic.usage.input_tokens, 50);
    assert_eq!(anthropic.usage.cache_read_tokens, 0);
}
