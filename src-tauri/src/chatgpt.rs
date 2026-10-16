//! ChatGPT 구독의 Codex 사용 한도 조회.
//!
//! Codex CLI가 쓰는 `GET https://chatgpt.com/backend-api/wham/usage`를 같은 방식으로 호출한다
//! (Bearer 액세스 토큰 + `ChatGPT-Account-Id`). 비공식 엔드포인트라 형식이 바뀔 수 있다.
//! 액세스 토큰은 며칠 만에 만료되므로, 로그인 때 받아 둔 chatgpt.com 쿠키로
//! `/api/auth/session`을 다시 불러 새 토큰을 받는다.
//!
//! 일반 채팅 메시지 한도는 조회 수단이 없어 다루지 않는다.

use crate::claude::{get_json, FetchError, Limit, SESSION_SECS, WEEK_SECS};
use base64::Engine;
use chrono::{DateTime, Duration, TimeZone, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const SESSION_COOKIE: &str = "__Secure-next-auth.session-token";

/// OS 보안 저장소에 JSON으로 저장하는 로그인 정보.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secret {
    pub access_token: String,
    /// chatgpt.com 쿠키 ("이름=값; 이름=값")
    pub cookies: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl Secret {
    pub fn from_json(s: &str) -> Result<Self, FetchError> {
        serde_json::from_str(s).map_err(|_| FetchError::Auth("저장된 ChatGPT 로그인 정보가 손상되었습니다. 다시 로그인해 주세요.".into()))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// 로그인 창의 세션 응답(`/api/auth/session`)에서 꺼낸 값.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub access_token: String,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan: Option<String>,
}

pub fn parse_session(v: &Value) -> Option<SessionInfo> {
    let token = v["accessToken"].as_str().filter(|t| !t.is_empty())?.to_string();
    let claims = jwt_claims(&token).unwrap_or(Value::Null);
    let auth = &claims["https://api.openai.com/auth"];
    Some(SessionInfo {
        email: v["user"]["email"].as_str().map(str::to_string),
        account_id: v["account"]["id"]
            .as_str()
            .or(auth["chatgpt_account_id"].as_str())
            .map(str::to_string),
        plan: v["account"]["planType"]
            .as_str()
            .or(auth["chatgpt_plan_type"].as_str())
            .map(plan_label),
        access_token: token,
    })
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn token_expires_soon(token: &str) -> bool {
    let exp = jwt_claims(token).and_then(|c| c["exp"].as_i64());
    match exp {
        Some(exp) => exp - 300 < Utc::now().timestamp(),
        None => false,
    }
}

pub fn plan_label(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "edu" => "Edu".into(),
        "go" => "Go".into(),
        "free" => "Free".into(),
        other => other.to_string(),
    }
}

/// 응답의 Set-Cookie로 바뀐 세션 쿠키를 저장된 쿠키 문자열에 반영한다.
fn merge_cookies(cookies: &str, set_cookies: &[String]) -> String {
    let mut pairs: Vec<(String, String)> = cookies
        .split(';')
        .filter_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    for sc in set_cookies {
        let first = sc.split(';').next().unwrap_or("");
        let Some((k, v)) = first.trim().split_once('=') else { continue };
        if !k.starts_with(SESSION_COOKIE) {
            continue;
        }
        match pairs.iter_mut().find(|(name, _)| name == k) {
            Some(p) => p.1 = v.to_string(),
            None => pairs.push((k.to_string(), v.to_string())),
        }
    }
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
}

async fn refresh(client: &Client, secret: &Secret) -> Result<Secret, FetchError> {
    let res = client
        .get(SESSION_URL)
        .header("Cookie", &secret.cookies)
        .header("User-Agent", crate::claude::USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    let set_cookies: Vec<String> = res
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    if status == 403 || status == 503 {
        return Err(FetchError::Auth(
            "ChatGPT 로그인 갱신이 차단되었습니다. 다시 로그인해 주세요.".into(),
        ));
    }
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        FetchError::Auth("ChatGPT 로그인이 만료되었습니다. 다시 로그인해 주세요.".into())
    })?;
    let info = parse_session(&v).ok_or_else(|| {
        FetchError::Auth("ChatGPT 로그인이 만료되었습니다. 다시 로그인해 주세요.".into())
    })?;
    Ok(Secret {
        access_token: info.access_token,
        cookies: merge_cookies(&secret.cookies, &set_cookies),
        account_id: info.account_id.or_else(|| secret.account_id.clone()),
    })
}

async fn call_usage(client: &Client, secret: &Secret) -> Result<Value, FetchError> {
    let mut req = client
        .get(USAGE_URL)
        .bearer_auth(&secret.access_token)
        .header("Accept", "application/json")
        .header("User-Agent", "ai-usage-checker");
    if let Some(acc) = &secret.account_id {
        req = req.header("ChatGPT-Account-Id", acc);
    }
    get_json(req).await
}

/// 사용량을 조회한다. 토큰을 갱신했다면 새 로그인 정보도 돌려준다(저장용).
pub async fn usage(client: &Client, secret: &Secret) -> Result<(Value, Option<Secret>), FetchError> {
    let mut current = secret.clone();
    let mut updated = None;
    if token_expires_soon(&current.access_token) {
        current = refresh(client, &current).await?;
        updated = Some(current.clone());
    }
    match call_usage(client, &current).await {
        Err(FetchError::Auth(_)) if updated.is_none() => {
            current = refresh(client, &current).await?;
            let v = call_usage(client, &current).await?;
            Ok((v, Some(current)))
        }
        r => Ok((r?, updated)),
    }
}

fn window_label(secs: i64) -> String {
    match secs {
        SESSION_SECS => "5시간 한도".into(),
        WEEK_SECS => "주간 한도".into(),
        s if s % 86400 == 0 => format!("{}일 한도", s / 86400),
        s if s % 3600 == 0 => format!("{}시간 한도", s / 3600),
        s => format!("{}분 한도", s / 60),
    }
}

fn parse_window(w: &Value, now: DateTime<Utc>) -> Option<(i64, f64, Option<DateTime<Utc>>)> {
    if !w.is_object() {
        return None;
    }
    let secs = w["limit_window_seconds"].as_i64().unwrap_or(0);
    let percent = w["used_percent"].as_f64().unwrap_or(0.0);
    let resets_at = w["reset_at"]
        .as_i64()
        .filter(|t| *t > 0)
        .and_then(|t| Utc.timestamp_opt(t, 0).single())
        .or_else(|| w["reset_after_seconds"].as_i64().map(|s| now + Duration::seconds(s)));
    Some((secs, percent, resets_at))
}

/// 응답을 한도 목록으로 바꾼다. 요금제 이름과 크레딧 잔액 표시도 함께 돌려준다.
pub fn parse_usage(v: &Value) -> (Vec<Limit>, Option<String>, Option<String>) {
    let now = Utc::now();
    let mut out = Vec::new();

    let rl = &v["rate_limit"];
    for w in [&rl["primary_window"], &rl["secondary_window"]] {
        let Some((secs, percent, resets_at)) = parse_window(w, now) else { continue };
        let key = match secs {
            s if s > 0 && s <= 6 * 3600 => "session".to_string(),
            s if s >= 6 * 86400 => "weekly".to_string(),
            s => format!("window:{s}"),
        };
        out.push(Limit {
            key,
            label: window_label(secs),
            percent,
            resets_at,
            window_secs: (secs > 0).then_some(secs),
            ..Default::default()
        });
    }

    if let Some(extra) = v["additional_rate_limits"].as_array() {
        for a in extra {
            let name = a["limit_name"].as_str().or(a["metered_feature"].as_str()).unwrap_or("추가 한도");
            let rl = &a["rate_limit"];
            for w in [&rl["primary_window"], &rl["secondary_window"]] {
                let Some((secs, percent, resets_at)) = parse_window(w, now) else { continue };
                let dur = window_label(secs).trim_end_matches(" 한도").to_string();
                out.push(Limit {
                    key: format!("model:{name} {dur}"),
                    label: format!("{name} · {dur}"),
                    percent,
                    resets_at,
                    window_secs: (secs > 0).then_some(secs),
                    ..Default::default()
                });
            }
        }
    }

    let plan = v["plan_type"].as_str().map(plan_label);
    let credits = &v["credits"];
    let badge = if credits["unlimited"].as_bool() == Some(true) {
        Some("크레딧 무제한".to_string())
    } else if credits["has_credits"].as_bool() == Some(true) {
        let bal = credits["balance"]
            .as_str()
            .map(str::to_string)
            .or_else(|| credits["balance"].as_f64().map(|b| format!("{b:.0}")));
        bal.map(|b| format!("크레딧 {b}"))
    } else {
        None
    };
    (out, plan, badge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_codex_usage() {
        let v = json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": 1792200000},
                "secondary_window": {"used_percent": 7, "limit_window_seconds": 604800, "reset_after_seconds": 86400, "reset_at": 0}
            },
            "additional_rate_limits": [
                {"limit_name": "GPT-5-Codex-Max", "metered_feature": "x", "rate_limit": {
                    "primary_window": {"used_percent": 10, "limit_window_seconds": 18000, "reset_after_seconds": 60, "reset_at": 1792200000}
                }}
            ],
            "credits": {"has_credits": true, "unlimited": false, "balance": "120"}
        });
        let (l, plan, badge) = parse_usage(&v);
        assert_eq!(plan.as_deref(), Some("Plus"));
        assert_eq!(badge.as_deref(), Some("크레딧 120"));
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].key, "session");
        assert_eq!(l[0].label, "5시간 한도");
        assert_eq!(l[0].percent, 42.0);
        assert_eq!(l[0].resets_at.unwrap().timestamp(), 1792200000);
        assert_eq!(l[1].key, "weekly");
        // reset_at이 0이면 reset_after_seconds로 계산
        assert!(l[1].resets_at.unwrap() > Utc::now());
        assert_eq!(l[2].label, "GPT-5-Codex-Max · 5시간");
    }

    #[test]
    fn session_info_from_jwt() {
        let claims = json!({"exp": 9999999999i64, "https://api.openai.com/auth": {"chatgpt_account_id": "acc-1", "chatgpt_plan_type": "pro"}});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        let token = format!("h.{payload}.s");
        let info = parse_session(&json!({"accessToken": token, "user": {"email": "a@b.c"}})).unwrap();
        assert_eq!(info.account_id.as_deref(), Some("acc-1"));
        assert_eq!(info.plan.as_deref(), Some("Pro"));
        assert_eq!(info.email.as_deref(), Some("a@b.c"));
        assert!(!token_expires_soon(&info.access_token));
        assert!(parse_session(&json!({})).is_none());
    }

    #[test]
    fn merges_rotated_session_cookie() {
        let merged = merge_cookies(
            "a=1; __Secure-next-auth.session-token=old; b=2",
            &["__Secure-next-auth.session-token=new; Path=/; HttpOnly".into(), "other=x; Path=/".into()],
        );
        assert_eq!(merged, "a=1; __Secure-next-auth.session-token=new; b=2");
    }
}
