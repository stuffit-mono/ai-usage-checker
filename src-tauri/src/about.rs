//! 앱 정보(개발자, 연락처) 링크.
//!
//! 화면에서 임의 주소를 열 수 없도록, 미리 정한 링크만 기본 앱(메일·브라우저)으로 연다.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

pub const DEVELOPER_EMAIL: &str = "lonelydesigners@gmail.com";
pub const REPO_URL: &str = "https://github.com/stuffit-mono/ai-usage-checker";

#[tauri::command]
pub fn open_link(app: AppHandle, kind: String) -> Result<(), String> {
    let url = match kind.as_str() {
        "email" => format!("mailto:{DEVELOPER_EMAIL}?subject=AI%20Usage%20Checker"),
        "repo" => REPO_URL.to_string(),
        _ => return Err("알 수 없는 링크입니다.".into()),
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("링크를 열지 못했습니다: {e}"))
}
