use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UsageWindow {
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_window_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_after_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
}

impl UsageWindow {
    pub fn window_minutes(&self) -> Option<u64> {
        self.limit_window_seconds
            .map(|seconds| seconds.saturating_add(59) / 60)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UsageRateLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_window: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UsageCredits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_credits: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlimited: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UsageSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<UsageRateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<UsageCredits>,
}

impl UsageSnapshot {
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let primary = window_from_headers(headers, "primary");
        let secondary = window_from_headers(headers, "secondary");
        if primary.is_none() && secondary.is_none() {
            return None;
        }
        Some(Self {
            plan_type: None,
            rate_limit: Some(UsageRateLimit {
                allowed: None,
                limit_reached: None,
                primary_window: primary,
                secondary_window: secondary,
            }),
            credits: None,
        })
    }
}

fn window_from_headers(headers: &HeaderMap, name: &str) -> Option<UsageWindow> {
    let used_percent = header_f64(headers, &format!("x-codex-{name}-used-percent"))?;
    let window_minutes = header_u64(headers, &format!("x-codex-{name}-window-minutes"));
    Some(UsageWindow {
        used_percent,
        limit_window_seconds: window_minutes.map(|minutes| minutes.saturating_mul(60)),
        reset_after_seconds: None,
        reset_at: header_i64(headers, &format!("x-codex-{name}-reset-at")),
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_str(headers, name)?.parse().ok()
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_str(headers, name)?.parse().ok()
}

fn header_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    header_str(headers, name)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn decodes_backend_usage_payload() {
        let value = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 66,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 4200,
                    "reset_at": 1770000000
                },
                "secondary_window": {
                    "used_percent": 20,
                    "limit_window_seconds": 604800,
                    "reset_at": 1770500000
                }
            },
            "credits": { "has_credits": true, "unlimited": false, "balance": "9.99" }
        });
        let snapshot: UsageSnapshot = serde_json::from_value(value).expect("usage payload");
        let primary = snapshot
            .rate_limit
            .as_ref()
            .and_then(|limit| limit.primary_window.as_ref())
            .expect("primary window");
        assert_eq!(primary.used_percent, 66.0);
        assert_eq!(primary.window_minutes(), Some(300));
        assert_eq!(snapshot.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn extracts_usage_from_response_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("66"),
        );
        headers.insert(
            "x-codex-primary-window-minutes",
            HeaderValue::from_static("300"),
        );
        headers.insert(
            "x-codex-primary-reset-at",
            HeaderValue::from_static("1770000000"),
        );
        let snapshot = UsageSnapshot::from_headers(&headers).expect("usage headers");
        let primary = snapshot
            .rate_limit
            .and_then(|limit| limit.primary_window)
            .expect("primary window");
        assert_eq!(primary.used_percent, 66.0);
        assert_eq!(primary.window_minutes(), Some(300));
        assert_eq!(primary.reset_at, Some(1770000000));
    }
}
