//! 주기적으로 모든 계정의 사용량을 조회하고, 한도 도달 예측(ETA)과 알림을 처리한다.

use crate::claude::{self, FetchError, Limit};
use crate::{billing, chatgpt};
use crate::config::{self, Account, Settings, Source};
use crate::AppState;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

#[derive(Debug, Clone, Serialize)]
pub struct LimitView {
    #[serde(flatten)]
    pub limit: Limit,
    /// 현재 속도로 100%에 도달하는 예상 시각
    pub eta: Option<DateTime<Utc>>,
    /// 현재 속도면 초기화 전에 한도에 닿지 않음
    pub safe_until_reset: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountStatus {
    pub account_id: String,
    pub ok: bool,
    pub error_kind: Option<String>,
    pub error: Option<String>,
    pub limits: Vec<LimitView>,
    pub extra_usage: bool,
    /// 카드 머리에 붙이는 짧은 정보 (예: "크레딧 120")
    pub badge: Option<String>,
    /// 마지막으로 성공한 조회 시각. 실패해도 이전 값을 유지한다.
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub t: DateTime<Utc>,
    pub p: f64,
    pub r: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct NotifyMark {
    level: u8,
    resets_at: Option<DateTime<Utc>>,
    percent: f64,
}

#[derive(Default)]
pub struct Backoff {
    until: Option<DateTime<Utc>>,
    factor: u32,
}

#[derive(Default)]
pub struct Runtime {
    pub statuses: HashMap<String, AccountStatus>,
    pub history: HashMap<String, Vec<Sample>>,
    pub marks: HashMap<String, NotifyMark>,
    pub backoff: HashMap<String, Backoff>,
}

const HISTORY_KEEP_DAYS: i64 = 7;

pub async fn run(app: AppHandle) {
    loop {
        refresh_all(&app).await;
        let secs = {
            let state = app.state::<AppState>();
            let base = state.config.lock().unwrap().settings.interval_secs.max(60) as f64;
            base * (0.9 + fastrand::f64() * 0.2)
        };
        let state = app.state::<AppState>();
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs_f64(secs)) => {}
            _ = state.wake.notified() => {}
        }
    }
}

/// 계정 하나의 조회 결과
#[derive(Default)]
struct Fetched {
    limits: Vec<Limit>,
    extra_usage: bool,
    plan: Option<String>,
    badge: Option<String>,
    /// 토큰을 갱신했으면 새로 저장할 비밀값
    new_secret: Option<String>,
}

async fn fetch(state: &AppState, account: &Account) -> Result<Fetched, FetchError> {
    let secret = || config::get_secret(&account.id).map_err(FetchError::Auth);
    match &account.source {
        Source::ClaudeWeb { org_uuid } => {
            let v = claude::web_usage(&state.client, &secret()?, org_uuid).await?;
            let (limits, extra_usage) = claude::parse_usage(&v);
            Ok(Fetched { limits, extra_usage, ..Default::default() })
        }
        Source::ClaudeCode { credentials_path } => {
            let (v, plan) = claude::code_usage(&state.client, credentials_path).await?;
            let (limits, extra_usage) = claude::parse_usage(&v);
            Ok(Fetched { limits, extra_usage, plan, ..Default::default() })
        }
        Source::ChatgptWeb { .. } => {
            let s = chatgpt::Secret::from_json(&secret()?)?;
            let (v, updated) = chatgpt::usage(&state.client, &s).await?;
            let (limits, plan, badge) = chatgpt::parse_usage(&v);
            Ok(Fetched { limits, plan, badge, new_secret: updated.map(|u| u.to_json()), ..Default::default() })
        }
        Source::AnthropicAdmin => {
            let spent = billing::anthropic_month_to_date(&state.client, &secret()?).await?;
            Ok(Fetched { limits: vec![billing::spend_limit(spent, account.budget_usd)], ..Default::default() })
        }
        Source::OpenaiAdmin => {
            let spent = billing::openai_month_to_date(&state.client, &secret()?).await?;
            Ok(Fetched { limits: vec![billing::spend_limit(spent, account.budget_usd)], ..Default::default() })
        }
    }
}

pub async fn refresh_all(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (accounts, settings) = {
        let c = state.config.lock().unwrap();
        (c.accounts.clone(), c.settings.clone())
    };
    let now = Utc::now();

    for account in &accounts {
        let skip = state
            .runtime
            .lock()
            .unwrap()
            .backoff
            .get(&account.id)
            .and_then(|b| b.until)
            .is_some_and(|u| u > now);
        if skip {
            continue;
        }

        let result = fetch(&state, account).await;
        let mut notes: Vec<(String, String)> = Vec::new();
        {
            let mut rt = state.runtime.lock().unwrap();
            let now = Utc::now();
            match result {
                Ok(Fetched { limits, extra_usage, plan, badge, new_secret }) => {
                    rt.backoff.remove(&account.id);
                    if let Some(s) = new_secret {
                        let _ = config::set_secret(&account.id, &s);
                    }
                    if let Some(p) = plan {
                        if account.plan.as_deref() != Some(&p) {
                            let mut c = state.config.lock().unwrap();
                            if let Some(a) = c.accounts.iter_mut().find(|a| a.id == account.id) {
                                a.plan = Some(p);
                            }
                            let _ = c.save(&state.config_path);
                        }
                    }
                    let views = limits
                        .into_iter()
                        .map(|l| {
                            let lookback = match l.key.as_str() {
                                "session" => Some(Duration::minutes(45)),
                                "weekly" => Some(Duration::hours(24)),
                                _ => None,
                            };
                            let (eta, safe) = match lookback {
                                Some(lb) => {
                                    let hk = format!("{}|{}", account.id, l.key);
                                    let hist = rt.history.entry(hk).or_default();
                                    push_sample(hist, now, l.percent, l.resets_at);
                                    estimate(hist, now, l.percent, l.resets_at, lb)
                                }
                                None => (None, false),
                            };
                            let mk = format!("{}|{}", account.id, l.key);
                            let mark = rt.marks.entry(mk).or_default();
                            notes.extend(check_notify(mark, account, &l, &settings));
                            LimitView { limit: l, eta, safe_until_reset: safe }
                        })
                        .collect();
                    rt.statuses.insert(
                        account.id.clone(),
                        AccountStatus {
                            account_id: account.id.clone(),
                            ok: true,
                            error_kind: None,
                            error: None,
                            limits: views,
                            extra_usage,
                            badge,
                            updated_at: Some(now),
                        },
                    );
                }
                Err(e) => {
                    if matches!(e, FetchError::RateLimited | FetchError::Blocked) {
                        let b = rt.backoff.entry(account.id.clone()).or_default();
                        b.factor = (b.factor.max(1) * 2).min(16);
                        b.until = Some(now + Duration::seconds((settings.interval_secs * b.factor as u64) as i64));
                    }
                    let prev = rt.statuses.remove(&account.id);
                    rt.statuses.insert(
                        account.id.clone(),
                        AccountStatus {
                            account_id: account.id.clone(),
                            ok: false,
                            error_kind: Some(e.kind().to_string()),
                            error: Some(e.message()),
                            limits: prev.as_ref().map(|p| p.limits.clone()).unwrap_or_default(),
                            extra_usage: prev.as_ref().is_some_and(|p| p.extra_usage),
                            badge: prev.as_ref().and_then(|p| p.badge.clone()),
                            updated_at: prev.and_then(|p| p.updated_at),
                        },
                    );
                }
            }
        }
        if settings.notifications {
            for (title, body) in notes {
                let _ = app.notification().builder().title(title).body(body).show();
            }
        }
    }

    {
        let mut rt = state.runtime.lock().unwrap();
        let ids: Vec<&String> = accounts.iter().map(|a| &a.id).collect();
        rt.statuses.retain(|k, _| ids.contains(&k));
        let _ = save_history(&state, &rt.history);
    }
    crate::emit_snapshot(app);
    crate::tray::update_tooltip(app);
}

fn push_sample(hist: &mut Vec<Sample>, now: DateTime<Utc>, p: f64, r: Option<DateTime<Utc>>) {
    let cutoff = now - Duration::days(HISTORY_KEEP_DAYS);
    hist.retain(|s| s.t >= cutoff);
    // 값이 같으면 2분에 한 번만 기록해서 파일이 불어나지 않게 한다.
    if let Some(last) = hist.last() {
        if (last.p - p).abs() < f64::EPSILON && same_window(last.r, r) && now - last.t < Duration::minutes(2) {
            return;
        }
    }
    hist.push(Sample { t: now, p, r });
}

fn same_window(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() < 300,
        (None, None) => true,
        _ => false,
    }
}

/// 같은 초기화 구간 안에서 최근 `lookback` 동안의 증가 속도로 100% 도달 시각을 추정한다.
fn estimate(
    hist: &[Sample],
    now: DateTime<Utc>,
    p: f64,
    reset: Option<DateTime<Utc>>,
    lookback: Duration,
) -> (Option<DateTime<Utc>>, bool) {
    if p >= 100.0 {
        return (Some(now), false);
    }
    let base = hist
        .iter()
        .find(|s| s.t >= now - lookback && same_window(s.r, reset));
    let Some(base) = base else { return (None, false) };
    let dt = (now - base.t).num_seconds() as f64;
    if dt < 300.0 {
        return (None, false);
    }
    let dp = p - base.p;
    if dp <= 0.0 {
        return (None, reset.is_some());
    }
    let secs_left = (100.0 - p) / (dp / dt);
    let eta = now + Duration::seconds(secs_left as i64);
    match reset {
        Some(r) if eta >= r => (None, true),
        _ => (Some(eta), false),
    }
}

fn check_notify(mark: &mut NotifyMark, account: &Account, l: &Limit, s: &Settings) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let title = format!("{} · {}", account.label, l.label);

    let new_window = mark.resets_at.is_some() && !same_window(mark.resets_at, l.resets_at);
    if new_window {
        if s.notify_reset && l.key == "session" && mark.percent >= 1.0 && l.percent < mark.percent {
            out.push((title.clone(), format!("사용량이 초기화되었습니다. (현재 {:.0}%)", l.percent)));
        }
        mark.level = 0;
    }

    let level = if l.percent >= s.crit_percent {
        2
    } else if l.percent >= s.warn_percent {
        1
    } else {
        0
    };
    if level > mark.level {
        let when = l
            .resets_at
            .map(|r| format!(" · {} 초기화", r.with_timezone(&chrono::Local).format("%m/%d %H:%M")))
            .unwrap_or_default();
        out.push((title, format!("{:.0}% 사용했습니다{when}", l.percent)));
    }
    mark.level = level;
    mark.resets_at = l.resets_at;
    mark.percent = l.percent;
    out
}

pub fn load_history(path: &std::path::Path) -> HashMap<String, Vec<Sample>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(state: &AppState, history: &HashMap<String, Vec<Sample>>) -> Result<(), String> {
    let data = serde_json::to_vec(history).map_err(|e| e.to_string())?;
    config::write_atomic(&state.history_path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_from_rate() {
        let now = Utc::now();
        let reset = Some(now + Duration::hours(3));
        let hist = vec![Sample { t: now - Duration::minutes(30), p: 40.0, r: reset }];
        // 30분에 20%p → 남은 40%p는 60분 뒤
        let (eta, safe) = estimate(&hist, now, 60.0, reset, Duration::minutes(45));
        let mins = (eta.unwrap() - now).num_minutes();
        assert!((59..=60).contains(&mins));
        assert!(!safe);
    }

    #[test]
    fn safe_when_reset_comes_first() {
        let now = Utc::now();
        let reset = Some(now + Duration::minutes(20));
        let hist = vec![Sample { t: now - Duration::minutes(30), p: 40.0, r: reset }];
        assert_eq!(estimate(&hist, now, 60.0, reset, Duration::minutes(45)), (None, true));
    }

    #[test]
    fn ignores_previous_window() {
        let now = Utc::now();
        let old = Some(now - Duration::minutes(10));
        let reset = Some(now + Duration::hours(4));
        let hist = vec![Sample { t: now - Duration::minutes(30), p: 90.0, r: old }];
        assert_eq!(estimate(&hist, now, 5.0, reset, Duration::minutes(45)), (None, false));
    }
}
