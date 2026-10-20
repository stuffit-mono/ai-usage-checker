//! API 종량제 사용액 조회 (Admin API 키).
//!
//! - Anthropic: `GET /v1/organizations/cost_report` — 일 단위 버킷, 금액은 센트 단위 소수 문자열
//! - OpenAI: `GET /v1/organization/costs` — 일 단위 버킷, 금액은 달러(`amount.value`)
//!
//! 두 곳 모두 남은 선불 크레딧 잔액은 API로 제공하지 않는다. 그래서 이번 달(UTC) 사용액을 보여주고,
//! 사용자가 남은 크레딧을 한 번 입력하면 그 뒤 사용액을 빼서 남은 크레딧을 계산한다.
//! 월 예산을 정하면 예산 대비 비율도 함께 보여준다.

use crate::claude::{get_json, FetchError, Limit};
use crate::config::Credit;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

const ANTHROPIC_COST_URL: &str = "https://api.anthropic.com/v1/organizations/cost_report";
const OPENAI_COST_URL: &str = "https://api.openai.com/v1/organization/costs";
/// 페이지 상한 (한 페이지 31일, 약 1년치). 무한 루프 방지
const MAX_PAGES: usize = 13;

#[derive(Debug, Clone, Copy)]
pub enum Provider {
    Anthropic,
    Openai,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::Openai => "OpenAI",
        }
    }
}

/// 하루(UTC) 단위 사용액 목록: (그날 0시, 금액 USD)
pub type Daily = Vec<(DateTime<Utc>, f64)>;

pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0).unwrap()
}

pub fn next_month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = if now.month() == 12 { (now.year() + 1, 1) } else { (now.year(), now.month() + 1) };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap()
}

pub fn day_start(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(t.year(), t.month(), t.day(), 0, 0, 0).unwrap()
}

fn auth_error(provider: Provider) -> FetchError {
    FetchError::Auth(format!("{} Admin 키가 올바르지 않거나 권한이 없습니다.", provider.name()))
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

/// 응답 한 페이지의 버킷들을 (그날 0시, 금액)으로 바꾼다.
fn parse_daily(v: &Value, provider: Provider) -> Daily {
    let Some(buckets) = v["data"].as_array() else { return Vec::new() };
    buckets
        .iter()
        .filter_map(|b| {
            let start = match provider {
                Provider::Anthropic => b["starting_at"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc)),
                Provider::Openai => b["start_time"].as_i64().and_then(|t| Utc.timestamp_opt(t, 0).single()),
            }?;
            let amount_of = match provider {
                Provider::Anthropic => anthropic_amount,
                Provider::Openai => openai_amount,
            };
            let total: f64 = b["results"].as_array().map(|r| r.iter().map(amount_of).sum()).unwrap_or(0.0);
            Some((start, total))
        })
        .collect()
}

/// 조회 범위의 최소 길이. 하루도 안 되는 짧은 구간(예: 오늘 0시부터)은 서버가 거절할 수 있어
/// 항상 이만큼은 받고, 필요한 기간만 `sum_since`로 골라 쓴다.
const MIN_QUERY_DAYS: i64 = 7;

/// `start`(UTC 0시로 내림)부터 지금까지 하루 단위 사용액을 가져온다. 최소 7일 전부터 받는다.
pub async fn daily_costs(client: &Client, provider: Provider, key: &str, start: DateTime<Utc>) -> Result<Daily, FetchError> {
    let start = day_start(start.min(Utc::now() - chrono::Duration::days(MIN_QUERY_DAYS)));
    let mut out = Daily::new();
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut req = match provider {
            Provider::Anthropic => client
                .get(ANTHROPIC_COST_URL)
                .query(&[
                    ("starting_at", start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true).as_str()),
                    ("limit", "31"),
                ])
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            Provider::Openai => client
                .get(OPENAI_COST_URL)
                .query(&[("start_time", start.timestamp().to_string().as_str()), ("bucket_width", "1d"), ("limit", "31")])
                .bearer_auth(key),
        };
        if let Some(p) = &page {
            req = req.query(&[("page", p.as_str())]);
        }
        let v = get_json(req).await.map_err(|e| match e {
            FetchError::Auth(_) => auth_error(provider),
            e => e,
        })?;
        out.extend(parse_daily(&v, provider));
        page = v["next_page"].as_str().map(str::to_string);
        if v["has_more"].as_bool() != Some(true) || page.is_none() {
            break;
        }
    }
    Ok(out)
}

/// `since`(UTC 0시로 내림) 이후 사용액 합계
pub fn sum_since(daily: &Daily, since: DateTime<Utc>) -> f64 {
    let since = day_start(since);
    daily.iter().filter(|(t, _)| *t >= since).map(|(_, a)| a).sum()
}

/// 입력한 크레딧에서 그 뒤 사용액을 뺀 남은 크레딧. 막대는 입력한 크레딧 중 쓴 비율을 보여준다.
pub fn credit_limit(c: &Credit, daily: &Daily) -> Limit {
    let used = (sum_since(daily, c.set_at) - c.baseline_spent).max(0.0);
    let left = c.balance - used;
    let percent = if c.balance > 0.0 { used / c.balance * 100.0 } else { 100.0 };
    let since = c.set_at.with_timezone(&chrono::Local).format("%m/%d");
    Limit {
        key: "credit".into(),
        label: "남은 크레딧".into(),
        percent,
        display: Some(if left > 0.0 { format!("${left:.2}") } else { "소진".into() }),
        note: Some(format!("{since} 입력한 ${:.2}에서 ${used:.2} 사용", c.balance)),
        ..Default::default()
    }
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
            ..Default::default()
        },
        None => Limit {
            key: "spend".into(),
            label: "이번 달 사용액".into(),
            percent: 0.0,
            resets_at: Some(next),
            window_secs: window,
            display: Some(format!("${spent:.2}")),
            no_bar: true,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_anthropic_cents() {
        let v = json!({"data": [
            {"starting_at": "2026-10-01T00:00:00Z", "results": [{"amount": "1234.5", "currency": "USD"}, {"amount": "100", "currency": "USD"}]},
            {"starting_at": "2026-10-02T00:00:00Z", "results": []}
        ], "has_more": false});
        let d = parse_daily(&v, Provider::Anthropic);
        assert_eq!(d.len(), 2);
        assert!((d[0].1 - 13.345).abs() < 1e-9);
        assert_eq!(d[0].0, Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn parses_openai_dollars() {
        let v = json!({"object": "page", "data": [
            {"object": "bucket", "start_time": 1790812800, "results": [{"amount": {"value": 0.5, "currency": "usd"}}]},
            {"object": "bucket", "start_time": 1790899200, "results": [{"amount": {"value": 1.25, "currency": "usd"}}]}
        ]});
        let d = parse_daily(&v, Provider::Openai);
        let total: f64 = d.iter().map(|x| x.1).sum();
        assert!((total - 1.75).abs() < 1e-9);
    }

    #[test]
    fn credit_subtracts_spend_after_baseline() {
        let day = |d| Utc.with_ymd_and_hms(2026, 10, d, 0, 0, 0).unwrap();
        // 10/19 오후에 $50 입력. 그날 입력 전까지 이미 $3를 썼다.
        let c = Credit { balance: 50.0, set_at: day(19) + chrono::Duration::hours(15), baseline_spent: 3.0 };
        let daily: Daily = vec![(day(18), 9.0), (day(19), 5.0), (day(20), 2.5)];
        let l = credit_limit(&c, &daily);
        // 입력 뒤 사용액 = (5 + 2.5) - 3 = 4.5 → 남은 $45.50, 9% 사용. 10/18 사용액은 빼지 않는다.
        assert_eq!(l.key, "credit");
        assert_eq!(l.display.as_deref(), Some("$45.50"));
        assert!((l.percent - 9.0).abs() < 1e-9);
        // 크레딧보다 많이 쓰면 "소진"
        let l = credit_limit(&Credit { balance: 4.0, ..c }, &daily);
        assert_eq!(l.display.as_deref(), Some("소진"));
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
