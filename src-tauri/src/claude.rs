//! Claude 사용량 조회.
//!
//! - claude.ai 세션: `GET https://claude.ai/api/organizations/{org}/usage` (sessionKey 쿠키)
//! - Claude Code OAuth: `GET https://api.anthropic.com/api/oauth/usage`
//!
//! 둘 다 비공식 엔드포인트라 응답 형식이 바뀔 수 있다. 구형(top-level `five_hour`,
//! `seven_day`, `seven_day_<model>`)과 신형(`limits` 배열) 형식을 모두 읽는다.

use chrono::{DateTime, TimeZone, Utc};
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::Value;

const CLAUDE_AI: &str = "https://claude.ai";
const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";

/// 화면에 막대 하나로 그리는 한도(또는 사용액). Claude 외 서비스도 이 형태로 바꿔 쓴다.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Limit {
    /// "session", "weekly", "model:<이름>", "budget", "spend" 등
    pub key: String,
    pub label: String,
    pub percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    /// 한도 구간 길이(초). 지난 시간 막대를 그릴 때 쓴다.
    #[serde(default)]
    pub window_secs: Option<i64>,
    /// 퍼센트 대신 보여줄 값 (예: "$12.30 / $50")
    #[serde(default)]
    pub display: Option<String>,
    /// 사용량 막대를 그리지 않는다 (예산 없는 사용액)
    #[serde(default)]
    pub no_bar: bool,
}

pub const SESSION_SECS: i64 = 5 * 3600;
pub const WEEK_SECS: i64 = 7 * 24 * 3600;

#[derive(Debug)]
pub enum FetchError {
    Auth(String),
    RateLimited,
    Blocked,
    Http(u16),
    Network(String),
    Parse(String),
}

impl FetchError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth",
            Self::RateLimited => "rate_limited",
            Self::Blocked => "blocked",
            Self::Http(_) => "http",
            Self::Network(_) => "network",
            Self::Parse(_) => "parse",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Auth(m) => m.clone(),
            Self::RateLimited => "조회 요청이 너무 많아 잠시 쉬었다가 다시 시도합니다.".into(),
            Self::Blocked => "서버가 요청을 차단했습니다(Cloudflare). 잠시 후 다시 시도합니다.".into(),
            Self::Http(c) => format!("서버 오류 (HTTP {c})"),
            Self::Network(m) => format!("네트워크 오류: {m}"),
            Self::Parse(m) => format!("응답을 해석하지 못했습니다: {m}"),
        }
    }
}

pub(crate) async fn get_json(req: RequestBuilder) -> Result<Value, FetchError> {
    let res = req
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    match status {
        200..=299 => serde_json::from_str(&text).map_err(|e| FetchError::Parse(e.to_string())),
        429 => Err(FetchError::RateLimited),
        401 | 403 => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("just a moment") || lower.contains("cf-chl") || lower.contains("cloudflare") {
                Err(FetchError::Blocked)
            } else {
                Err(FetchError::Auth("로그인이 만료되었습니다. 다시 로그인해 주세요.".into()))
            }
        }
        _ => Err(FetchError::Http(status)),
    }
}

fn web_request(client: &Client, path: &str, session_key: &str) -> RequestBuilder {
    client
        .get(format!("{CLAUDE_AI}{path}"))
        .header("Cookie", format!("sessionKey={session_key}"))
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .header("Referer", "https://claude.ai/settings/usage")
}

pub struct Org {
    pub uuid: String,
    pub name: String,
    pub plan: Option<String>,
}

/// 세션으로 접근 가능한 조직 중 채팅(구독) 조직만 돌려준다.
pub async fn list_orgs(client: &Client, session_key: &str) -> Result<Vec<Org>, FetchError> {
    let v = get_json(web_request(client, "/api/organizations", session_key)).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| FetchError::Parse("organizations 응답이 배열이 아님".into()))?;
    let orgs: Vec<Org> = arr
        .iter()
        .filter_map(|o| {
            let caps: Vec<&str> = o["capabilities"]
                .as_array()
                .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
                .unwrap_or_default();
            if !caps.is_empty() && !caps.contains(&"chat") {
                return None;
            }
            Some(Org {
                uuid: o["uuid"].as_str()?.to_string(),
                name: o["name"].as_str().unwrap_or("Claude").to_string(),
                plan: plan_name(o["rate_limit_tier"].as_str(), &caps),
            })
        })
        .collect();
    Ok(orgs)
}

fn plan_name(tier: Option<&str>, caps: &[&str]) -> Option<String> {
    let t = tier.unwrap_or("").to_ascii_lowercase();
    let p = if t.contains("max_20x") {
        "Max 20x"
    } else if t.contains("max_5x") {
        "Max 5x"
    } else if t.contains("max") || caps.contains(&"claude_max") {
        "Max"
    } else if t.contains("pro") || caps.contains(&"claude_pro") {
        "Pro"
    } else if t.contains("team") || t.contains("enterprise") {
        "Team"
    } else {
        return None;
    };
    Some(p.to_string())
}

pub async fn web_usage(client: &Client, session_key: &str, org_uuid: &str) -> Result<Value, FetchError> {
    get_json(web_request(client, &format!("/api/organizations/{org_uuid}/usage"), session_key)).await
}

/// Claude Code 인증 파일의 토큰으로 조회한다. 토큰 갱신은 Claude Code에 맡기고 파일은 읽기만 한다.
pub async fn code_usage(client: &Client, credentials_path: &str) -> Result<(Value, Option<String>), FetchError> {
    let raw = std::fs::read_to_string(credentials_path)
        .map_err(|_| FetchError::Auth(format!("인증 파일을 읽을 수 없습니다: {credentials_path}")))?;
    let json: Value = serde_json::from_str(&raw).map_err(|e| FetchError::Parse(e.to_string()))?;
    let oauth = &json["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .ok_or_else(|| FetchError::Auth("인증 파일에 accessToken이 없습니다.".into()))?;
    if let Some(exp) = oauth["expiresAt"].as_i64() {
        if exp < Utc::now().timestamp_millis() {
            return Err(FetchError::Auth(
                "Claude Code 토큰이 만료되었습니다. Claude Code를 한 번 실행하면 갱신됩니다.".into(),
            ));
        }
    }
    let plan = plan_name(
        oauth["rateLimitTier"].as_str().or(oauth["subscriptionType"].as_str()),
        &[],
    );
    let v = get_json(
        client
            .get(OAUTH_USAGE_URL)
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("Accept", "application/json"),
    )
    .await?;
    Ok((v, plan))
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc));
    }
    let n = v.as_f64()?;
    // 초 또는 밀리초 단위 에포크
    let secs = if n > 1e12 { n / 1000.0 } else { n };
    Utc.timestamp_opt(secs as i64, 0).single()
}

fn model_label(slug: &str) -> String {
    match slug {
        "opus" => "Opus".into(),
        "sonnet" => "Sonnet".into(),
        "oauth_apps" => "OAuth 앱".into(),
        _ => {
            let s = slug.replace('_', " ");
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

fn window_for(key: &str) -> Option<i64> {
    match key {
        "session" => Some(SESSION_SECS),
        "weekly" => Some(WEEK_SECS),
        k if k.starts_with("model:") => Some(WEEK_SECS),
        _ => None,
    }
}

/// 응답을 표시용 한도 목록으로 바꾼다. 두 번째 값은 추가 사용량(extra usage) 활성 여부.
pub fn parse_usage(v: &Value) -> (Vec<Limit>, bool) {
    let mut out: Vec<Limit> = Vec::new();

    if let Some(limits) = v["limits"].as_array().filter(|a| !a.is_empty()) {
        for l in limits {
            let percent = l["percent"].as_f64().or(l["utilization"].as_f64()).unwrap_or(0.0);
            let resets_at = parse_time(&l["resets_at"]);
            let (key, label) = match l["kind"].as_str().unwrap_or("") {
                "session" => ("session".to_string(), "5시간 세션".to_string()),
                "weekly_all" => ("weekly".to_string(), "주간 (전체 모델)".to_string()),
                "weekly_scoped" => {
                    let name = l["scope"]["model"]["display_name"].as_str().unwrap_or("모델별");
                    (format!("model:{name}"), format!("주간 · {name}"))
                }
                other if !other.is_empty() => (format!("other:{other}"), other.replace('_', " ")),
                _ => continue,
            };
            out.push(Limit { window_secs: window_for(&key), key, label, percent, resets_at, ..Default::default() });
        }
    }

    let has = |out: &Vec<Limit>, k: &str| out.iter().any(|l| l.key == k);
    let has_models = out.iter().any(|l| l.key.starts_with("model:"));

    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !val.is_object() || val.get("utilization").is_none() {
                continue;
            }
            let percent = val["utilization"].as_f64().unwrap_or(0.0);
            let resets_at = parse_time(&val["resets_at"]);
            let (key, label) = match k.as_str() {
                "five_hour" if !has(&out, "session") => ("session".to_string(), "5시간 세션".to_string()),
                "seven_day" if !has(&out, "weekly") => ("weekly".to_string(), "주간 (전체 모델)".to_string()),
                s if s.starts_with("seven_day_") && !has_models => {
                    let name = model_label(&s["seven_day_".len()..]);
                    (format!("model:{name}"), format!("주간 · {name}"))
                }
                _ => continue,
            };
            out.push(Limit { window_secs: window_for(&key), key, label, percent, resets_at, ..Default::default() });
        }
    }

    let order = |k: &str| match k {
        "session" => 0,
        "weekly" => 1,
        k if k.starts_with("model:") => 2,
        _ => 3,
    };
    out.sort_by(|a, b| order(&a.key).cmp(&order(&b.key)).then(a.label.cmp(&b.label)));

    let extra = v["extra_usage"]["is_enabled"].as_bool().unwrap_or(false);
    (out, extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_legacy_format() {
        let v = json!({
            "five_hour": {"utilization": 37.0, "resets_at": "2026-10-16T15:00:00.123+00:00"},
            "seven_day": {"utilization": 12, "resets_at": "2026-10-20T00:00:00Z"},
            "seven_day_opus": {"utilization": 5.5, "resets_at": null},
            "seven_day_sonnet": null,
            "extra_usage": {"is_enabled": true}
        });
        let (l, extra) = parse_usage(&v);
        assert!(extra);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].key, "session");
        assert_eq!(l[0].percent, 37.0);
        assert!(l[0].resets_at.is_some());
        assert_eq!(l[1].key, "weekly");
        assert_eq!(l[1].percent, 12.0);
        assert_eq!(l[2].label, "주간 · Opus");
    }

    #[test]
    fn limits_array_takes_priority() {
        let v = json!({
            "five_hour": {"utilization": 1.0, "resets_at": null},
            "seven_day_sonnet": {"utilization": 9.0, "resets_at": null},
            "limits": [
                {"kind": "session", "percent": 57, "resets_at": "2026-10-16T15:00:00Z"},
                {"kind": "weekly_all", "percent": 20, "resets_at": "2026-10-20T00:00:00Z"},
                {"kind": "weekly_scoped", "percent": 33, "scope": {"model": {"display_name": "Fable"}}}
            ]
        });
        let (l, _) = parse_usage(&v);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].percent, 57.0);
        assert_eq!(l[2].label, "주간 · Fable");
    }

    #[test]
    fn plan_names() {
        assert_eq!(plan_name(Some("default_claude_max_20x"), &[]).as_deref(), Some("Max 20x"));
        assert_eq!(plan_name(None, &["chat", "claude_pro"]).as_deref(), Some("Pro"));
        assert_eq!(plan_name(None, &["chat"]), None);
    }
}
