//! `atb extensions` 的 daemon 侧实现（2026-11-13）。
//!
//! 通过扩展的 `listExtensions` 命令读取 `chrome.management.getAll()`；该权限是
//! optional，未授权时返回 `permission_required` 与授权指引。
//!
//! 失败分类有一个**过渡期**问题：旧扩展的命令分发层会把 `{ok:false,error_code}`
//! 压成纯文本 `{type:"error",error}`（`background.js` 的通用回包路径），
//! `error_code` 与 `hint` 不会到达 daemon。因此这里既识别结构化错误码，也按
//! 稳定文案兜底分类，保证 `atb extensions` 的退出码与 hint 不依赖扩展版本。

use super::*;

/// `listExtensions` 的 HTTP 侧超时上限：用户手动点授权弹窗时扩展可能等待较久，
/// 但仍要防止 daemon 无限挂住一条请求。
const MAX_EXTENSIONS_TIMEOUT_SEC: f64 = 300.0;

pub(crate) async fn extensions(
    State(state): State<AppState>,
    Json(req): Json<ExtensionsRequest>,
) -> Json<Value> {
    touch(&state).await;
    // 只借一个可用标签拿扩展连接；命令本身不作用在页面上。
    let session_key = match select_tab(
        &state,
        SessionSelector::new(None, req.browser.clone(), req.profile.clone()),
    )
    .await
    {
        Ok(key) => key,
        Err(err) => return Json(session_error_json(&err)),
    };
    let timeout = req.timeout.clamp(0.1, MAX_EXTENSIONS_TIMEOUT_SEC);
    let timeout = Duration::try_from_secs_f64(timeout).unwrap_or(DEFAULT_EXTENSION_CONTROL_TIMEOUT);
    let payload = json!({ "includeDisabled": req.include_disabled });
    let value =
        match extension_control(&state, &session_key, "listExtensions", payload, timeout).await {
            Ok(value) => value,
            Err(err) => return Json(list_extensions_error_json(&err.to_string())),
        };
    // 兼容两种回包：新扩展直接回内层对象，极旧/第三方扩展可能多包一层 data。
    let payload = if value.get("items").is_some() {
        value.clone()
    } else {
        value.get("data").cloned().unwrap_or_else(|| value.clone())
    };
    let permission_granted = payload
        .get("permission_granted")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !permission_granted {
        return Json(permission_required_json(
            "扩展未获得 management 权限，无法列出已安装扩展",
        ));
    }
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count = payload
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(items.len() as u64);
    let (browser, profile) = session_identity(&state, &session_key).await;
    Json(json!({
        "ok": true,
        "result": {
            "permission_granted": true,
            "count": count,
            "items": items,
            "browser": browser,
            "profile": profile,
            // 权限是 optional：没有授权时不会退回「只列本扩展」，
            // 而是明确失败（permission_required），避免调用方误读成空列表。
            "note": "management 是可选权限；未授权时 atb extensions 返回 permission_required，而不是只列本工具扩展",
        }
    }))
}

/// 当前会话的可读 browser/profile 名称（label 优先，回落 id）。
async fn session_identity(state: &AppState, session_key: &str) -> (Value, Value) {
    let driver = state.driver.read().await;
    let Some(session) = driver.sessions.get(session_key) else {
        return (Value::Null, Value::Null);
    };
    let browser = non_empty(session.info.browser_label.as_deref())
        .unwrap_or(session.browser_id.as_str())
        .to_string();
    let profile = non_empty(session.profile_label.as_deref())
        .unwrap_or(session.profile_id.as_str())
        .to_string();
    (json!(browser), json!(profile))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

fn permission_required_json(error: &str) -> Value {
    json!({
        "ok": false,
        "error_code": "permission_required",
        "error": error,
        "hint": "打开扩展设置页 → 高级权限 → 授权读取扩展列表；然后重试 atb extensions",
    })
}

/// 把扩展错误分成「可操作」的两类；其余保持原样，避免编造错误码。
fn classify_list_extensions_error(message: &str) -> Option<(&'static str, &'static str)> {
    let lower = message.to_lowercase();
    let permission =
        lower.contains("permission") || lower.contains("management") || lower.contains("权限");
    if permission {
        return Some((
            "permission_required",
            "打开扩展设置页 → 高级权限 → 授权读取扩展列表；然后重试 atb extensions",
        ));
    }
    let outdated = lower.contains("unknown cmd")
        || lower.contains("unknown command")
        || lower.contains("unsupported")
        || lower.contains("not implemented")
        || lower.contains("is not a function")
        || message.contains("不支持")
        || message.contains("未实现")
        || message.contains("未知命令");
    if outdated {
        return Some((
            "extension_outdated",
            "浏览器里的扩展还不支持 listExtensions：打开 chrome://extensions（Edge 为 edge://extensions）重新加载随包扩展，再重试 atb extensions",
        ));
    }
    None
}

fn list_extensions_error_json(message: &str) -> Value {
    match classify_list_extensions_error(message) {
        Some(("permission_required", _)) => permission_required_json(message),
        Some((code, hint)) => json!({
            "ok": false,
            "error_code": code,
            "error": message,
            "hint": hint,
        }),
        None => json!({ "ok": false, "error": message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_error_is_classified() {
        // 扩展的通用回包路径会丢掉 error_code，只能按稳定文案兜底。
        let classified =
            classify_list_extensions_error("扩展未获得 management 权限，无法列出已安装扩展")
                .expect("应识别为权限问题");
        assert_eq!(classified.0, "permission_required");
    }

    #[test]
    fn unknown_command_is_extension_outdated() {
        let classified =
            classify_list_extensions_error("Unknown cmd: listExtensions").expect("应识别为旧扩展");
        assert_eq!(classified.0, "extension_outdated");
    }

    #[test]
    fn generic_error_stays_unclassified() {
        assert!(classify_list_extensions_error("socket closed").is_none());
    }
}
