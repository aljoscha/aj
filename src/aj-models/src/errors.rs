//! Provider-independent error classification and overflow detection.
//!
//! Providers translate upstream failures (HTTP error bodies, finish
//! reasons, stream drops) into [`AssistantError`] values via the
//! `classify_*` helpers below. Callers then key retry behaviour off
//! [`ErrorCategory`] without ever pattern-matching message strings.
//!
//! See `docs/models-spec.md` §10 for the design — §10.3 owns the
//! per-provider tables, §10.5 owns context-overflow detection.
//!
//! `is_context_overflow` is the public entry point for callers that
//! want to know whether a turn failed because the request didn't fit
//! in the model's context window. It uses `error.category` as the
//! primary signal and falls back to a small set of regex patterns for
//! defensive handling of categories the provider couldn't classify
//! definitively (proxy-reshaped errors, upstream message churn).

use std::sync::OnceLock;

use regex::RegexSet;

use crate::types::{AssistantError, AssistantMessage, ErrorCategory, Message, StopReason};

// ---------------------------------------------------------------------------
// §10.5 Context overflow detection
// ---------------------------------------------------------------------------

/// Patterns that indicate a non-overflow failure mode and therefore
/// short-circuit the regex fallback. Without these, "rate limit" or
/// "too many requests" messages whose categories happened to land in
/// `InvalidRequest` / `Unknown` would otherwise match the generic
/// "too many tokens" pattern below.
const NON_OVERFLOW_EXCLUSION_PATTERNS: &[&str] = &[r"(?i)rate limit", r"(?i)too many requests"];

/// Patterns matched against `error.message` when the category is not
/// already `ContextOverflow`. Order matters only insofar as the
/// `RegexSet` returns any match; we don't care which pattern fired.
const OVERFLOW_PATTERNS: &[&str] = &[
    r"(?i)prompt is too long",
    r"(?i)request_too_large",
    r"(?i)exceeds the context window",
    r"(?i)context[_ ]length[_ ]exceeded",
    r"(?i)too many tokens",
    r"(?i)token limit exceeded",
    r"(?i)maximum context length is \d+ tokens",
    r"(?i)reduce the length of the messages",
];

fn exclusion_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new(NON_OVERFLOW_EXCLUSION_PATTERNS)
            .expect("static exclusion patterns must compile")
    })
}

fn overflow_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new(OVERFLOW_PATTERNS).expect("static overflow patterns must compile")
    })
}

/// Whether the message in `error.message` looks like a context-overflow
/// failure even though its category came in as something other than
/// [`ErrorCategory::ContextOverflow`]. Used as a defensive fallback;
/// see `docs/models-spec.md` §10.5.
fn message_matches_overflow(msg: &str) -> bool {
    if exclusion_set().is_match(msg) {
        return false;
    }
    overflow_set().is_match(msg)
}

/// Whether `message` represents a context-overflow failure.
///
/// Per `docs/models-spec.md` §10.5 the primary signal is
/// `error.category == ContextOverflow`; the regex fallback covers
/// proxy-reshaped errors and upstream message-string churn that left
/// the failure in `InvalidRequest` or `Unknown`. Also detects "silent
/// overflow" — a request that succeeded with `Stop` but whose input
/// token count plus cache hits exceeded the supplied context window.
///
/// `context_window` is optional. Pass it to enable silent-overflow
/// detection; pass `None` to skip that branch.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if let Some(err) = message.error.as_ref() {
        match err.category {
            ErrorCategory::ContextOverflow => return true,
            ErrorCategory::InvalidRequest | ErrorCategory::Unknown => {
                if message_matches_overflow(&err.message) {
                    return true;
                }
            }
            _ => {}
        }
    }

    // Silent overflow: provider accepted the request without erroring
    // but the prompt clearly didn't fit. Detect by comparing total
    // input (counted tokens + cache reads) against the model's window.
    if matches!(message.stop_reason, StopReason::Stop) {
        if let Some(window) = context_window {
            let used = message.usage.input.saturating_add(message.usage.cache_read);
            if used > window {
                return true;
            }
        }
    }

    false
}

/// Whether the most recent assistant turn in `messages` was a context
/// overflow. Convenience wrapper around [`is_context_overflow`] for
/// callers that hold a transcript instead of a single message.
pub fn last_turn_is_context_overflow(messages: &[Message], context_window: Option<u64>) -> bool {
    for msg in messages.iter().rev() {
        if let Message::Assistant(a) = msg {
            return is_context_overflow(a, context_window);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// §10.3 Per-provider classification
// ---------------------------------------------------------------------------

/// Classify an Anthropic error envelope into an [`AssistantError`].
///
/// `error_type` is the typed tag from `error.type` (e.g.
/// `"authentication_error"`, `"overloaded_error"`); `http_status` is
/// the originating response status. `retry_after_ms` is parsed from
/// the response's `Retry-After` header by the SDK before this is
/// called. `message` is the upstream-supplied human message.
///
/// See `docs/models-spec.md` §10.3 "Anthropic" table.
pub fn classify_anthropic_error(
    error_type: Option<&str>,
    http_status: Option<u16>,
    retry_after_ms: Option<u64>,
    message: String,
) -> AssistantError {
    let category = match error_type {
        Some("authentication_error") => ErrorCategory::Auth,
        Some("permission_error") => ErrorCategory::Auth,
        Some("not_found_error") => ErrorCategory::InvalidRequest,
        Some("invalid_request_error") => {
            // §10.3: 400 messages that match overflow patterns get
            // promoted from InvalidRequest to ContextOverflow so the
            // retry-on-overflow path doesn't have to fall through to
            // the regex net.
            if message_matches_overflow(&message) {
                ErrorCategory::ContextOverflow
            } else {
                ErrorCategory::InvalidRequest
            }
        }
        Some("request_too_large") => ErrorCategory::ContextOverflow,
        Some("billing_error") => ErrorCategory::InvalidRequest,
        Some("rate_limit_error") => ErrorCategory::RateLimit,
        Some("overloaded_error") => ErrorCategory::Overloaded,
        Some("api_error") => ErrorCategory::Transient,
        Some("timeout_error") => ErrorCategory::Transient,
        // No typed tag: fall back on HTTP status.
        _ => category_from_status(http_status, &message),
    };

    AssistantError {
        category,
        message,
        retry_after_ms,
        http_status,
    }
}

/// Classify an Anthropic stop-reason-driven failure (refusal,
/// model-context-window-exceeded, compaction). These don't come with
/// an HTTP error envelope; the upstream stream finished with one of
/// the failure-flavoured `stop_reason` values.
pub fn classify_anthropic_stop_reason(stop_reason_label: &str, message: String) -> AssistantError {
    let category = match stop_reason_label {
        "refusal" | "sensitive" => ErrorCategory::ContentFilter,
        "model_context_window_exceeded" => ErrorCategory::ContextOverflow,
        // Compaction failures and unknown failure-flavoured stop
        // reasons fall into Unknown so callers don't auto-retry.
        _ => ErrorCategory::Unknown,
    };
    AssistantError {
        category,
        message,
        retry_after_ms: None,
        http_status: None,
    }
}

/// Classify an OpenAI Chat Completions / Responses HTTP error.
///
/// `error_code` and `error_type` come from the `{error: {code, type}}`
/// envelope. See `docs/models-spec.md` §10.3 "OpenAI Chat Completions"
/// for the table; the Responses initial-HTTP-error path uses the same
/// mapping per §10.3.
pub fn classify_openai_error(
    error_code: Option<&str>,
    error_type: Option<&str>,
    http_status: Option<u16>,
    retry_after_ms: Option<u64>,
    message: String,
) -> AssistantError {
    let category = match (error_code, error_type, http_status) {
        // 401 / 403 with auth-shaped codes always means Auth, no
        // matter what `error_type` claims.
        (Some("invalid_api_key"), _, _) => ErrorCategory::Auth,
        (Some("invalid_request_error"), _, Some(401)) => ErrorCategory::Auth,
        (_, _, Some(401)) => ErrorCategory::Auth,
        (_, _, Some(403)) => ErrorCategory::Auth,
        // Explicit overflow code — fast path before the 400 fallthrough.
        (Some("context_length_exceeded"), _, _) => ErrorCategory::ContextOverflow,
        // Insufficient quota arrives as 429 but is a billing / config
        // problem, not a rate-limit one — not retryable.
        (Some("insufficient_quota"), _, _) => ErrorCategory::InvalidRequest,
        // 429 without an explicit insufficient_quota tag: treat as
        // rate-limit. Honour Retry-After.
        (_, _, Some(429)) => ErrorCategory::RateLimit,
        (Some("rate_limit_exceeded"), _, _) => ErrorCategory::RateLimit,
        // 400 invalid_request: promote to ContextOverflow if the
        // message looks like one of the well-known overflow shapes.
        (_, Some("invalid_request_error"), Some(400))
        | (Some("invalid_request_error"), _, Some(400))
        | (None, None, Some(400)) => {
            if message_matches_overflow(&message) {
                ErrorCategory::ContextOverflow
            } else {
                ErrorCategory::InvalidRequest
            }
        }
        // Any other 5xx is transient.
        (_, _, Some(s)) if (500..600).contains(&s) => ErrorCategory::Transient,
        // Last-ditch: classify off whatever we have.
        _ => category_from_status(http_status, &message),
    };

    AssistantError {
        category,
        message,
        retry_after_ms,
        http_status,
    }
}

/// Classify an OpenAI Chat Completions terminal `finish_reason` that
/// indicates an in-stream failure rather than a successful end.
///
/// Per §10.3, `content_filter` → `ContentFilter`, `network_error` →
/// `Transient`. Anything else falls through to `Unknown`.
pub fn classify_openai_finish_reason(label: &str, message: String) -> AssistantError {
    let category = match label {
        "content_filter" => ErrorCategory::ContentFilter,
        "network_error" => ErrorCategory::Transient,
        _ => ErrorCategory::Unknown,
    };
    AssistantError {
        category,
        message,
        retry_after_ms: None,
        http_status: None,
    }
}

/// Classify an OpenAI Responses mid-stream failure event
/// (`response.failed`, `response.incomplete`, `response.refusal`,
/// top-level `error` SSE event). See §10.3 "OpenAI Responses".
pub fn classify_openai_responses_failure(
    response_status: Option<&str>,
    incomplete_reason: Option<&str>,
    error_code: Option<&str>,
    message: String,
) -> AssistantError {
    let category = match (response_status, incomplete_reason, error_code) {
        (Some("cancelled"), _, _) => ErrorCategory::Aborted,
        (Some("incomplete"), Some("content_filter"), _) => ErrorCategory::ContentFilter,
        // `response.failed` carries a code that mirrors the HTTP-level
        // tags from §10.3 OpenAI; reuse the Chat Completions classifier.
        (Some("failed"), _, Some(code)) => {
            return classify_openai_error(Some(code), None, None, None, message);
        }
        (Some("failed"), _, None) => ErrorCategory::Unknown,
        // Top-level `error` SSE event with a known code.
        (None, _, Some(code)) => {
            return classify_openai_error(Some(code), None, None, None, message);
        }
        _ => ErrorCategory::Unknown,
    };
    AssistantError {
        category,
        message,
        retry_after_ms: None,
        http_status: None,
    }
}

/// Classify a transport-level failure (connection reset, DNS, TLS,
/// stream drop before completion). These never have a typed body.
pub fn transport_error(message: impl Into<String>) -> AssistantError {
    AssistantError {
        category: ErrorCategory::Transient,
        message: message.into(),
        retry_after_ms: None,
        http_status: None,
    }
}

/// Classify a client-initiated abort.
pub fn aborted_error(message: impl Into<String>) -> AssistantError {
    AssistantError {
        category: ErrorCategory::Aborted,
        message: message.into(),
        retry_after_ms: None,
        http_status: None,
    }
}

/// Last-ditch HTTP status → category mapping when no typed body is
/// available. Order: auth, rate, server-error, client-error.
fn category_from_status(http_status: Option<u16>, message: &str) -> ErrorCategory {
    match http_status {
        Some(401) | Some(403) => ErrorCategory::Auth,
        Some(429) => ErrorCategory::RateLimit,
        Some(529) => ErrorCategory::Overloaded,
        Some(s) if (500..600).contains(&s) => ErrorCategory::Transient,
        Some(413) => ErrorCategory::ContextOverflow,
        Some(400) => {
            if message_matches_overflow(message) {
                ErrorCategory::ContextOverflow
            } else {
                ErrorCategory::InvalidRequest
            }
        }
        Some(_) | None => ErrorCategory::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Header parsing
// ---------------------------------------------------------------------------

/// Parse a `Retry-After` header value (RFC 7231 §7.1.3) into a delay
/// in milliseconds. Accepts integer-seconds form and HTTP-date form.
/// Returns `None` if the value is missing or unparseable.
///
/// HTTP-date parsing relies on `chrono`'s RFC 2822 / RFC 7231 IMF-fixdate
/// support; an unparseable date yields `None` rather than spuriously
/// returning a delay.
#[allow(clippy::as_conversions)]
pub fn parse_retry_after(header_value: Option<&str>) -> Option<u64> {
    let value = header_value?.trim();
    if value.is_empty() {
        return None;
    }
    // Integer seconds form is the common case for both Anthropic and
    // OpenAI rate-limit responses.
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs.saturating_mul(1000));
    }
    if let Ok(secs) = value.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            // Clamp to u64 range. f64 → u64 is the only practical
            // path here; the value is bounded by HTTP semantics
            // (servers won't ask us to wait a year).
            let ms = (secs * 1000.0).round();
            if ms >= u64::MAX as f64 {
                return Some(u64::MAX);
            }
            return Some(ms as u64);
        }
    }
    // HTTP-date form: compute the delta from now.
    if let Ok(when) = chrono::DateTime::parse_from_rfc2822(value) {
        let now = chrono::Utc::now();
        let delta = when.signed_duration_since(now);
        let ms = delta.num_milliseconds();
        return Some(u64::try_from(ms).unwrap_or(0));
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Usage, UsageCost};

    fn assistant_with_error(err: AssistantError) -> AssistantMessage {
        let mut msg = AssistantMessage::empty();
        msg.stop_reason = StopReason::Error;
        msg.error = Some(err);
        msg
    }

    // -------- is_context_overflow --------

    #[test]
    fn overflow_via_category_primary_path() {
        let msg = assistant_with_error(AssistantError::new(
            ErrorCategory::ContextOverflow,
            "anything",
        ));
        assert!(is_context_overflow(&msg, None));
    }

    #[test]
    fn overflow_via_regex_fallback_invalid_request() {
        let msg = assistant_with_error(AssistantError::new(
            ErrorCategory::InvalidRequest,
            "prompt is too long: 250000 > 200000 tokens",
        ));
        assert!(is_context_overflow(&msg, None));
    }

    #[test]
    fn overflow_via_regex_fallback_unknown() {
        let msg = assistant_with_error(AssistantError::new(
            ErrorCategory::Unknown,
            "Please reduce the length of the messages.",
        ));
        assert!(is_context_overflow(&msg, None));
    }

    #[test]
    fn rate_limit_message_does_not_trigger_overflow() {
        // Rate-limit text contains "too many" which would otherwise
        // greedy-match the "too many tokens" pattern. Exclusion saves it.
        let msg = assistant_with_error(AssistantError::new(
            ErrorCategory::Unknown,
            "rate limit exceeded — too many requests",
        ));
        assert!(!is_context_overflow(&msg, None));
    }

    #[test]
    fn overflow_only_runs_regex_for_invalid_or_unknown() {
        // Auth-classified errors aren't run through the regex fallback,
        // even if their message would happen to match.
        let msg = assistant_with_error(AssistantError::new(
            ErrorCategory::Auth,
            "context_length_exceeded ha ha",
        ));
        assert!(!is_context_overflow(&msg, None));
    }

    #[test]
    fn silent_overflow_detected_via_usage_and_window() {
        let mut msg = AssistantMessage::empty();
        msg.stop_reason = StopReason::Stop;
        msg.usage = Usage {
            input: 200_000,
            output: 0,
            cache_read: 5_000,
            cache_write: 0,
            total_tokens: 205_000,
            cost: UsageCost::default(),
        };
        // 205_000 > 200_000.
        assert!(is_context_overflow(&msg, Some(200_000)));
        // No window: silent overflow not detected.
        assert!(!is_context_overflow(&msg, None));
    }

    #[test]
    fn silent_overflow_only_on_stop() {
        let mut msg = AssistantMessage::empty();
        msg.stop_reason = StopReason::ToolUse;
        msg.usage = Usage {
            input: 200_000,
            output: 0,
            cache_read: 5_000,
            cache_write: 0,
            total_tokens: 205_000,
            cost: UsageCost::default(),
        };
        assert!(!is_context_overflow(&msg, Some(200_000)));
    }

    // -------- Anthropic classification --------

    #[test]
    fn anthropic_typed_tags_map_correctly() {
        let cases = [
            ("authentication_error", 401, ErrorCategory::Auth),
            ("permission_error", 403, ErrorCategory::Auth),
            ("not_found_error", 404, ErrorCategory::InvalidRequest),
            ("billing_error", 402, ErrorCategory::InvalidRequest),
            ("rate_limit_error", 429, ErrorCategory::RateLimit),
            ("overloaded_error", 529, ErrorCategory::Overloaded),
            ("api_error", 500, ErrorCategory::Transient),
            ("timeout_error", 504, ErrorCategory::Transient),
            ("request_too_large", 413, ErrorCategory::ContextOverflow),
        ];
        for (tag, status, expect) in cases {
            let err = classify_anthropic_error(Some(tag), Some(status), None, "msg".into());
            assert_eq!(err.category, expect, "tag={tag}");
            assert_eq!(err.http_status, Some(status));
        }
    }

    #[test]
    fn anthropic_invalid_request_promotes_overflow_message() {
        let err = classify_anthropic_error(
            Some("invalid_request_error"),
            Some(400),
            None,
            "prompt is too long".into(),
        );
        assert_eq!(err.category, ErrorCategory::ContextOverflow);
    }

    #[test]
    fn anthropic_invalid_request_otherwise_invalid() {
        let err = classify_anthropic_error(
            Some("invalid_request_error"),
            Some(400),
            None,
            "tool name not recognized".into(),
        );
        assert_eq!(err.category, ErrorCategory::InvalidRequest);
    }

    #[test]
    fn anthropic_stop_reason_classification() {
        let refusal = classify_anthropic_stop_reason("refusal", "blocked".into()).category;
        assert_eq!(refusal, ErrorCategory::ContentFilter);
        let overflow =
            classify_anthropic_stop_reason("model_context_window_exceeded", "too long".into())
                .category;
        assert_eq!(overflow, ErrorCategory::ContextOverflow);
        let other = classify_anthropic_stop_reason("compaction", "x".into()).category;
        assert_eq!(other, ErrorCategory::Unknown);
    }

    // -------- OpenAI classification --------

    #[test]
    fn openai_codes_map_correctly() {
        let cases: &[(Option<&str>, Option<&str>, Option<u16>, ErrorCategory)] = &[
            (
                Some("invalid_api_key"),
                None,
                Some(401),
                ErrorCategory::Auth,
            ),
            (None, None, Some(401), ErrorCategory::Auth),
            (None, None, Some(403), ErrorCategory::Auth),
            (
                Some("context_length_exceeded"),
                None,
                Some(400),
                ErrorCategory::ContextOverflow,
            ),
            (
                Some("insufficient_quota"),
                None,
                Some(429),
                ErrorCategory::InvalidRequest,
            ),
            (
                Some("rate_limit_exceeded"),
                None,
                Some(429),
                ErrorCategory::RateLimit,
            ),
            (None, None, Some(503), ErrorCategory::Transient),
        ];
        for (code, ty, status, expect) in cases {
            let err = classify_openai_error(*code, *ty, *status, None, "test".into());
            assert_eq!(err.category, *expect, "code={code:?}");
        }
    }

    #[test]
    fn openai_invalid_request_overflow_promotion() {
        let err = classify_openai_error(
            None,
            Some("invalid_request_error"),
            Some(400),
            None,
            "This model's maximum context length is 128000 tokens".into(),
        );
        assert_eq!(err.category, ErrorCategory::ContextOverflow);
    }

    #[test]
    fn openai_finish_reason_classification() {
        assert_eq!(
            classify_openai_finish_reason("content_filter", "x".into()).category,
            ErrorCategory::ContentFilter
        );
        assert_eq!(
            classify_openai_finish_reason("network_error", "x".into()).category,
            ErrorCategory::Transient
        );
        assert_eq!(
            classify_openai_finish_reason("weird", "x".into()).category,
            ErrorCategory::Unknown
        );
    }

    #[test]
    fn openai_responses_failure_classification() {
        assert_eq!(
            classify_openai_responses_failure(Some("cancelled"), None, None, "x".into()).category,
            ErrorCategory::Aborted
        );
        assert_eq!(
            classify_openai_responses_failure(
                Some("incomplete"),
                Some("content_filter"),
                None,
                "x".into()
            )
            .category,
            ErrorCategory::ContentFilter
        );
        assert_eq!(
            classify_openai_responses_failure(
                Some("failed"),
                None,
                Some("rate_limit_exceeded"),
                "x".into()
            )
            .category,
            ErrorCategory::RateLimit
        );
        assert_eq!(
            classify_openai_responses_failure(Some("failed"), None, None, "x".into()).category,
            ErrorCategory::Unknown
        );
    }

    // -------- Header parsing --------

    #[test]
    fn parse_retry_after_integer_seconds() {
        assert_eq!(parse_retry_after(Some("5")), Some(5_000));
        assert_eq!(parse_retry_after(Some("0")), Some(0));
    }

    #[test]
    fn parse_retry_after_fractional_seconds() {
        assert_eq!(parse_retry_after(Some("1.5")), Some(1_500));
    }

    #[test]
    fn parse_retry_after_http_date_future() {
        let future = (chrono::Utc::now() + chrono::Duration::seconds(60))
            .format("%a, %d %b %Y %H:%M:%S GMT");
        let s = future.to_string();
        let ms = parse_retry_after(Some(&s)).expect("parse future date");
        // Allow some slack for parse latency.
        assert!(ms > 30_000 && ms <= 60_500, "ms={ms}");
    }

    #[test]
    fn parse_retry_after_missing_or_empty() {
        assert_eq!(parse_retry_after(None), None);
        assert_eq!(parse_retry_after(Some("")), None);
        assert_eq!(parse_retry_after(Some("   ")), None);
        assert_eq!(parse_retry_after(Some("not-a-date")), None);
    }

    // -------- transport / aborted helpers --------

    #[test]
    fn transport_helper_marks_transient() {
        let err = transport_error("connection reset");
        assert_eq!(err.category, ErrorCategory::Transient);
        assert!(err.http_status.is_none());
    }

    #[test]
    fn aborted_helper_marks_aborted() {
        let err = aborted_error("user cancelled");
        assert_eq!(err.category, ErrorCategory::Aborted);
    }

    // -------- ErrorCategory::is_retryable --------

    #[test]
    fn is_retryable_truth_table() {
        assert!(ErrorCategory::RateLimit.is_retryable());
        assert!(ErrorCategory::Overloaded.is_retryable());
        assert!(ErrorCategory::Transient.is_retryable());
        assert!(!ErrorCategory::Auth.is_retryable());
        assert!(!ErrorCategory::ContextOverflow.is_retryable());
        assert!(!ErrorCategory::InvalidRequest.is_retryable());
        assert!(!ErrorCategory::ContentFilter.is_retryable());
        assert!(!ErrorCategory::Aborted.is_retryable());
        assert!(!ErrorCategory::Unknown.is_retryable());
    }
}
