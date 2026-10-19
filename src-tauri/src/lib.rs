mod about;
mod billing;
mod chatgpt;
mod claude;
mod config;
mod poller;
mod tray;
mod tray_icon;
mod update;

use config::{Account, Config, Settings, Source};
use poller::AccountStatus;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

const LOGIN_WINDOW: &str = "claude-login";
const CHATGPT_LOGIN_WINDOW: &str = "chatgpt-login";
/// 로그인 창 라벨에 공통으로 들어가는 문자열 (창 위치 저장 제외, 일괄 닫기에 쓴다)
const LOGIN_MARK: &str = "-login";
/// ChatGPT 로그인 창이 세션 정보를 넘길 때 쓰는 가짜 주소. 이동은 앱이 가로채서 취소한다.
const SENTINEL_HOST: &str = "aiusage.invalid";

pub struct AppState {
    pub config: Mutex<Config>,
    pub runtime: Mutex<poller::Runtime>,
    pub wake: tokio::sync::Notify,
    pub client: reqwest::Client,
    pub config_path: PathBuf,
    pub history_path: PathBuf,
}

#[derive(Clone, Serialize)]
struct Snapshot {
    accounts: Vec<Account>,
    statuses: HashMap<String, AccountStatus>,
    settings: Settings,
    autostart: bool,
}

fn snapshot(app: &AppHandle) -> Snapshot {
    let state = app.state::<AppState>();
    let (accounts, settings) = {
        let c = state.config.lock().unwrap();
        (c.accounts.clone(), c.settings.clone())
    };
    let statuses = state.runtime.lock().unwrap().statuses.clone();
    Snapshot {
        accounts,
        statuses,
        settings,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
    }
}

pub fn emit_snapshot(app: &AppHandle) {
    let _ = app.emit("snapshot", snapshot(app));
}

/// 설정을 바꾸고 저장한 뒤 화면에 알린다.
fn mutate_config<R>(app: &AppHandle, f: impl FnOnce(&mut Config) -> R) -> Result<R, String> {
    let state = app.state::<AppState>();
    let r = {
        let mut c = state.config.lock().unwrap();
        let r = f(&mut c);
        c.save(&state.config_path)?;
        r
    };
    emit_snapshot(app);
    Ok(r)
}

pub fn set_always_on_top(app: &AppHandle, on: bool) {
    let _ = mutate_config(app, |c| c.settings.always_on_top = on);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_always_on_top(on);
    }
    if let Some(items) = app.try_state::<tray::TrayItems>() {
        let _ = items.on_top.set_checked(on);
    }
}

/// claude.ai 개인 조직 이름("someone@example.com's Organization")을 보기 좋게 줄인다.
fn org_label(name: &str) -> String {
    name.strip_suffix("'s Organization")
        .or_else(|| name.strip_suffix("’s Organization"))
        .unwrap_or(name)
        .to_string()
}

async fn add_from_session_key(app: &AppHandle, raw: &str) -> Result<usize, String> {
    let key = raw.trim().trim_start_matches("sessionKey=").trim().to_string();
    if key.is_empty() {
        return Err("sessionKey가 비어 있습니다.".into());
    }
    let state = app.state::<AppState>();
    let orgs = claude::list_orgs(&state.client, &key).await.map_err(|e| e.message())?;
    if orgs.is_empty() {
        return Err("이 계정에서 Claude 구독 조직을 찾지 못했습니다.".into());
    }

    let added = mutate_config(app, |c| -> Result<usize, String> {
        let mut added = 0;
        for org in orgs {
            let pos = c.accounts.iter().position(
                |a| matches!(&a.source, Source::ClaudeWeb { org_uuid } if *org_uuid == org.uuid),
            );
            let id = match pos {
                // 이미 있는 계정이면 세션만 교체(재로그인)
                Some(i) => {
                    if org.plan.is_some() {
                        c.accounts[i].plan = org.plan.clone();
                    }
                    c.accounts[i].id.clone()
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    c.accounts.push(Account {
                        id: id.clone(),
                        label: org_label(&org.name),
                        source: Source::ClaudeWeb { org_uuid: org.uuid.clone() },
                        plan: org.plan.clone(),
                        budget_usd: None,
                        credit: None,
                    });
                    added += 1;
                    id
                }
            };
            config::set_secret(&id, &key)?;
        }
        Ok(added)
    })??;

    state.runtime.lock().unwrap().backoff.clear();
    state.wake.notify_one();
    Ok(added)
}

#[tauri::command]
fn get_snapshot(app: AppHandle) -> Snapshot {
    snapshot(&app)
}

/// 로그인 창에서는 필요 없는 claude.ai의 "데스크톱 앱 다운로드" 링크를 숨긴다.
const HIDE_DOWNLOAD_SCRIPT: &str = r#"
document.addEventListener("DOMContentLoaded", () => {
  const style = document.createElement("style");
  style.textContent = 'a[href*="/download"] { display: none !important; }';
  document.head.appendChild(style);
  const re = /download (the )?(desktop )?app|데스크톱 앱/i;
  const hide = () => document.querySelectorAll("a, button").forEach((el) => {
    if (re.test(el.textContent || "")) el.style.display = "none";
  });
  hide();
  new MutationObserver(hide).observe(document.body, { childList: true, subtree: true });
});
"#;

/// 로그인 창이 연 팝업(Google 로그인 등)을 같은 시크릿 세션의 새 창으로 띄운다.
fn open_login_popup(
    app: &AppHandle,
    parent: &str,
    features: tauri::webview::NewWindowFeatures,
) -> tauri::Result<tauri::WebviewWindow> {
    let label = format!("{parent}-popup-{}", uuid::Uuid::new_v4().simple());
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External("about:blank".parse().unwrap()))
        .window_features(features)
        .title("로그인")
        .on_document_title_changed(|w, title| {
            let _ = w.set_title(&title);
        });
    // macOS는 window_features가 opener의 데이터 저장소를 그대로 쓰므로 따로 지정하지 않는다.
    #[cfg(not(target_os = "macos"))]
    let builder = builder.incognito(true);
    builder.build()
}

/// 로그인 창과 그 팝업을 닫는다.
fn close_login_windows(app: &AppHandle, prefix: &str) {
    for (label, w) in app.webview_windows() {
        if label.starts_with(prefix) {
            let _ = w.destroy();
        }
    }
}

/// 시크릿 모드 로그인 창의 공통 설정: 팝업 허용, 다운로드 안내 숨김.
fn login_window<'a>(
    app: &'a AppHandle,
    label: &'a str,
    url: &str,
    title: &str,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    let popup_app = app.clone();
    let parent = label.to_string();
    WebviewWindowBuilder::new(app, label, WebviewUrl::External(url.parse().unwrap()))
        .title(title)
        .inner_size(480.0, 760.0)
        .incognito(true)
        .initialization_script(HIDE_DOWNLOAD_SCRIPT)
        // 소셜 로그인은 팝업 창으로 진행된다. 팝업을 막으면 사이트가 로그인 오류를 띄운다.
        .on_new_window(move |_url, features| match open_login_popup(&popup_app, &parent, features) {
            Ok(window) => tauri::webview::NewWindowResponse::Create { window },
            Err(_) => tauri::webview::NewWindowResponse::Deny,
        })
}

/// 시크릿(InPrivate) 창으로 claude.ai 로그인 화면을 띄우고, sessionKey 쿠키가 생기면 계정을 추가한다.
/// 창마다 쿠키가 분리되므로 여러 계정을 차례로 추가할 수 있다.
#[tauri::command]
async fn start_claude_login(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    login_window(&app, LOGIN_WINDOW, "https://claude.ai/login", "Claude 로그인 — 로그인이 끝나면 자동으로 닫힙니다")
        .build()
        .map_err(|e| e.to_string())?;

    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let target: tauri::Url = "https://claude.ai".parse().unwrap();
        // 최대 30분 대기
        for _ in 0..1800 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(w) = app2.get_webview_window(LOGIN_WINDOW) else {
                close_login_windows(&app2, LOGIN_WINDOW);
                let _ = app2.emit("login-result", json!({ "ok": false, "cancelled": true }));
                return;
            };
            let key = w.cookies_for_url(target.clone()).ok().and_then(|cs| {
                cs.into_iter()
                    .find(|c| c.name() == "sessionKey" && !c.value().is_empty())
                    .map(|c| c.value().to_string())
            });
            if let Some(key) = key {
                close_login_windows(&app2, LOGIN_WINDOW);
                let payload = match add_from_session_key(&app2, &key).await {
                    Ok(n) => json!({ "ok": true, "added": n }),
                    Err(e) => json!({ "ok": false, "error": e }),
                };
                let _ = app2.emit("login-result", payload);
                return;
            }
        }
        close_login_windows(&app2, LOGIN_WINDOW);
        let _ = app2.emit("login-result", json!({ "ok": false, "error": "로그인 대기 시간이 지났습니다." }));
    });
    Ok(())
}

/// 로그인된 chatgpt.com 페이지 안에서 세션 정보를 받아 가짜 주소로 넘긴다.
/// 요청을 브라우저가 직접 보내므로 Cloudflare에 막히지 않는다.
const CHATGPT_SESSION_SCRIPT: &str = r#"
(() => {
  if (location.hostname !== "chatgpt.com") return;
  fetch("/api/auth/session", { credentials: "include" })
    .then((r) => r.json())
    .then((j) => {
      if (j && j.accessToken) location.href = "https://aiusage.invalid/#" + encodeURIComponent(JSON.stringify(j));
    })
    .catch(() => {});
})();
"#;

fn add_chatgpt_account(app: &AppHandle, info: chatgpt::SessionInfo, cookies: String) -> Result<usize, String> {
    let account_id = info.account_id.clone().unwrap_or_default();
    let secret = chatgpt::Secret {
        access_token: info.access_token.clone(),
        cookies,
        account_id: info.account_id.clone(),
    };
    let added = mutate_config(app, |c| -> Result<usize, String> {
        let pos = c.accounts.iter().position(
            |a| matches!(&a.source, Source::ChatgptWeb { account_id: id } if *id == account_id),
        );
        let (id, added) = match pos {
            Some(i) => {
                if info.plan.is_some() {
                    c.accounts[i].plan = info.plan.clone();
                }
                (c.accounts[i].id.clone(), 0)
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                c.accounts.push(Account {
                    id: id.clone(),
                    label: info.email.clone().unwrap_or_else(|| "ChatGPT".into()),
                    source: Source::ChatgptWeb { account_id: account_id.clone() },
                    plan: info.plan.clone(),
                    budget_usd: None,
                    credit: None,
                });
                (id, 1)
            }
        };
        config::set_secret(&id, &secret.to_json())?;
        Ok(added)
    })??;
    let state = app.state::<AppState>();
    state.runtime.lock().unwrap().backoff.clear();
    state.wake.notify_one();
    Ok(added)
}

/// 시크릿 창으로 chatgpt.com에 로그인하게 하고, 로그인되면 액세스 토큰과 쿠키를 받아 계정을 추가한다.
#[tauri::command]
async fn start_chatgpt_login(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(CHATGPT_LOGIN_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let nav_slot = slot.clone();
    login_window(&app, CHATGPT_LOGIN_WINDOW, "https://chatgpt.com/auth/login", "ChatGPT 로그인 — 로그인이 끝나면 자동으로 닫힙니다")
        .on_navigation(move |url| {
            if url.host_str() != Some(SENTINEL_HOST) {
                return true;
            }
            if let Some(frag) = url.fragment() {
                *nav_slot.lock().unwrap() = Some(frag.to_string());
            }
            false
        })
        .build()
        .map_err(|e| e.to_string())?;

    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let site: tauri::Url = "https://chatgpt.com".parse().unwrap();
        for tick in 0..1800u32 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(w) = app2.get_webview_window(CHATGPT_LOGIN_WINDOW) else {
                close_login_windows(&app2, CHATGPT_LOGIN_WINDOW);
                let _ = app2.emit("login-result", json!({ "ok": false, "cancelled": true }));
                return;
            };
            let captured = slot.lock().unwrap().take();
            if let Some(frag) = captured {
                let info = percent_encoding::percent_decode_str(&frag)
                    .decode_utf8()
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| chatgpt::parse_session(&v));
                let Some(info) = info else { continue };
                let cookies = w
                    .cookies_for_url(site.clone())
                    .map(|cs| cs.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; "))
                    .unwrap_or_default();
                close_login_windows(&app2, CHATGPT_LOGIN_WINDOW);
                let payload = match add_chatgpt_account(&app2, info, cookies) {
                    Ok(n) => json!({ "ok": true, "added": n }),
                    Err(e) => json!({ "ok": false, "error": e }),
                };
                let _ = app2.emit("login-result", payload);
                return;
            }
            // 세션 쿠키가 생겼으면(로그인 완료) 2초마다 세션 정보를 요청해 본다.
            if tick % 2 == 0 {
                let logged_in = w
                    .cookies_for_url(site.clone())
                    .map(|cs| cs.iter().any(|c| c.name().starts_with(chatgpt::SESSION_COOKIE)))
                    .unwrap_or(false);
                if logged_in {
                    let _ = w.eval(CHATGPT_SESSION_SCRIPT);
                }
            }
        }
        close_login_windows(&app2, CHATGPT_LOGIN_WINDOW);
        let _ = app2.emit("login-result", json!({ "ok": false, "error": "로그인 대기 시간이 지났습니다." }));
    });
    Ok(())
}

/// Admin API 키를 확인(이번 달 사용액 조회)한 뒤 API 계정으로 추가한다.
/// 남은 크레딧의 기준값을 만든다. 입력한 날(UTC) 0시부터 지금까지 이미 쓴 금액을 함께 기억한다.
async fn make_credit(
    client: &reqwest::Client,
    provider: billing::Provider,
    key: &str,
    balance: f64,
) -> Result<config::Credit, String> {
    let now = chrono::Utc::now();
    let daily = billing::daily_costs(client, provider, key, now).await.map_err(|e| e.message())?;
    Ok(config::Credit { balance, set_at: now, baseline_spent: billing::sum_since(&daily, now) })
}

fn provider_of(source: &Source) -> Option<billing::Provider> {
    match source {
        Source::AnthropicAdmin => Some(billing::Provider::Anthropic),
        Source::OpenaiAdmin => Some(billing::Provider::Openai),
        _ => None,
    }
}

/// Admin API 키를 확인(이번 달 사용액 조회)한 뒤 API 계정으로 추가한다. 남은 크레딧을 함께 넣을 수 있다.
#[tauri::command]
async fn add_api_key(
    app: AppHandle,
    provider: String,
    key: String,
    label: String,
    budget: Option<f64>,
    credit: Option<f64>,
) -> Result<(), String> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("Admin 키를 입력해 주세요.".into());
    }
    let state = app.state::<AppState>();
    let (source, default_label) = match provider.as_str() {
        "anthropic" => (Source::AnthropicAdmin, "Claude API"),
        "openai" => (Source::OpenaiAdmin, "OpenAI API"),
        _ => return Err("알 수 없는 서비스입니다.".into()),
    };
    let p = provider_of(&source).unwrap();
    // 키 확인을 겸해 이번 달 사용액을 한 번 조회한다
    billing::daily_costs(&state.client, p, &key, billing::month_start(chrono::Utc::now()))
        .await
        .map_err(|e| e.message())?;
    let credit = match credit.filter(|c| *c > 0.0) {
        Some(b) => Some(make_credit(&state.client, p, &key, b).await?),
        None => None,
    };
    let id = uuid::Uuid::new_v4().to_string();
    config::set_secret(&id, &key)?;
    let label = if label.trim().is_empty() { default_label.to_string() } else { label.trim().to_string() };
    mutate_config(&app, |c| {
        c.accounts.push(Account {
            id,
            label,
            source,
            plan: None,
            budget_usd: budget.filter(|b| *b > 0.0),
            credit,
        })
    })?;
    state.wake.notify_one();
    Ok(())
}

/// API 계정의 남은 크레딧을 입력(또는 비움)한다. 충전했을 때 새 잔액을 다시 넣으면 된다.
#[tauri::command]
async fn set_credit(app: AppHandle, id: String, balance: Option<f64>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let source = state
        .config
        .lock()
        .unwrap()
        .accounts
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.source.clone())
        .ok_or("계정을 찾지 못했습니다.")?;
    let provider = provider_of(&source).ok_or("API 계정에서만 크레딧을 설정할 수 있습니다.")?;
    let credit = match balance.filter(|b| *b > 0.0) {
        Some(b) => {
            let key = config::get_secret(&id)?;
            Some(make_credit(&state.client, provider, &key, b).await?)
        }
        None => None,
    };
    mutate_config(&app, |c| {
        if let Some(a) = c.accounts.iter_mut().find(|a| a.id == id) {
            a.credit = credit;
        }
    })?;
    {
        // 이전 잔액 기준의 소진 속도 기록은 버린다
        let mut rt = state.runtime.lock().unwrap();
        rt.history.remove(&format!("{id}|credit"));
        rt.marks.remove(&format!("{id}|credit"));
    }
    state.wake.notify_one();
    Ok(())
}

#[tauri::command]
fn set_budget(app: AppHandle, id: String, budget: Option<f64>) -> Result<(), String> {
    mutate_config(&app, |c| {
        if let Some(a) = c.accounts.iter_mut().find(|a| a.id == id) {
            a.budget_usd = budget.filter(|b| *b > 0.0);
        }
    })?;
    app.state::<AppState>().wake.notify_one();
    Ok(())
}

#[tauri::command]
async fn add_session_key(app: AppHandle, key: String) -> Result<usize, String> {
    add_from_session_key(&app, &key).await
}

#[tauri::command]
fn default_claude_code_path(app: AppHandle) -> String {
    app.path()
        .home_dir()
        .map(|h| h.join(".claude").join(".credentials.json").to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[tauri::command]
fn add_claude_code(app: AppHandle, path: String, label: String) -> Result<(), String> {
    let path = path.trim().to_string();
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("파일이 없습니다: {path}"));
    }
    let label = if label.trim().is_empty() { "Claude Code".to_string() } else { label.trim().to_string() };
    mutate_config(&app, |c| {
        c.accounts.push(Account {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            source: Source::ClaudeCode { credentials_path: path },
            plan: None,
            budget_usd: None,
            credit: None,
        })
    })?;
    app.state::<AppState>().wake.notify_one();
    Ok(())
}

#[tauri::command]
fn rename_account(app: AppHandle, id: String, label: String) -> Result<(), String> {
    mutate_config(&app, |c| {
        if let Some(a) = c.accounts.iter_mut().find(|a| a.id == id) {
            a.label = label.trim().to_string();
        }
    })?;
    tray::update_tooltip(&app);
    Ok(())
}

#[tauri::command]
fn move_account(app: AppHandle, id: String, delta: i32) -> Result<(), String> {
    mutate_config(&app, |c| {
        if let Some(i) = c.accounts.iter().position(|a| a.id == id) {
            let j = (i as i32 + delta).clamp(0, c.accounts.len() as i32 - 1) as usize;
            c.accounts.swap(i, j);
        }
    })
}

#[tauri::command]
fn remove_account(app: AppHandle, id: String) -> Result<(), String> {
    config::delete_secret(&id);
    {
        let state = app.state::<AppState>();
        let mut rt = state.runtime.lock().unwrap();
        rt.statuses.remove(&id);
        let prefix = format!("{id}|");
        rt.history.retain(|k, _| !k.starts_with(&prefix));
        rt.marks.retain(|k, _| !k.starts_with(&prefix));
    }
    mutate_config(&app, |c| c.accounts.retain(|a| a.id != id))?;
    tray::update_tooltip(&app);
    Ok(())
}

/// 위젯 열기/숨기기 단축키를 (다시) 등록한다. 빈 문자열이면 해제만 한다.
fn apply_shortcut(app: &AppHandle, shortcut: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let s = shortcut.trim();
    if s.is_empty() {
        return Ok(());
    }
    gs.register(s)
        .map_err(|e| format!("단축키 '{s}'를 등록하지 못했습니다. 형식이 맞는지, 다른 프로그램이 쓰고 있지 않은지 확인해 주세요. ({e})"))
}

#[tauri::command]
fn update_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let on_top = settings.always_on_top;
    let mut s = settings;
    s.interval_secs = s.interval_secs.clamp(60, 3600);
    s.warn_percent = s.warn_percent.clamp(10.0, 99.0);
    s.crit_percent = s.crit_percent.clamp(s.warn_percent, 100.0);
    s.shortcut = s.shortcut.trim().to_string();

    let old_shortcut = app.state::<AppState>().config.lock().unwrap().settings.shortcut.clone();
    if s.shortcut != old_shortcut {
        if let Err(e) = apply_shortcut(&app, &s.shortcut) {
            // 새 단축키가 안 되면 이전 단축키를 되살리고 나머지 설정만 저장한다
            let _ = apply_shortcut(&app, &old_shortcut);
            s.shortcut = old_shortcut;
            mutate_config(&app, |c| c.settings = s)?;
            return Err(e);
        }
    }
    mutate_config(&app, |c| c.settings = s)?;
    set_always_on_top(&app, on_top);
    // 아이콘 표시 여부나 색 기준이 바뀌었을 수 있다
    tray::update_tooltip(&app);
    Ok(())
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let r = if enabled { app.autolaunch().enable() } else { app.autolaunch().disable() };
    r.map_err(|e| e.to_string())?;
    if let Some(items) = app.try_state::<tray::TrayItems>() {
        let _ = items.autostart.set_checked(enabled);
    }
    emit_snapshot(&app);
    Ok(())
}

/// 위젯 창이 내용보다 길어지지 않게 최대 높이를 내용 높이로 묶는다(너비는 그대로).
/// `resize`가 true면 창 높이도 내용에 맞춘다.
#[tauri::command]
fn fit_height(app: AppHandle, height: f64, resize: bool) -> Result<(), String> {
    let w = app.get_webview_window("main").ok_or("위젯 창을 찾지 못했습니다.")?;
    let scale = w.scale_factor().map_err(|e| e.to_string())?;
    let size = w.inner_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let h = height.clamp(80.0, 1400.0);
    w.set_min_size(Some(tauri::LogicalSize::new(280.0, 80.0_f64.min(h))))
        .map_err(|e| e.to_string())?;
    w.set_max_size(Some(tauri::LogicalSize::new(10_000.0, h)))
        .map_err(|e| e.to_string())?;
    if resize || size.height > h + 1.0 {
        w.set_size(tauri::LogicalSize::new(size.width, h)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn refresh_now(app: AppHandle) {
    let state = app.state::<AppState>();
    state.runtime.lock().unwrap().backoff.clear();
    state.wake.notify_one();
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        tray::toggle_widget(app);
                    }
                })
                .build(),
        )
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::POSITION | tauri_plugin_window_state::StateFlags::SIZE,
                )
                .with_filter(|label| !label.contains(LOGIN_MARK))
                .build(),
        )
        .setup(|app| {
            let config_path = app.path().app_config_dir()?.join("config.json");
            let history_path = app.path().app_data_dir()?.join("history.json");
            let config = Config::load(&config_path);
            let always_on_top = config.settings.always_on_top;
            let shortcut = config.settings.shortcut.clone();
            let runtime = poller::Runtime {
                history: poller::load_history(&history_path),
                ..Default::default()
            };
            app.manage(AppState {
                config: Mutex::new(config),
                runtime: Mutex::new(runtime),
                wake: tokio::sync::Notify::new(),
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(20))
                    .build()?,
                config_path,
                history_path,
            });

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Some(win) = app.get_webview_window("main") {
                win.set_always_on_top(always_on_top)?;
                // 닫기 버튼은 트레이로 숨기기만 한다.
                let w2 = win.clone();
                win.on_window_event(move |e| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        let _ = w2.hide();
                    }
                });
            }

            tray::setup(app.handle())?;
            // 다른 프로그램이 같은 단축키를 쓰고 있으면 등록만 건너뛴다 (설정에서 바꿀 수 있음)
            if let Err(e) = apply_shortcut(app.handle(), &shortcut) {
                eprintln!("{e}");
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move { poller::run(handle).await });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            start_claude_login,
            start_chatgpt_login,
            add_api_key,
            set_budget,
            set_credit,
            add_session_key,
            default_claude_code_path,
            add_claude_code,
            rename_account,
            move_account,
            remove_account,
            update_settings,
            set_autostart,
            fit_height,
            update::app_version,
            about::open_link,
            update::check_update,
            update::install_update,
            refresh_now,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("앱 실행 중 오류");
}

#[cfg(test)]
mod tests {
    use tauri_plugin_global_shortcut::Shortcut;

    /// 설정 화면이 만드는 단축키 문자열을 단축키 라이브러리가 해석할 수 있어야 한다.
    #[test]
    fn shortcut_strings_parse() {
        for s in [
            crate::config::DEFAULT_SHORTCUT,
            "Ctrl+Shift+U",
            "Ctrl+Alt+K",
            "Alt+Shift+1",
            "Super+Shift+F5",
        ] {
            assert!(s.parse::<Shortcut>().is_ok(), "해석 실패: {s}");
        }
        assert!("Ctrl+".parse::<Shortcut>().is_err());
    }
}
