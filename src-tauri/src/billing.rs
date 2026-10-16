//! API 종량제 사용액 조회 (Admin API 키).
//!
//! - Anthropic: `GET /v1/organizations/cost_report` — 일 단위 버킷, 금액은 센트 단위 소수 문자열
//! - OpenAI: `GET /v1/organization/costs` — 일 단위 버킷, 금액은 달러(`amount.value`)
//!
//! 두 곳 모두 남은 선불 크레딧 잔액은 API로 제공하지 않는다. 그래서 이번 달(UTC) 사용액을 보여주고,
//! 사용자가 월 예산을 정하면 예산 대비 비율을 함께 보여준다.

use crate::claude::{get_json, FetchError, Limit};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

const ANTHROPIC_COST_URL: &str = "https://api.anthropic.com/v1/organizations/cost_report";
const OPENAI_COST_URL: &str = "https://api.openai.com/v1/organization/costs";
/// 한 달 버킷을 모두 받기 위한 페이지 상한 (무한 루프 방지)
const MAX_PAGES: usize = 5;

pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0).unwrap()
}

pub fn next_month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = if now.month() == 12 { (now.year() + 1, 1) } else { (now.year(), now.month() + 1) };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap()
}

fn auth_error(provider: &str) -> FetchError {
    FetchError::Auth(format!("{provider} Admin 키가 올바르지 않거나 권한이 없습니다."))
}

/// 버킷 목록의 금액 합계. `amount_of`는 결과 항목 하나의 금액(USD)을 꺼낸다.
fn sum_buckets(v: &Value, amount_of: impl Fn(&Value) -> f64) -> f64 {
    v["data"]
        .as_array()
        .map(|buckets| {
            buckets
                .iter()
                .flat_map(|b| b["results"].as_array().cloned().unwrap_or_default())
                .map(|r| amount_of(&r))
                .sum()
        })
        .unwrap_or(0.0)
}

fn anthropic_amount(r: &Value) -> f64 {
    let cents = r["amount"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| r["amount"].as_f64())
        .unwrap_or(0.0);
    cents / 100.0
}

fn openai_amount(r: &Value) -> f64 {
    r["amount"]["value"].as_f64().unwrap_or(0.0)
}

pub async fn anthropic_month_to_date(client: &Client, key: &str) -> Result<f64, FetchError> {
    let start = month_start(Utc::now()).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut total = 0.0;
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut req = client
            .get(ANTHROPIC_COST_URL)
            .query(&[("starting_at", start.as_str()), ("limit", "31")])
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
        if let Some(p) = &page {
            req = req.query(&[("page", p.as_str())]);
        }
        let v = get_json(req).await.map_err(|e| match e {
            FetchError::Auth(_) => auth_error("Anthropic"),
            e => e,
        })?;
        total += sum_buckets(&v, anthropic_amount);
        page = v["next_page"].as_str().map(str::to_string);
        if v["has_more"].as_bool() != Some(true) || page.is_none() {
            break;
        }
    }
    Ok(total)
}

pub async fn openai_month_to_date(client: &Client, key: &str) -> Result<f64, FetchError> {
    let start = month_start(Utc::now()).timestamp().to_string();
    let mut total = 0.0;
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut req = client
            .get(OPENAI_COST_URL)
            .query(&[("start_time", start.as_str()), ("bucket_width", "1d"), ("limit", "31")])
            .bearer_auth(key);
        if let Some(p) = &page {
            req = req.query(&[("page", p.as_str())]);
        }
        let v = get_json(req).await.map_err(|e| match e {
            FetchError::Auth(_) => auth_error("OpenAI"),
            e => e,
        })?;
        total += sum_buckets(&v, openai_amount);
        page = v["next_page"].as_str().map(str::to_string);
        if v["has_more"].as_bool() != Some(true) || page.is_none() {
            break;
        }
    }
    Ok(total)
}

/// 이번 달 사용액을 표시용 한도 하나로 만든다. 초기화 시각은 다음 달 1일(UTC)이다.
pub fn spend_limit(spent: f64, budget: Option<f64>) -> Limit {
    let now = Utc::now();
    let start = month_start(now);
    let next = next_month_start(now);
    let window = Some((next - start).num_seconds());
    match budget.filter(|b| *b > 0.0) {
        Some(b) => Limit {
            key: "budget".into(),
            label: "이번 달 사용액".into(),
            percent: spent / b * 100.0,
            resets_at: Some(next),
            window_secs: window,
            display: Some(format!("${spent:.2} / ${b:.0}")),
            no_bar: false,
        },
        None => Limit {
            key: "spend".into(),
            label: "이번 달 사용액".into(),
            percent: 0.0,
            resets_at: Some(next),
            window_secs: window,
            display: Some(format!("${spent:.2}")),
            no_bar: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_anthropic_cents() {
        let v = json!({"data": [
            {"starting_at": "2026-10-01T00:00:00Z", "results": [{"amount": "1234.5", "currency": "USD"}, {"amount": "100", "currency": "USD"}]},
            {"starting_at": "2026-10-02T00:00:00Z", "results": []}
        ], "has_more": false});
        assert!((sum_buckets(&v, anthropic_amount) - 13.345).abs() < 1e-9);
    }

    #[test]
    fn sums_openai_dollars() {
        let v = json!({"object": "page", "data": [
            {"object": "bucket", "results": [{"amount": {"value": 0.5, "currency": "usd"}}]},
            {"object": "bucket", "results": [{"amount": {"value": 1.25, "currency": "usd"}}]}
        ]});
        assert!((sum_buckets(&v, openai_amount) - 1.75).abs() < 1e-9);
    }

    #[test]
    fn month_boundaries() {
        let d = Utc.with_ymd_and_hms(2026, 12, 15, 10, 0, 0).unwrap();
        assert_eq!(month_start(d), Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap());
        assert_eq!(next_month_start(d), Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn spend_with_and_without_budget() {
        let l = spend_limit(12.3, Some(50.0));
        assert_eq!(l.key, "budget");
        assert!((l.percent - 24.6).abs() < 1e-9);
        assert_eq!(l.display.as_deref(), Some("$12.30 / $50"));
        let l = spend_limit(12.3, None);
        assert!(l.no_bar);
        assert_eq!(l.display.as_deref(), Some("$12.30"));
    }
}
