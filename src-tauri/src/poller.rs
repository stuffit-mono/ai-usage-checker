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
    /// 최근 7일 사용률 추이 (5시간 한도, 없으면 주간 한도)
    pub trend: Option<Trend>,
    /// 5시간 한도 100%를 쓸 때 주간 한도가 몇 %p 줄어드는지 (최근 7일 기록으로 계산)
    pub weekly_per_session: Option<f64>,
    /// 마지막으로 성공한 조회 시각. 실패해도 이전 값을 유지한다.
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trend {
    /// 어떤 한도의 추이인지 ("session" 또는 "weekly")
    pub key: String,
    /// 가장 오래된 칸의 시작 시각
    pub start: DateTime<Utc>,
    pub bucket_hours: i64,
    /// 칸마다 그 시간대의 최대 사용률. 기록이 없으면 null
    pub buckets: Vec<Option<f64>>,
}

const TREND_DAYS: i64 = 7;
const TREND_BUCKET_HOURS: i64 = 2;

/// 최근 7일 기록을 2시간 칸으로 묶어 칸마다 최대값을 구한다.
fn build_trend(key: &str, hist: &[Sample], now: DateTime<Utc>) -> Option<Trend> {
    let n = (TREND_DAYS * 24 / TREND_BUCKET_HOURS) as usize;
    let bucket = Duration::hours(TREND_BUCKET_HOURS);
    // 칸 경계를 정시에 맞춘다
    let end = now - Duration::seconds(now.timestamp() % bucket.num_seconds()) + bucket;
    let start = end - bucket * n as i32;
    let mut buckets: Vec<Option<f64>> = vec![None; n];
    for s in hist.iter().filter(|s| s.t >= start && s.t < end) {
        let i = ((s.t - start).num_seconds() / bucket.num_seconds()) as usize;
        let slot = &mut buckets[i.min(n - 1)];
        *slot = Some(slot.map_or(s.p, |v| v.max(s.p)));
    }
    buckets.iter().any(Option::is_some).then(|| Trend {
        key: key.to_string(),
        start,
        bucket_hours: TREND_BUCKET_HOURS,
        buckets,
    })
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
    /// 로그인 만료 알림을 이미 보낸 계정. 다시 조회에 성공하면 지운다.
    pub auth_notified: std::collections::HashSet<String>,
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
        Source::AnthropicAdmin | Source::OpenaiAdmin => {
            let provider = if matches!(account.source, Source::AnthropicAdmin) {
                billing::Provider::Anthropic
            } else {
                billing::Provider::Openai
            };
            // 이번 달 1일과 크레딧 입력일 중 이른 날부터 한 번에 받아 두 값을 함께 계산한다
            let month = billing::month_start(Utc::now());
            let start = account.credit.as_ref().map_or(month, |c| c.set_at.min(month));
            let daily = billing::daily_costs(&state.client, provider, &secret()?, start).await?;
            let mut limits = Vec::new();
            if let Some(c) = &account.credit {
                limits.push(billing::credit_limit(c, &daily));
            }
            limits.push(billing::spend_limit(billing::sum_since(&daily, month), account.budget_usd));
            Ok(Fetched { limits, ..Default::default() })
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
                    rt.auth_notified.remove(&account.id);
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
                                // 크레딧은 하루 단위로만 집계되므로 최근 3일 속도로 소진 시점을 본다
                                "credit" => Some(Duration::days(3)),
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
                    let trend = ["session", "weekly"].iter().find_map(|k| {
                        let hist = rt.history.get(&format!("{}|{}", account.id, k))?;
                        build_trend(k, hist, now)
                    });
                    let weekly_per_session = match (
                        rt.history.get(&format!("{}|session", account.id)),
                        rt.history.get(&format!("{}|weekly", account.id)),
                    ) {
                        (Some(s), Some(w)) => weekly_per_session(s, w, now),
                        _ => None,
                    };
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
                            trend,
                            weekly_per_session,
                            updated_at: Some(now),
                        },
                    );
                }
                Err(e) => {
                    // 로그인(인증)이 만료되면 위젯을 숨겨 둬도 알 수 있게 한 번만 알린다
                    if matches!(e, FetchError::Auth(_)) && rt.auth_notified.insert(account.id.clone()) {
                        // 메시지 예: "로그인이 만료되었습니다. 다시 로그인해 주세요." / "OpenAI Admin 키가 올바르지 않거나…"
                        notes.push((format!("{} · 다시 로그인 필요", account.label), e.message()));
                    }
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
                            trend: prev.as_ref().and_then(|p| p.trend.clone()),
                            weekly_per_session: prev.as_ref().and_then(|p| p.weekly_per_session),
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

/// 비율 계산에 쓸 최소 5시간 사용량 합(%p). 이보다 적으면 오차가 커서 표시하지 않는다.
const RATIO_MIN_SESSION_PP: f64 = 20.0;
/// 이 간격보다 멀리 떨어진 기록은 앱이 꺼져 있던 구간으로 보고 계산에서 뺀다.
const RATIO_MAX_GAP_MINUTES: i64 = 40;

/// 최근 7일 기록에서 "5시간 한도 100%를 쓸 때 주간 한도가 몇 %p 줄어드는지"를 구한다.
/// 이웃한 5시간 기록 두 개 사이에서 5시간 사용률이 오른 만큼과, 같은 시점의 주간 사용률이 오른 만큼을
/// 각각 더해 비율을 낸다. 초기화를 사이에 둔 구간과 앱이 꺼져 있던 구간은 짝이 맞지 않아 뺀다.
fn weekly_per_session(session: &[Sample], weekly: &[Sample], now: DateTime<Utc>) -> Option<f64> {
    let from = now - Duration::days(7);
    // 기록은 값이 바뀔 때(또는 몇 분마다) 남으므로, 어떤 시각의 주간 값은 그 시각 이전의 마지막 기록이다
    let weekly_at = |t: DateTime<Utc>| weekly.iter().rev().find(|w| w.t <= t);
    let (mut ds, mut dw) = (0.0, 0.0);
    for pair in session.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.t < from || b.t - a.t > Duration::minutes(RATIO_MAX_GAP_MINUTES) || !same_window(a.r, b.r) {
            continue;
        }
        let (Some(wa), Some(wb)) = (weekly_at(a.t), weekly_at(b.t)) else { continue };
        if !same_window(wa.r, wb.r) || b.t - wa.t > Duration::minutes(RATIO_MAX_GAP_MINUTES) {
            continue;
        }
        let (d_session, d_weekly) = (b.p - a.p, wb.p - wa.p);
        if d_session < 0.0 || d_weekly < 0.0 {
            continue;
        }
        ds += d_session;
        dw += d_weekly;
    }
    (ds >= RATIO_MIN_SESSION_PP).then(|| dw / ds * 100.0)
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
    fn trend_buckets_take_max_and_skip_old() {
        let now = Utc::now();
        let hist = vec![
            Sample { t: now - Duration::days(8), p: 99.0, r: None }, // 7일보다 오래됨
            Sample { t: now - Duration::minutes(30), p: 10.0, r: None },
            Sample { t: now - Duration::minutes(5), p: 30.0, r: None },
        ];
        let t = build_trend("session", &hist, now).unwrap();
        assert_eq!(t.buckets.len(), 84);
        let filled: Vec<f64> = t.buckets.iter().flatten().copied().collect();
        assert!(!filled.contains(&99.0));
        assert_eq!(filled.iter().cloned().fold(0.0, f64::max), 30.0);
        assert!(t.start <= now - Duration::days(7) + Duration::hours(2));
        assert!(build_trend("session", &[], now).is_none());
    }

    #[test]
    fn ratio_from_paired_increases() {
        let now = Utc::now();
        let sr = Some(now + Duration::hours(2));
        let wr = Some(now + Duration::days(3));
        let at = |m: i64| now - Duration::minutes(m);
        // 5시간: 0 → 10 → 30 → 50 (3분 간격), 주간: 0 → 2 → 6 → 10. 5시간 50%p에 주간 10%p → 100%당 20%
        let session = vec![
            Sample { t: at(9), p: 0.0, r: sr },
            Sample { t: at(6), p: 10.0, r: sr },
            Sample { t: at(3), p: 30.0, r: sr },
            Sample { t: at(0), p: 50.0, r: sr },
        ];
        let weekly = vec![
            Sample { t: at(9), p: 0.0, r: wr },
            Sample { t: at(6), p: 2.0, r: wr },
            Sample { t: at(3), p: 6.0, r: wr },
            Sample { t: at(0), p: 10.0, r: wr },
        ];
        let r = weekly_per_session(&session, &weekly, now).unwrap();
        assert!((r - 20.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_skips_gaps_and_needs_enough_data() {
        let now = Utc::now();
        let sr = Some(now + Duration::hours(2));
        let wr = Some(now + Duration::days(3));
        // 두 기록 사이가 3시간(앱이 꺼져 있던 구간)이면 계산하지 않는다
        let session = vec![Sample { t: now - Duration::hours(3), p: 0.0, r: sr }, Sample { t: now, p: 80.0, r: sr }];
        let weekly = vec![Sample { t: now - Duration::hours(3), p: 0.0, r: wr }, Sample { t: now, p: 30.0, r: wr }];
        assert_eq!(weekly_per_session(&session, &weekly, now), None);
        // 5시간 사용량 합이 20%p 미만이면 표시하지 않는다
        let session = vec![Sample { t: now - Duration::minutes(3), p: 0.0, r: sr }, Sample { t: now, p: 5.0, r: sr }];
        let weekly = vec![Sample { t: now - Duration::minutes(3), p: 0.0, r: wr }, Sample { t: now, p: 1.0, r: wr }];
        assert_eq!(weekly_per_session(&session, &weekly, now), None);
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
