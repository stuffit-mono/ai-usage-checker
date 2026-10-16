mod claude;
mod config;
mod poller;
mod tray;

use config::{Account, Config, Settings, Source};
use poller::AccountStatus;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const LOGIN_WINDOW: &str = "claude-login";

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
    features: tauri::webview::NewWindowFeatures,
) -> tauri::Result<tauri::WebviewWindow> {
    let label = format!("{LOGIN_WINDOW}-popup-{}", uuid::Uuid::new_v4().simple());
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

fn close_login_windows(app: &AppHandle) {
    for (label, w) in app.webview_windows() {
        if label.starts_with(LOGIN_WINDOW) {
            let _ = w.destroy();
        }
    }
}

/// 시크릿(InPrivate) 창으로 claude.ai 로그인 화면을 띄우고, sessionKey 쿠키가 생기면 계정을 추가한다.
/// 창마다 쿠키가 분리되므로 여러 계정을 차례로 추가할 수 있다.
#[tauri::command]
async fn start_claude_login(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url: tauri::Url = "https://claude.ai/login".parse().unwrap();
    let popup_app = app.clone();
    WebviewWindowBuilder::new(&app, LOGIN_WINDOW, WebviewUrl::External(url))
        .title("Claude 로그인 — 로그인이 끝나면 자동으로 닫힙니다")
        .inner_size(480.0, 760.0)
        .incognito(true)
        .initialization_script(HIDE_DOWNLOAD_SCRIPT)
        // "Google로 계속하기"는 팝업 창으로 진행된다. 팝업을 막으면 claude.ai가 로그인 오류를 띄운다.
        .on_new_window(move |_url, features| match open_login_popup(&popup_app, features) {
            Ok(window) => tauri::webview::NewWindowResponse::Create { window },
            Err(_) => tauri::webview::NewWindowResponse::Deny,
        })
        .build()
        .map_err(|e| e.to_string())?;

    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let target: tauri::Url = "https://claude.ai".parse().unwrap();
        // 최대 30분 대기
        for _ in 0..1800 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(w) = app2.get_webview_window(LOGIN_WINDOW) else {
                close_login_windows(&app2);
                let _ = app2.emit("login-result", json!({ "ok": false, "cancelled": true }));
                return;
            };
            let key = w.cookies_for_url(target.clone()).ok().and_then(|cs| {
                cs.into_iter()
                    .find(|c| c.name() == "sessionKey" && !c.value().is_empty())
                    .map(|c| c.value().to_string())
            });
            if let Some(key) = key {
                close_login_windows(&app2);
                let payload = match add_from_session_key(&app2, &key).await {
                    Ok(n) => json!({ "ok": true, "added": n }),
                    Err(e) => json!({ "ok": false, "error": e }),
                };
                let _ = app2.emit("login-result", payload);
                return;
            }
        }
        close_login_windows(&app2);
        let _ = app2.emit("login-result", json!({ "ok": false, "error": "로그인 대기 시간이 지났습니다." }));
    });
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

#[tauri::command]
fn update_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let on_top = settings.always_on_top;
    let mut s = settings;
    s.interval_secs = s.interval_secs.clamp(60, 3600);
    s.warn_percent = s.warn_percent.clamp(10.0, 99.0);
    s.crit_percent = s.crit_percent.clamp(s.warn_percent, 100.0);
    mutate_config(&app, |c| c.settings = s)?;
    set_always_on_top(&app, on_top);
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
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::POSITION | tauri_plugin_window_state::StateFlags::SIZE,
                )
                .with_filter(|label| !label.starts_with(LOGIN_WINDOW))
                .build(),
        )
        .setup(|app| {
            let config_path = app.path().app_config_dir()?.join("config.json");
            let history_path = app.path().app_data_dir()?.join("history.json");
            let config = Config::load(&config_path);
            let always_on_top = config.settings.always_on_top;
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

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move { poller::run(handle).await });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            start_claude_login,
            add_session_key,
            default_claude_code_path,
            add_claude_code,
            rename_account,
            move_account,
            remove_account,
            update_settings,
            set_autostart,
            refresh_now,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("앱 실행 중 오류");
}
