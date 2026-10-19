//! Redacting a third-party account identifier from provider error text.
//!
//! LiteLLM embeds OpenRouter's raw error JSON as an escaped string inside its
//! own `error.message`, and OpenRouter's error carries the operator's own
//! OpenRouter account id as `user_id`. An application-token holder must never
//! see that id, whether it lands here as a bare JSON field or, once escaped
//! into another document's string content, as `\"user_id\"`.

/// Replace every `user_id` value in `text` with a redacted placeholder,
/// matching the key whether its quotes are bare (a real JSON field) or
/// backslash-escaped (JSON string content nested inside another document).
/// Everything else — status, structure, surrounding text — is left as is.
pub(crate) fn redact_user_id(text: &str) -> String {
    const BARE_KEY: &str = "\"user_id\"";
    const ESCAPED_KEY: &str = "\\\"user_id\\\"";
    const BARE_VALUE: &str = "\"<redacted>\"";
    const ESCAPED_VALUE: &str = "\\\"<redacted>\\\"";

    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = [
            rest.find(BARE_KEY).map(|at| (at, BARE_KEY, BARE_VALUE)),
            rest.find(ESCAPED_KEY)
                .map(|at| (at, ESCAPED_KEY, ESCAPED_VALUE)),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(at, _, _)| *at);
        let Some((at, key, value)) = next else {
            break;
        };
        clean.push_str(&rest[..at]);
        let after = &rest[at..];
        // The value ends at the next `,` or `}` after the key's colon. IDs
        // never contain either, so this stays correct without parsing JSON.
        let end = after
            .char_indices()
            .skip_while(|(_, c)| *c != ':')
            .skip(1)
            .find(|(_, c)| matches!(c, ',' | '}'))
            .map_or(after.len(), |(index, _)| index);
        clean.push_str(key);
        clean.push(':');
        clean.push_str(value);
        rest = &after[end..];
    }
    clean.push_str(rest);
    clean
}

/// What a client sees of an upstream error answer (L3): the upstream's own
/// message, type and code in an OpenAI-style envelope, without the gateway
/// internals LiteLLM appends (model groups, fallbacks, retries, `api_base`),
/// URLs or the OpenRouter account id. The wording of the message is otherwise
/// kept, because clients such as Pi recognise context overflow by it. A body
/// that is not a JSON error becomes a plain status message.
pub(crate) fn client_error_body(status: u16, raw: &[u8]) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(raw).ok();
    let error = parsed.as_ref().map(|value| match &value["error"] {
        serde_json::Value::Object(_) => value["error"].clone(),
        _ => value.clone(),
    });
    let message = error
        .as_ref()
        .and_then(|error| {
            ["message", "error", "detail"]
                .iter()
                .find_map(|key| error[*key].as_str())
        })
        .map(clean_message)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("the model server returned HTTP {status}"));
    let kind = error
        .as_ref()
        .and_then(|error| error["type"].as_str())
        .unwrap_or("upstream_error")
        .chars()
        .take(64)
        .collect::<String>();
    let code = error
        .as_ref()
        .map(|error| match &error["code"] {
            serde_json::Value::String(code) => {
                serde_json::Value::String(code.chars().take(64).collect())
            }
            serde_json::Value::Number(code) => serde_json::Value::Number(code.clone()),
            _ => serde_json::Value::Null,
        })
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({"error":{"message":message,"type":kind,"code":code}}).to_string()
}

fn clean_message(text: &str) -> String {
    let mut text = redact_user_id(text);
    for marker in [
        "Received Model Group",
        "Available Model Group Fallbacks",
        "LiteLLM Retried",
        "Traceback (most recent call last)",
    ] {
        if let Some(at) = text.find(marker) {
            text.truncate(at);
        }
    }
    let mut clean = String::with_capacity(text.len());
    for piece in text.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let separator = &piece[word.len()..];
        if word.contains("://") {
            clean.push_str("<url>");
        } else if is_address(word) {
            clean.push_str("<address>");
        } else if is_credential(word) {
            clean.push_str("<redacted>");
        } else if let Some(at) = word.find("api_base=") {
            clean.push_str(&word[..at]);
            clean.push_str("api_base=<redacted>");
        } else {
            clean.push_str(word);
        }
        clean.push_str(separator);
    }
    let text = clean.trim().trim_end_matches(',').trim();
    text.chars().take(2000).collect()
}

/// A credential an upstream echoed back: an API key in the `sk-…` form that
/// OpenAI, Anthropic and OpenRouter keys take, or a `key=`, `token=`,
/// `secret=` or `password=` value. The operator's upstream key must not reach
/// an application token holder through an error message.
fn is_credential(word: &str) -> bool {
    let word = word.trim_matches(|c: char| "()[]{}<>,;'\"".contains(c) || c == '.');
    let lower = word.to_ascii_lowercase();
    if ["key=", "token=", "secret=", "password="]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return true;
    }
    let Some(rest) = word
        .strip_prefix("sk-")
        .or_else(|| word.strip_prefix("sk_"))
    else {
        return false;
    };
    rest.len() >= 8
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An IP literal or a `host:port`, as in "Cannot connect to host 10.20.0.66:8080".
/// A `host:port` needs a dotted, bracketed or `localhost` host, so times and
/// words such as `ssl:default` stay readable.
fn is_address(word: &str) -> bool {
    let word = word.trim_matches(|c: char| "()[]{}<>,;'\"".contains(c) || c == '.');
    if word.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = word.rsplit_once(':') else {
        return false;
    };
    let port_ok = !port.is_empty() && port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host_ok = host.parse::<std::net::IpAddr>().is_ok()
        || host == "localhost"
        || (host.contains('.')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    port_ok && host_ok
}

/// An upstream error answer, read only up to `limit` bytes.
pub(crate) async fn read_capped(mut response: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < limit {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk[..chunk.len().min(limit - body.len())])
            }
            _ => break,
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::{client_error_body, redact_user_id};

    #[test]
    fn bare_field_is_redacted() {
        let clean = redact_user_id("{\"error\":{\"user_id\":\"user_2abc\",\"code\":400}}");
        assert!(!clean.contains("user_2abc"), "{clean}");
        assert_eq!(
            clean,
            "{\"error\":{\"user_id\":\"<redacted>\",\"code\":400}}"
        );
    }

    #[test]
    fn escaped_field_inside_a_nested_document_is_redacted() {
        let clean = redact_user_id(
            "{\"error\":{\"message\":\"outer - {\\\"user_id\\\":\\\"user_2abc\\\"}. tail\"}}",
        );
        assert!(!clean.contains("user_2abc"), "{clean}");
        assert!(
            clean.contains("\\\"user_id\\\":\\\"<redacted>\\\"}. tail"),
            "{clean}"
        );
    }

    #[test]
    fn text_without_a_user_id_is_untouched() {
        let text = "no account id here";
        assert_eq!(redact_user_id(text), text);
    }

    fn envelope(status: u16, raw: &str) -> serde_json::Value {
        serde_json::from_str(&client_error_body(status, raw.as_bytes())).unwrap()
    }

    #[test]
    fn gateway_internals_are_cut_from_an_upstream_error() {
        let raw = r#"{"error":{"message":"litellm.BadRequestError: OpenAIException - model not found at api_base=http://10.20.0.66:8080/v1 see https://docs.litellm.ai/x\nReceived Model Group=orion-27b\nAvailable Model Group Fallbacks=None","type":"invalid_request_error","code":"400"}}"#;
        let error = &envelope(400, raw)["error"];
        let message = error["message"].as_str().unwrap();
        assert!(
            message.starts_with("litellm.BadRequestError: OpenAIException - model not found"),
            "{message}"
        );
        for leaked in ["10.20.0.66", "docs.litellm.ai", "Model Group", "orion-27b"] {
            assert!(!message.contains(leaked), "{leaked} in {message}");
        }
        assert_eq!(
            (error["type"].as_str(), error["code"].as_str()),
            (Some("invalid_request_error"), Some("400"))
        );
    }

    #[test]
    fn a_context_overflow_keeps_its_wording() {
        let raw = r#"{"error":{"message":"This model's maximum context length is 131072 tokens. However, you requested 140000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        assert_eq!(
            envelope(400, raw)["error"]["message"],
            "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens."
        );
    }

    #[test]
    fn a_body_that_is_not_a_json_error_becomes_a_status_message() {
        let error = &envelope(
            502,
            "<html><body>Bad gateway at nginx/1.25 10.20.0.66</body></html>",
        )["error"];
        assert_eq!(error["message"], "the model server returned HTTP 502");
        assert_eq!(error["type"], "upstream_error");
        assert!(error["code"].is_null());
    }

    #[test]
    fn the_account_id_stays_redacted_inside_the_envelope() {
        let raw = r#"{"error":{"message":"OpenRouter said {\"user_id\":\"user_2abc\",\"code\":402}","code":402}}"#;
        let error = &envelope(402, raw)["error"];
        assert!(!error["message"].as_str().unwrap().contains("user_2abc"));
        assert_eq!(error["code"], 402);
    }

    #[test]
    fn echoed_credentials_are_redacted() {
        let raw = r#"{"error":{"message":"Incorrect API key provided: sk-proj-exampleexample. Retry with token=abc123 or (sk_live_0123456789)."}}"#;
        let message = envelope(401, raw)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            message,
            "Incorrect API key provided: <redacted> Retry with <redacted> or <redacted>"
        );
        // Words that merely start alike, and short codes, stay readable.
        let raw = r#"{"error":{"message":"sk-short and skeleton keys: the task-key setting"}}"#;
        let message = envelope(400, raw)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(message, "sk-short and skeleton keys: the task-key setting");
    }

    #[test]
    fn bare_addresses_are_redacted_but_ordinary_words_stay() {
        let raw = r#"{"error":{"message":"Cannot connect to host 10.20.0.66:8080 ssl:default [gateway.internal:4000] at 12:30 via fd00::1"}}"#;
        let message = envelope(502, raw)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            message,
            "Cannot connect to host <address> ssl:default <address> at 12:30 via <address>"
        );
    }
}
