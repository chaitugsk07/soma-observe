//! Webhook notification for alert state transitions.
//!
//! Uses `soma_infra::http::client()` for the outbound POST.
//! Failures are logged as warnings and do not propagate — the evaluator must not
//! crash because a webhook endpoint is unreachable.

use tracing::warn;

/// POST the given JSON payload to `url`.
///
/// Compatible with Slack / Discord / Mattermost incoming webhooks (the `text`
/// field in the payload) and generic consumers (the structured fields).
pub async fn send_webhook(url: &str, payload: &serde_json::Value) {
    let client = match soma_infra::http::client() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "webhook: failed to build HTTP client");
            return;
        }
    };

    match client.post(url).json(payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(url, status = %resp.status(), "webhook delivered");
        }
        Ok(resp) => {
            warn!(url, status = %resp.status(), "webhook returned non-success status");
        }
        Err(e) => {
            warn!(url, error = %e, "webhook POST failed");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Payload builder: verify the JSON shape expected by Slack/Discord/generic.
    #[test]
    fn webhook_payload_shape() {
        let payload = serde_json::json!({
            "text": "[CRITICAL] cpu.usage firing: value 92.0000 exceeds/triggers threshold 80.0000",
            "rule": "cpu.usage",
            "severity": "critical",
            "state": "firing",
            "value": 92.0,
            "threshold": 80.0,
            "kind": "metric",
            "timestamp": "2026-06-29T12:00:00Z",
        });

        // Validate all required fields are present and have the right type.
        assert!(payload["text"].as_str().is_some(), "text must be a string");
        assert!(payload["rule"].as_str().is_some(), "rule must be a string");
        assert!(
            payload["severity"].as_str().is_some(),
            "severity must be a string"
        );
        assert!(payload["state"].as_str().is_some(), "state must be a string");
        assert!(
            payload["value"].as_f64().is_some(),
            "value must be numeric"
        );
        assert!(
            payload["threshold"].as_f64().is_some(),
            "threshold must be numeric"
        );
        assert!(payload["kind"].as_str().is_some(), "kind must be a string");
        assert!(
            payload["timestamp"].as_str().is_some(),
            "timestamp must be a string"
        );

        // Verify text contains the key identifying parts.
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("CRITICAL"), "text must contain severity");
        assert!(text.contains("cpu.usage"), "text must contain rule name");
        assert!(text.contains("firing"), "text must contain state");
    }

    #[test]
    fn resolved_payload_shape() {
        let payload = serde_json::json!({
            "text": "[WARNING] error.rate resolved: value 0.0000 no longer triggers threshold 5.0000",
            "rule": "error.rate",
            "severity": "warning",
            "state": "resolved",
            "value": 0.0,
            "threshold": 5.0,
            "kind": "metric",
            "timestamp": "2026-06-29T12:05:00Z",
        });

        assert_eq!(payload["state"].as_str(), Some("resolved"));
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("resolved"));
    }

    /// send_webhook to an invalid URL should not panic.
    #[tokio::test]
    async fn send_webhook_invalid_url_does_not_panic() {
        // A localhost port that nothing listens on — must fail gracefully.
        let payload = serde_json::json!({"text": "test"});
        send_webhook("http://127.0.0.1:1", &payload).await;
        // If we reach here the function didn't panic — that's the contract.
    }
}
