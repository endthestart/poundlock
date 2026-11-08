//! What one external request may cost, what its response says it did, and what
//! it is charged.

use crate::domain::{ModelProfile, Pricing};
use serde_json::{Value, json};

/// Whether a profile's requests count against the budget: they leave the
/// operator network, so they can cost money.
pub fn metered(profile: &ModelProfile) -> bool {
    profile.data_destination == "external"
}

/// A request's most it could cost at the profile's published price: its body
/// as input at about three bytes a token (high for prose, so it counts
/// generously), plus its whole output budget. `None` without a price.
pub fn estimate_usd(profile: &ModelProfile, body: &Value) -> Option<f64> {
    let pricing = profile.catalog.pricing.as_ref()?;
    let input_tokens = body.to_string().len() as f64 / 3.0;
    let output_tokens = ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|key| body.get(*key).and_then(Value::as_u64))
        .unwrap_or(profile.max_output_tokens)
        .min(profile.max_output_tokens) as f64;
    Some(price(pricing, input_tokens, output_tokens))
}

fn price(pricing: &Pricing, input_tokens: f64, output_tokens: f64) -> f64 {
    (input_tokens * pricing.input_per_mtok + output_tokens * pricing.output_per_mtok) / 1_000_000.0
}

/// What a response said it used.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// The provider's own cost in US dollars, when it reports one.
    pub cost_usd: Option<f64>,
}

impl Usage {
    fn from_value(usage: &Value) -> Option<Self> {
        let usage = usage.as_object()?;
        let count = |names: &[&str]| names.iter().find_map(|name| usage.get(*name)?.as_u64());
        let found = Self {
            prompt_tokens: count(&["prompt_tokens", "input_tokens"]),
            completion_tokens: count(&["completion_tokens", "output_tokens"]),
            cost_usd: usage
                .get("cost")
                .and_then(Value::as_f64)
                .filter(|cost| cost.is_finite() && *cost >= 0.0),
        };
        (found != Self::default()).then_some(found)
    }
}

/// The most of a non-streamed body kept for reading its usage.
pub(super) const MAX_METERED_BODY: usize = 4 * 1024 * 1024;
/// The longest server-sent-event line kept; a usage line is far shorter.
const MAX_METERED_LINE: usize = 1024 * 1024;

/// Reads a response's usage as it passes through, without changing it. A
/// stream's usage is its last `data:` event that carries one; a body's is its
/// top-level `usage`.
pub struct UsageMeter {
    streaming: bool,
    buffer: Vec<u8>,
    overflowed: bool,
    usage: Option<Usage>,
}

impl UsageMeter {
    pub fn new(streaming: bool) -> Self {
        Self {
            streaming,
            buffer: Vec::new(),
            overflowed: false,
            usage: None,
        }
    }

    pub fn observe(&mut self, chunk: &[u8]) {
        if !self.streaming {
            if self.buffer.len() + chunk.len() > MAX_METERED_BODY {
                self.overflowed = true;
                self.buffer.clear();
            } else if !self.overflowed {
                self.buffer.extend_from_slice(chunk);
            }
            return;
        }
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.buffer);
                if !self.overflowed {
                    self.read_event_line(&line);
                }
                self.overflowed = false;
            } else if self.buffer.len() < MAX_METERED_LINE {
                self.buffer.push(*byte);
            } else {
                self.overflowed = true;
                self.buffer.clear();
            }
        }
    }

    fn read_event_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        // Most events carry no usage; skip parsing them.
        if !data.windows(7).any(|window| window == b"\"usage\"") {
            return;
        }
        if let Ok(event) = serde_json::from_slice::<Value>(data)
            && let Some(usage) = event.get("usage").and_then(Usage::from_value)
        {
            self.usage = Some(usage);
        }
    }

    pub fn finish(mut self) -> Option<Usage> {
        if self.streaming {
            let line = std::mem::take(&mut self.buffer);
            if !self.overflowed {
                self.read_event_line(&line);
            }
            return self.usage;
        }
        if self.overflowed {
            return None;
        }
        serde_json::from_slice::<Value>(&self.buffer)
            .ok()
            .and_then(|body| body.get("usage").and_then(Usage::from_value))
    }
}

/// What a finished request is charged, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Charge {
    pub cost_usd: f64,
    /// `gateway`, `provider`, `pricing`, `estimate` or `unpriced`.
    pub source: &'static str,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// The charge for a finished request: the most authoritative figure that is
/// positive, else zero from an authoritative figure, else the estimate.
pub fn charge(
    gateway_cost_usd: Option<f64>,
    usage: Option<&Usage>,
    pricing: Option<&Pricing>,
    estimate_usd: Option<f64>,
) -> Charge {
    let priced = usage.zip(pricing).and_then(|(usage, pricing)| {
        let prompt = usage.prompt_tokens?;
        let completion = usage.completion_tokens.unwrap_or(0);
        Some(price(pricing, prompt as f64, completion as f64))
    });
    let candidates = [
        ("gateway", gateway_cost_usd),
        ("provider", usage.and_then(|usage| usage.cost_usd)),
        ("pricing", priced),
    ];
    let chosen = candidates
        .iter()
        .find(|(_, cost)| cost.is_some_and(|cost| cost > 0.0))
        .or_else(|| candidates.iter().find(|(_, cost)| cost.is_some()));
    let (cost_usd, source) = match chosen {
        Some((source, Some(cost))) => (*cost, *source),
        _ => match estimate_usd {
            Some(estimate) => (estimate, "estimate"),
            None => (0.0, "unpriced"),
        },
    };
    Charge {
        cost_usd,
        source,
        prompt_tokens: usage.and_then(|usage| usage.prompt_tokens),
        completion_tokens: usage.and_then(|usage| usage.completion_tokens),
    }
}

/// Ask a streamed request to end with its usage, so it can be metered. The
/// final event is OpenAI's standard usage chunk, with no choices.
pub fn request_stream_usage(body: &mut Value) {
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if !body.get("stream_options").is_some_and(Value::is_object) {
        body["stream_options"] = json!({});
    }
    body["stream_options"]["include_usage"] = json!(true);
}
