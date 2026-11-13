//! `atb focus` / `atb use` 的 daemon 侧实现（2026-11-13）。
//!
//! - focus：把目标标签页切到前台（`chrome.tabs.update` + 可选窗口聚焦）；
//! - use：把当前 agent 的默认目标固定到指定标签页。
//!
//! 两者共用 `--tab` 的完整语法（tab id / `@N` / `@latest` / `url:` / `title:` /
//! `group:` / `window:` / `current`），由 [`resolve_targets`] 在 daemon 侧解析成
//! 确定的会话后再动手；`focus` 只取第一个目标（切前台没有「批量」语义）。

use super::*;

/// `focus` 的扩展命令名（R2 在 `background.js` 实现）。
const FOCUS_COMMAND: &str = "focusTab";

pub(crate) async fn focus(
    State(state): State<AppState>,
    Json(req): Json<FocusRequest>,
) -> Json<Value> {
    touch(&state).await;
    let raw = req.switch_tab_id.as_deref().unwrap_or("");
    let spec = match TargetSpec::parse(raw) {
        Ok(spec) => spec,
        Err(err) => {
            return Json(json!({
                "ok": false,
                "error": err.message,
                "error_code": err.code,
                "hint": err.hint,
                "example": err.example,
            }));
        }
    };
    // `window:` / `group:` / `current` 依赖新版扩展上报的字段；先做能力检查，
    // 避免把「扩展过旧」误报成「标签不存在」（与 /multi 同一口径）。
    if let Some((feature, min_protocol)) = spec_requires_protocol(&spec) {
        if let Err(error) = ensure_extension_protocol(&state, min_protocol, feature).await {
            return Json(error);
        }
    }
    let resolved = match resolve_targets(
        &state,
        &spec,
        req.browser.as_deref(),
        req.profile.as_deref(),
        // focus 是显式目标操作：不受 agent 的 open 范围限制（用户可能要看别人
        // 打开的标签来排查问题），但仍要求目标真实存在。
        false,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(err) => return Json(err.to_json()),
    };
    let Some(target) = resolved.targets.into_iter().next() else {
        return Json(json!({
            "ok": false,
            "error": "focus: 目标解析结果为空",
            "error_code": "no_target_tabs",
            "hint": "先运行 atb tabs --ids 确认标签页存在，再用 --tab <tab_id> 指定",
        }));
    };

    // windowId 来自会话快照；取不到时让扩展按 tabId 自行推断（payload 里不传）。
    let window_id = {
        let driver = state.driver.read().await;
        driver
            .sessions
            .get(&target.session_key)
            .and_then(|session| session.info.window_id)
    };
    // tab_id 是数字字符串；解析失败也把原字符串交给扩展（扩展侧会 Number()），
    // 这样自定义 id / 旧会话不会在 daemon 侧被误判成「tabId 必填」。
    let tab_id_value = match target.tab_id.trim().parse::<i64>() {
        Ok(value) => json!(value),
        Err(_) => json!(&target.tab_id),
    };
    let mut payload = json!({
        "tabId": tab_id_value,
        "focusWindow": !req.no_window,
    });
    if let Some(window_id) = window_id {
        payload["windowId"] = json!(window_id);
    }

    match extension_control(
        &state,
        &target.session_key,
        FOCUS_COMMAND,
        payload,
        secs(req.timeout),
    )
    .await
    {
        Ok(response) => {
            // 扩展成功回包约定：{ok:true, data:{active:true, tab_id, url, title, window_id}}。
            // 字段以扩展回包为准（切前台后 active/url 可能已变化），缺失时用解析时的快照。
            let data = response.get("data").cloned().unwrap_or(Value::Null);
            let url = data
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or(target.url.as_str())
                .to_string();
            let title = data
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(target.title.as_str())
                .to_string();
            let window_id = data.get("window_id").and_then(Value::as_i64).or(window_id);
            Json(json!({
                "ok": true,
                "result": {
                    "focused": true,
                    "tab_id": target.tab_id,
                    "url": url,
                    "title": title,
                    "window_id": window_id,
                    // 只切标签页（--no-window）时窗口可能仍在后台。
                    "window_focused": !req.no_window,
                },
            }))
        }
        Err(err) => Json(focus_failure(&err)),
    }
}

fn focus_failure(err: &anyhow::Error) -> Value {
    let message = err.to_string();
    let outdated = message.contains("Unknown cmd")
        || message.contains("unknown cmd")
        || message.contains("未知命令");
    if outdated {
        json!({
            "ok": false,
            "error": format!("扩展不支持 {FOCUS_COMMAND}：{message}"),
            "error_code": "extension_outdated",
            "hint": "扩展可能过旧：用本仓库 assets/atb_bridge 覆盖已加载的扩展目录，在 chrome://extensions 重新加载后重试",
        })
    } else {
        json!({
            "ok": false,
            "error": format!("focus 失败：{message}"),
            "error_code": "focus_failed",
            "hint": "先 tabs --ids 确认标签页仍存在；只想切标签页不抢系统前台时加 --no-window",
        })
    }
}

pub(crate) async fn use_target(
    State(state): State<AppState>,
    Json(req): Json<UseRequest>,
) -> Json<Value> {
    touch(&state).await;
    let spec = match TargetSpec::parse(&req.target) {
        Ok(spec) => spec,
        Err(err) => {
            return Json(json!({
                "ok": false,
                "error": err.message,
                "error_code": err.code,
                "hint": err.hint,
                "example": err.example,
            }));
        }
    };
    if let Some((feature, min_protocol)) = spec_requires_protocol(&spec) {
        if let Err(error) = ensure_extension_protocol(&state, min_protocol, feature).await {
            return Json(error);
        }
    }
    let resolved = match resolve_targets(
        &state,
        &spec,
        req.browser.as_deref(),
        req.profile.as_deref(),
        false,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(err) => return Json(err.to_json()),
    };
    let mut targets = resolved.targets;
    if targets.len() > 1 {
        // `use` 的语义是「固定一个默认目标」；命中多个时静默取首个会让下一条
        // 无 --tab 的命令落到一个调用方没预期的标签上，且成功得毫无提示。
        let ids: Vec<String> = targets.iter().map(|target| target.tab_id.clone()).collect();
        return Json(json!({
            "ok": false,
            "error": format!(
                "target_ambiguous: 目标 {:?} 命中 {} 个标签页（{}）",
                req.target,
                ids.len(),
                ids.join(", ")
            ),
            "error_code": "target_ambiguous",
            "hint": "atb use <tab_id> 一次只固定一个；先用 tabs 选准 tab_id 再 use",
            "candidates": ids,
        }));
    }
    let Some(target) = targets.pop() else {
        return Json(json!({
            "ok": false,
            "error": "use: 目标解析结果为空",
            "error_code": "no_target_tabs",
            "hint": "先运行 atb tabs --ids 确认标签页存在，再用 atb use <tab_id> 固定默认目标",
        }));
    };

    let agent = current_agent();
    remember_agent_default(&state, &agent, &target.session_key).await;
    if agent == "default" {
        // 与 `open` / `select_tab` 保持一致：只有 default 桶才同步全局默认目标；
        // 具名 agent 的默认目标只存在自己的 agent_defaults 里，避免串线。
        state.driver.write().await.default_session_key = Some(target.session_key.clone());
    }
    let (browser, profile) = session_labels(&state, &target.session_key).await;
    Json(json!({
        "ok": true,
        "result": {
            "used": true,
            "tab_id": target.tab_id,
            "session_key": target.session_key,
            "url": target.url,
            "title": target.title,
            "browser": browser,
            "profile": profile,
        },
    }))
}

/// 会话的可读 browser/profile 标签；缺失时回落 id（与会话仍在但扩展没上报标签的情形一致）。
async fn session_labels(state: &AppState, session_key: &str) -> (String, String) {
    let driver = state.driver.read().await;
    match driver.sessions.get(session_key) {
        Some(session) => (
            non_empty(session.info.browser_label.as_deref())
                .unwrap_or(&session.browser_id)
                .to_string(),
            non_empty(session.profile_label.as_deref())
                .unwrap_or(&session.profile_id)
                .to_string(),
        ),
        None => (String::new(), String::new()),
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}
