//! 앱 자동 업데이트.
//!
//! GitHub 릴리스에 올라간 `latest.json`을 확인하고, 새 버전이 있으면 서명을 검증한 뒤 설치하고 재시작한다.
//! 릴리스는 `.github/workflows/release.yml`이 태그(`v*`)를 받을 때 만든다.

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
}

#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// 새 버전이 있으면 정보를 돌려준다. 없으면 null.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("업데이트를 확인하지 못했습니다: {e}"))?;
    Ok(update.map(|u| UpdateInfo {
        version: u.version,
        current: u.current_version,
        notes: u.body,
    }))
}

/// 새 버전을 내려받아 설치하고 앱을 다시 시작한다. 진행률은 `update-progress` 이벤트로 알린다.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("업데이트를 확인하지 못했습니다: {e}"))?
        .ok_or("설치할 새 버전이 없습니다.")?;

    let progress_app = app.clone();
    let mut received = 0usize;
    update
        .download_and_install(
            move |chunk, total| {
                received += chunk;
                let _ = progress_app.emit("update-progress", json!({ "received": received, "total": total }));
            },
            || {},
        )
        .await
        .map_err(|e| format!("업데이트를 설치하지 못했습니다: {e}"))?;
    app.restart();
}
