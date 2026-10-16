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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub source: Source,
    #[serde(default)]
    pub plan: Option<String>,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_secs: 180,
            warn_percent: 80.0,
            crit_percent: 95.0,
            always_on_top: true,
            notifications: true,
            notify_reset: true,
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

fn entry(account_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, account_id).map_err(|e| e.to_string())
}

pub fn set_secret(account_id: &str, secret: &str) -> Result<(), String> {
    entry(account_id)?
        .set_password(secret)
        .map_err(|e| format!("보안 저장소에 저장하지 못했습니다: {e}"))
}

pub fn get_secret(account_id: &str) -> Result<String, String> {
    entry(account_id)?
        .get_password()
        .map_err(|_| "저장된 로그인 정보가 없습니다. 다시 로그인해 주세요.".to_string())
}

pub fn delete_secret(account_id: &str) {
    if let Ok(e) = entry(account_id) {
        let _ = e.delete_credential();
    }
}
