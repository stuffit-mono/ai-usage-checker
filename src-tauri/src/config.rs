//! 계정 목록과 설정(config.json), 비밀값(OS 보안 저장소) 관리.

use serde::{Deserialize, Serialize};
use std::path::Path;

const KEYRING_SERVICE: &str = "ai-usage-checker";

/// 사용량을 어디서 가져오는지.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// claude.ai 로그인 세션(sessionKey 쿠키). 비밀값은 OS 보안 저장소에 둔다.
    ClaudeWeb { org_uuid: String },
    /// Claude Code CLI가 저장한 OAuth 인증 파일. 파일은 읽기만 한다.
    ClaudeCode { credentials_path: String },
    /// chatgpt.com 로그인 세션으로 Codex 사용 한도를 본다. 토큰과 쿠키는 OS 보안 저장소에 둔다.
    ChatgptWeb { account_id: String },
    /// Anthropic Admin API 키로 이번 달 API 사용액을 본다.
    AnthropicAdmin,
    /// OpenAI Admin API 키로 이번 달 API 사용액을 본다.
    OpenaiAdmin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub source: Source,
    #[serde(default)]
    pub plan: Option<String>,
    /// API 계정의 월 예산(USD). 있으면 예산 대비 비율과 알림을 보여준다.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// API 계정의 남은 크레딧 기준값. 이후 사용액을 빼서 남은 크레딧을 계산한다.
    #[serde(default)]
    pub credit: Option<Credit>,
}

/// 사용자가 입력한 남은 크레딧과 그 시점의 기준 정보.
/// 비용 API는 하루 단위로만 집계되므로, 입력한 날(UTC) 0시부터 입력 시각까지 이미 쓴 금액을
/// `baseline_spent`로 기억해 두고 이후 계산에서 빼서 이중으로 차감하지 않는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credit {
    pub balance: f64,
    pub set_at: chrono::DateTime<chrono::Utc>,
    pub baseline_spent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub interval_secs: u64,
    pub warn_percent: f64,
    pub crit_percent: f64,
    pub always_on_top: bool,
    pub notifications: bool,
    pub notify_reset: bool,
    /// 트레이 아이콘에 가장 높은 사용률을 숫자로 표시
    pub tray_percent: bool,
    /// 미니 모드: 계정당 한 줄로 줄여 보여준다
    pub compact: bool,
    /// 위젯 열기/숨기기 전역 단축키 (빈 문자열이면 사용 안 함)
    pub shortcut: String,
    /// 로그인 시 자동 실행으로 켜졌을 때 위젯을 띄우지 않고 트레이 아이콘만 표시
    pub start_hidden: bool,
}

pub const DEFAULT_SHORTCUT: &str = "CommandOrControl+Shift+U";

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_secs: 180,
            warn_percent: 80.0,
            crit_percent: 95.0,
            always_on_top: true,
            notifications: true,
            notify_reset: true,
            tray_percent: true,
            compact: false,
            shortcut: DEFAULT_SHORTCUT.to_string(),
            start_hidden: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub settings: Settings,
}

impl Config {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| e.to_string())
}

/// Windows 자격 증명 관리자는 항목 하나에 2560바이트까지만 저장된다.
/// ChatGPT 토큰처럼 긴 값은 여러 항목(`id#1`, `id#2`, …)으로 나누고, 기본 항목에는 `chunks:<개수>`를 둔다.
const CHUNK: usize = 1000;
const CHUNK_PREFIX: &str = "chunks:";

fn chunk_name(account_id: &str, i: usize) -> String {
    format!("{account_id}#{i}")
}

fn set_raw(name: &str, value: &str) -> Result<(), String> {
    entry(name)?
        .set_password(value)
        .map_err(|e| format!("보안 저장소에 저장하지 못했습니다: {e}"))
}

pub fn set_secret(account_id: &str, secret: &str) -> Result<(), String> {
    delete_secret(account_id);
    if secret.len() <= CHUNK {
        return set_raw(account_id, secret);
    }
    let chars: Vec<char> = secret.chars().collect();
    let parts: Vec<String> = chars.chunks(CHUNK).map(|c| c.iter().collect()).collect();
    for (i, part) in parts.iter().enumerate() {
        set_raw(&chunk_name(account_id, i + 1), part)?;
    }
    set_raw(account_id, &format!("{CHUNK_PREFIX}{}", parts.len()))
}

pub fn get_secret(account_id: &str) -> Result<String, String> {
    let missing = || "저장된 로그인 정보가 없습니다. 다시 로그인해 주세요.".to_string();
    let head = entry(account_id)?.get_password().map_err(|_| missing())?;
    let Some(n) = head.strip_prefix(CHUNK_PREFIX).and_then(|n| n.parse::<usize>().ok()) else {
        return Ok(head);
    };
    let mut out = String::new();
    for i in 1..=n {
        out.push_str(&entry(&chunk_name(account_id, i))?.get_password().map_err(|_| missing())?);
    }
    Ok(out)
}

pub fn delete_secret(account_id: &str) {
    let Ok(e) = entry(account_id) else { return };
    if let Ok(head) = e.get_password() {
        if let Some(n) = head.strip_prefix(CHUNK_PREFIX).and_then(|n| n.parse::<usize>().ok()) {
            for i in 1..=n {
                if let Ok(c) = entry(&chunk_name(account_id, i)) {
                    let _ = c.delete_credential();
                }
            }
        }
    }
    let _ = e.delete_credential();
}
