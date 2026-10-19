//! 트레이(Windows) / 메뉴바(macOS) 아이콘과 메뉴.

use crate::config::Source;
use crate::{tray_icon, AppState};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt;

pub struct TrayItems {
    pub on_top: CheckMenuItem<Wry>,
    pub autostart: CheckMenuItem<Wry>,
}

pub fn toggle_widget(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let on_top_checked = app.state::<AppState>().config.lock().unwrap().settings.always_on_top;
    let autostart_checked = app.autolaunch().is_enabled().unwrap_or(false);

    let toggle = MenuItem::with_id(app, "toggle", "위젯 보이기/숨기기", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "지금 새로고침", true, None::<&str>)?;
    let on_top = CheckMenuItem::with_id(app, "on_top", "항상 위에 표시", true, on_top_checked, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", "로그인 시 자동 실행", true, autostart_checked, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &refresh, &sep1, &on_top, &autostart, &sep2, &quit])?;

    app.manage(TrayItems { on_top, autostart });

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().expect("앱 아이콘 없음"))
        .tooltip("AI 사용량")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => toggle_widget(app),
            "refresh" => app.state::<AppState>().wake.notify_one(),
            "on_top" => {
                let checked = app.state::<TrayItems>().on_top.is_checked().unwrap_or(true);
                crate::set_always_on_top(app, checked);
            }
            "autostart" => {
                let items = app.state::<TrayItems>();
                let want = items.autostart.is_checked().unwrap_or(false);
                let _ = if want { app.autolaunch().enable() } else { app.autolaunch().disable() };
                let _ = items.autostart.set_checked(app.autolaunch().is_enabled().unwrap_or(false));
                crate::emit_snapshot(app);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_widget(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 툴팁에 계정별 요약을, 아이콘(macOS는 메뉴바 글자도)에 가장 높은 5시간 사용률을 표시한다.
/// 5시간 한도가 있는 계정이 없으면 가장 높은 주간 사용률을 쓴다.
pub fn update_tooltip(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main") else { return };
    let state = app.state::<AppState>();
    let (accounts, settings) = {
        let c = state.config.lock().unwrap();
        (c.accounts.clone(), c.settings.clone())
    };
    let rt = state.runtime.lock().unwrap();

    // (사용률, 그 계정의 서비스 대표색) 중 사용률이 가장 높은 것
    type Peak = Option<(f64, [u8; 3])>;
    let max = |acc: Peak, v: Option<f64>, svc: [u8; 3]| match (acc, v) {
        (Some(a), Some(b)) if b > a.0 => Some((b, svc)),
        (None, Some(b)) => Some((b, svc)),
        (a, _) => a,
    };
    let mut lines = vec!["AI 사용량".to_string()];
    let mut max_session: Peak = None;
    let mut max_weekly: Peak = None;
    for a in &accounts {
        let Some(st) = rt.statuses.get(&a.id) else { continue };
        let pick = |k: &str| st.limits.iter().find(|l| l.limit.key == k).map(|l| l.limit.percent);
        let session = pick("session");
        let weekly = pick("weekly");
        let svc = service_color(&a.source);
        max_session = max(max_session, session, svc);
        max_weekly = max(max_weekly, weekly, svc);
        let fmt = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.0}%"));
        let warn = if st.ok { "" } else { " ⚠" };
        let name: String = a.label.chars().take(16).collect();
        if session.is_some() || weekly.is_some() {
            lines.push(format!("{name}: 5h {} · 주 {}{warn}", fmt(session), fmt(weekly)));
        } else if let Some(l) = st.limits.first() {
            // API 계정: 남은 크레딧(입력했으면) 또는 이번 달 사용액
            let v = l.limit.display.clone().unwrap_or_else(|| fmt(Some(l.limit.percent)));
            let what = if l.limit.key == "credit" { "잔액" } else { "이번 달" };
            lines.push(format!("{name}: {what} {v}{warn}"));
        }
    }
    // Windows 툴팁은 127자 제한
    let mut text = lines.join("\n");
    if text.chars().count() > 120 {
        text = text.chars().take(119).collect::<String>() + "…";
    }
    let _ = tray.set_tooltip(Some(text));

    let shown = if settings.tray_percent { max_session.or(max_weekly) } else { None };
    let icon = match shown {
        Some((p, svc)) => Image::new_owned(
            tray_icon::render(p, svc, settings.warn_percent, settings.crit_percent),
            tray_icon::SIZE,
            tray_icon::SIZE,
        ),
        None => match app.default_window_icon() {
            Some(i) => i.clone(),
            None => return,
        },
    };
    let _ = tray.set_icon(Some(icon));
    #[cfg(target_os = "macos")]
    let _ = tray.set_title(shown.map(|(s, _)| format!("{s:.0}%")));
}

/// 트레이 아이콘 배경에 쓰는 서비스 대표색
fn service_color(source: &Source) -> [u8; 3] {
    match source {
        Source::ChatgptWeb { .. } | Source::OpenaiAdmin => tray_icon::CHATGPT_RGB,
        _ => tray_icon::CLAUDE_RGB,
    }
}
