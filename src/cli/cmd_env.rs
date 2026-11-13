//! `atb env` / `atb extensions`（2026-11-13）。
//!
//! `env` 是**纯 CLI 聚合**：不新增 daemon 路由，依次复用 `/health`、`/tabs`、
//! `/inject`（`action:list`）与 `/capabilities`，任何一块失败只降级成
//! `available:false + error`，不拖垮整条命令。`extensions` 转发到 daemon 的
//! `POST /extensions`，由 `src/server/env.rs` 借扩展的 `listExtensions` 实现。

use super::*;

/// 各子请求的客户端预算。`env` 是诊断命令，宁可单块降级也不要整体卡住。
const ENV_HEALTH_TIMEOUT: f64 = 5.0;
const ENV_TABS_TIMEOUT: f64 = 15.0;
const ENV_INJECT_TIMEOUT: f64 = 20.0;
const ENV_CAPABILITIES_TIMEOUT: f64 = 3.0;

/// 从 `/health.connection.profiles[]` / `/tabs` 推导出的浏览器身份。
///
/// `browser` / `profile` 是**可读名**（label 优先，回落 id），`*_label` / `*_id`
/// 保留原始值，调用方不用再从字符串里猜哪个是 UUID。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EnvIdentity {
    browser_id: Option<String>,
    browser_label: Option<String>,
    profile_id: Option<String>,
    profile_label: Option<String>,
}

impl EnvIdentity {
    fn browser_name(&self) -> Option<String> {
        self.browser_label
            .clone()
            .or_else(|| self.browser_id.clone())
    }

    fn profile_name(&self) -> Option<String> {
        self.profile_label
            .clone()
            .or_else(|| self.profile_id.clone())
    }

    fn has_browser(&self) -> bool {
        self.browser_id.is_some() || self.browser_label.is_some()
    }
}

fn clean_filter(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 选择器匹配：`--browser Chrome` 既要能命中 `browser_id`，也要能命中可读 label。
fn selector_matches(value: Option<&str>, filter: &str) -> bool {
    let Some(value) = value.map(str::trim).filter(|text| !text.is_empty()) else {
        return false;
    };
    value.to_lowercase().contains(&filter.to_lowercase())
}

fn profile_matches(
    profile: &Value,
    browser_filter: Option<&str>,
    profile_filter: Option<&str>,
) -> bool {
    if let Some(filter) = browser_filter {
        let hit = selector_matches(text_field(profile, "browser_id").as_deref(), filter)
            || selector_matches(text_field(profile, "browser_label").as_deref(), filter);
        if !hit {
            return false;
        }
    }
    if let Some(filter) = profile_filter {
        let hit = selector_matches(text_field(profile, "profile_id").as_deref(), filter)
            || selector_matches(text_field(profile, "profile_label").as_deref(), filter);
        if !hit {
            return false;
        }
    }
    true
}

fn matches_default_profile(
    profile: &Value,
    default_browser_id: Option<&str>,
    default_profile_id: Option<&str>,
) -> bool {
    if default_browser_id.is_none() && default_profile_id.is_none() {
        return false;
    }
    if let Some(expected) = default_browser_id {
        if text_field(profile, "browser_id").as_deref() != Some(expected) {
            return false;
        }
    }
    if let Some(expected) = default_profile_id {
        if text_field(profile, "profile_id").as_deref() != Some(expected) {
            return false;
        }
    }
    true
}

fn identity_from_profile(profile: &Value) -> EnvIdentity {
    EnvIdentity {
        browser_id: text_field(profile, "browser_id"),
        browser_label: text_field(profile, "browser_label"),
        profile_id: text_field(profile, "profile_id"),
        profile_label: text_field(profile, "profile_label"),
    }
}

/// 从 `profiles[]` 里挑出「正在操作的浏览器/profile」。
///
/// 优先级：`is_default` → 与 `default_browser_id/default_profile_id` 匹配 →
/// （有过滤器或只有一个候选时）第一个候选 → 只有 id 的默认值。
/// 多个 profile 且没有默认标记时返回空：宁可不猜，也不把 A 浏览器的身份写到 B 上。
fn select_identity(
    profiles: &[Value],
    default_browser_id: Option<&str>,
    default_profile_id: Option<&str>,
    browser_filter: Option<&str>,
    profile_filter: Option<&str>,
) -> EnvIdentity {
    let browser_filter = clean_filter(browser_filter);
    let profile_filter = clean_filter(profile_filter);
    let candidates: Vec<&Value> = profiles
        .iter()
        .filter(|profile| profile_matches(profile, browser_filter, profile_filter))
        .collect();
    let chosen = candidates
        .iter()
        .copied()
        .find(|profile| {
            profile
                .get("is_default")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| {
            candidates.iter().copied().find(|profile| {
                matches_default_profile(profile, default_browser_id, default_profile_id)
            })
        })
        .or_else(|| {
            // 显式过滤器选中了多个 profile、或总共只有一个候选时，取第一个；
            // 否则「多浏览器且无默认」时返回空，由调用方给歧义提示。
            if browser_filter.is_some() || profile_filter.is_some() || candidates.len() == 1 {
                candidates.first().copied()
            } else {
                None
            }
        });
    if let Some(profile) = chosen {
        return identity_from_profile(profile);
    }
    // `/health` 没给 profiles（旧 daemon）时仍有 default_*_id，至少回 id。
    if browser_filter.is_none() && profile_filter.is_none() {
        return EnvIdentity {
            browser_id: default_browser_id.map(str::to_string),
            profile_id: default_profile_id.map(str::to_string),
            ..EnvIdentity::default()
        };
    }
    EnvIdentity::default()
}

/// `/health` 里没有 profiles 时，用 `/tabs` 的条目合成同形状的候选。
fn profiles_from_tabs(tabs: &[Value]) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut profiles = Vec::new();
    for tab in tabs {
        let browser_id = text_field(tab, "browser_id").unwrap_or_default();
        let profile_id = text_field(tab, "profile_id").unwrap_or_default();
        if browser_id.is_empty() && profile_id.is_empty() {
            continue;
        }
        if !seen.insert((browser_id.clone(), profile_id.clone())) {
            continue;
        }
        profiles.push(json!({
            "browser_id": browser_id,
            "browser_label": tab.get("browser_label").cloned().unwrap_or(Value::Null),
            "profile_id": profile_id,
            "profile_label": tab.get("profile_label").cloned().unwrap_or(Value::Null),
            "is_default": false,
        }));
    }
    profiles
}

/// 把默认目标与当前激活标签排到前面，其余保持 `/tabs` 原顺序。
///
/// 只调整展示顺序，不改变 total / hidden 的口径。
fn order_env_tabs(tabs: &[Value], default_session_key: Option<&str>) -> Vec<Value> {
    let mut ordered: Vec<Value> = Vec::with_capacity(tabs.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 1) 默认目标
    if let Some(key) = default_session_key.filter(|key| !key.trim().is_empty()) {
        if let Some(tab) = tabs
            .iter()
            .find(|tab| tab.get("session_key").and_then(Value::as_str) == Some(key))
        {
            push_unique_tab(&mut ordered, &mut seen, tab);
        }
    }
    // 2) 当前激活标签
    for tab in tabs {
        if tab.get("active").and_then(Value::as_bool) == Some(true) {
            push_unique_tab(&mut ordered, &mut seen, tab);
        }
    }
    // 3) 其余保持原顺序
    for tab in tabs {
        push_unique_tab(&mut ordered, &mut seen, tab);
    }
    ordered
}

fn push_unique_tab(
    ordered: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<String>,
    tab: &Value,
) {
    let key = tab
        .get("session_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if seen.insert(key) {
        ordered.push(tab.clone());
    }
}

/// 标签页摘要：按已排序的 tabs 取前 `limit` 个，返回 `(total, shown, hidden)`。
/// `total` 用 daemon 的 `tabs_count`（可能与本次返回的数组长度不同）。
fn tab_digest(
    tabs: &[Value],
    limit: usize,
    total: usize,
    default_session_key: Option<&str>,
) -> (usize, Vec<Value>, usize) {
    let shown: Vec<Value> = tabs
        .iter()
        .take(limit)
        .map(|tab| {
            let is_default = default_session_key
                .filter(|key| !key.trim().is_empty())
                .map(|key| tab.get("session_key").and_then(Value::as_str) == Some(key))
                .unwrap_or(false);
            json!({
                "tab_id": tab.get("tab_id").cloned().unwrap_or(Value::Null),
                "title": tab.get("title").cloned().unwrap_or(Value::Null),
                "url": tab.get("url").cloned().unwrap_or(Value::Null),
                "active": tab.get("active").and_then(Value::as_bool).unwrap_or(false),
                "is_default": is_default,
            })
        })
        .collect();
    let hidden = total.saturating_sub(shown.len());
    (total, shown, hidden)
}

/// 默认目标是否落在后台标签页（`active=false`）。
///
/// 默认目标不在当前列表里时返回 false：那通常是「刚关闭 / 快照还没到」，
/// 不应该用一条「在后台」的误导性警告代替它。
fn default_tab_hidden(tabs: &[Value], default_session_key: Option<&str>) -> bool {
    let Some(key) = default_session_key.filter(|key| !key.trim().is_empty()) else {
        return false;
    };
    tabs.iter()
        .find(|tab| tab.get("session_key").and_then(Value::as_str) == Some(key))
        .and_then(|tab| tab.get("active").and_then(Value::as_bool))
        .map(|active| !active)
        .unwrap_or(false)
}

/// `env.warnings` 的固定生成规则（顺序稳定，便于测试与调用方断言）。
fn build_env_warnings(
    persist_count: u64,
    extension_stale: bool,
    default_tab_hidden: bool,
    browser_label_missing: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if persist_count > 0 {
        warnings.push(format!(
            "有 {persist_count} 个持久注入会在页面导航后重放；用 atb inject --list 查看剩余时间"
        ));
    }
    if extension_stale {
        warnings.push("浏览器内扩展版本低于随包版本，界面/命令可能不一致".to_string());
    }
    if default_tab_hidden {
        warnings.push("默认目标标签页在后台；需要可见行为先 atb focus".to_string());
    }
    if browser_label_missing {
        warnings.push("扩展未上报浏览器标识".to_string());
    }
    warnings
}

fn push_hint(hints: &mut Vec<String>, text: impl Into<String>) {
    let text = text.into();
    if !hints.iter().any(|hint| hint == &text) {
        hints.push(text);
    }
}

fn api_error(value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("error_code")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| value.to_string())
}

fn daemon_block(health: Option<&Value>, error: Option<&str>) -> Value {
    match health {
        Some(health) => json!({
            "available": true,
            "running": health.get("running").and_then(Value::as_bool).unwrap_or(true),
            "ready": health.get("ready").and_then(Value::as_bool).unwrap_or(false),
            "version": health.get("version").cloned().unwrap_or(Value::Null),
            "api_protocol": health.get("protocol").cloned().unwrap_or(Value::Null),
            "uptime_sec": health.get("uptime").cloned().unwrap_or(Value::Null),
        }),
        None => json!({
            "available": false,
            "running": false,
            "ready": false,
            "version": Value::Null,
            "api_protocol": Value::Null,
            "error": error.unwrap_or("/health 不可用"),
        }),
    }
}

fn extension_block(health: Option<&Value>, error: Option<&str>) -> Value {
    match health {
        Some(health) => json!({
            "available": true,
            "connected": health
                .pointer("/connection/extension_connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "connected_versions": health
                .pointer("/extension/connected_versions")
                .cloned()
                .unwrap_or_else(|| json!([])),
            "bundled_version": health
                .pointer("/extension/bundled_version")
                .cloned()
                .unwrap_or(Value::Null),
            "stale": health
                .pointer("/extension/stale")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        None => json!({
            "available": false,
            "connected": false,
            "connected_versions": [],
            "bundled_version": Value::Null,
            "stale": false,
            "error": error.unwrap_or("/health 不可用"),
        }),
    }
}

fn injections_block(result: Option<&Value>, error: Option<&str>) -> Value {
    match result {
        Some(result) => {
            let items = result
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let count = result
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(items.len() as u64);
            json!({
                "available": true,
                "persist_count": count,
                "items": items,
                "warnings": result
                    .get("warnings")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            })
        }
        None => json!({
            "available": false,
            "persist_count": 0,
            "items": [],
            "warnings": [],
            "error": error.unwrap_or("持久注入列表不可用"),
        }),
    }
}

/// `/capabilities` 路由不存在（当前 daemon 没有该路由）时，从 `/health` 推导最小能力块。
fn capabilities_from_health(health: Option<&Value>, health_error: Option<&str>) -> Value {
    match health {
        Some(health) => json!({
            "available": true,
            "source": "health",
            "daemon_version": health.get("version").cloned().unwrap_or(Value::Null),
            "api_protocol": health.get("protocol").cloned().unwrap_or(Value::Null),
            "extension_connected": health
                .pointer("/connection/extension_connected")
                .cloned()
                .unwrap_or(Value::Bool(false)),
            "extension_versions": health
                .pointer("/extension/connected_versions")
                .cloned()
                .unwrap_or_else(|| json!([])),
            "compatibility": health.get("compatibility").cloned().unwrap_or(Value::Null),
        }),
        None => json!({
            "available": false,
            "source": "health",
            "error": health_error.unwrap_or("/health 不可用，无法推导能力"),
        }),
    }
}

fn tabs_path(browser: Option<&str>, profile: Option<&str>) -> String {
    let mut query = Vec::new();
    if let Some(browser) = clean_filter(browser) {
        query.push(format!("browser={}", query_escape(browser)));
    }
    if let Some(profile) = clean_filter(profile) {
        query.push(format!("profile={}", query_escape(profile)));
    }
    if query.is_empty() {
        "/tabs".to_string()
    } else {
        format!("/tabs?{}", query.join("&"))
    }
}

/// `ensure_server()` 失败：环境故障不是「命令写错了」，输出 `ok:false` 并由
/// `print_json` 映射为退出码 1（不是 2）。
fn print_daemon_unavailable(command: &str, err: &anyhow::Error) -> Result<()> {
    let code = err
        .downcast_ref::<HttpFailure>()
        .map(|failure| failure.code)
        .unwrap_or("daemon_not_running");
    print_json(json!({
        "ok": false,
        "error_code": code,
        "error": format!("{command}: daemon 不可用：{err}"),
        "hint": "执行 atb restart，或任意页面命令（tabs/open/exec）自动拉起 daemon 后重试",
    }));
    Ok(())
}

pub(crate) fn cmd_env(args: EnvArgs, is_async: bool) -> Result<()> {
    // 分发层已统一拒绝 --async；这里保留第二道闸，避免直接调用时静默忽略。
    reject_async_if_set("env", is_async)?;
    if DRY_RUN.load(Ordering::Relaxed) {
        // 计划模式完全本地：不拉起 daemon、不发请求。
        let planned_tabs_path = tabs_path(args.browser.as_deref(), args.profile.as_deref());
        print_json(json!({
            "ok": true,
            "dry_run": true,
            "command": "env",
            "would": {
                "browser": args.browser,
                "profile": args.profile,
                "tab": args.tab,
                "tab_limit": args.tab_limit,
                "requests": [
                    "GET /health",
                    planned_tabs_path,
                    "POST /inject {action:\"list\", switch_tab_id:null}",
                    "GET /capabilities（daemon 没有该路由时由 /health 推导）",
                ],
                "note": "去掉 --dry-run 才会真正聚合；任一子请求失败只降级为 available:false",
            }
        }));
        return Ok(());
    }
    if let Err(err) = ensure_server() {
        return print_daemon_unavailable("env", &err);
    }

    // 1) /health：daemon / 连接 / 扩展版本。
    let (health, health_error) = match request("GET", "/health", None, ENV_HEALTH_TIMEOUT) {
        Ok(value) => (Some(value), None),
        Err(err) => (None, Some(err.to_string())),
    };

    // 2) /tabs：标签页摘要。`/tabs` 当前没有 `limit` 参数（未知 query 会被拒），
    //    因此在 CLI 侧切片，而不是把 limit 传给 daemon。
    let mut tabs_list: Vec<Value> = Vec::new();
    let mut tabs_total = 0usize;
    let mut tabs_error: Option<String> = None;
    let mut tabs_meta_warning: Option<String> = None;
    match request(
        "GET",
        &tabs_path(args.browser.as_deref(), args.profile.as_deref()),
        None,
        ENV_TABS_TIMEOUT,
    ) {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(false) => {
            tabs_error = Some(api_error(&value));
        }
        Ok(value) => {
            tabs_list = value
                .pointer("/result/metadata/tabs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            tabs_total = value
                .pointer("/result/metadata/tabs_count")
                .and_then(Value::as_u64)
                .map(|count| count as usize)
                .unwrap_or(tabs_list.len());
            tabs_meta_warning = value
                .pointer("/result/metadata/warning")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Err(err) => tabs_error = Some(err.to_string()),
    }

    // 3) /inject action=list：持久注入。
    let mut injections: Option<Value> = None;
    let mut injections_error: Option<String> = None;
    let inject_payload = json!({
        "action": "list",
        "switch_tab_id": args.tab.clone(),
        "browser": args.browser.clone(),
        "profile": args.profile.clone(),
    });
    match request("POST", "/inject", Some(inject_payload), ENV_INJECT_TIMEOUT) {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(false) => {
            injections_error = Some(api_error(&value));
        }
        Ok(value) => injections = value.get("result").cloned(),
        Err(err) => injections_error = Some(err.to_string()),
    }

    // 4) /capabilities 若存在就用 daemon 的；否则由 /health 推导。
    let capabilities = match request("GET", "/capabilities", None, ENV_CAPABILITIES_TIMEOUT) {
        Ok(value) if value.get("ok").and_then(Value::as_bool) != Some(false) => {
            let mut value = value;
            value["available"] = json!(true);
            value["source"] = json!("daemon");
            value
        }
        _ => capabilities_from_health(health.as_ref(), health_error.as_deref()),
    };

    // 浏览器身份：优先 /health.connection.profiles；旧 daemon 没有该字段时用 /tabs 合成。
    let profiles = health
        .as_ref()
        .and_then(|health| health.pointer("/connection/profiles"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let default_browser_id = health
        .as_ref()
        .and_then(|health| health.pointer("/connection/default_browser_id"))
        .and_then(Value::as_str);
    let default_profile_id = health
        .as_ref()
        .and_then(|health| health.pointer("/connection/default_profile_id"))
        .and_then(Value::as_str);
    let mut identity = select_identity(
        &profiles,
        default_browser_id,
        default_profile_id,
        args.browser.as_deref(),
        args.profile.as_deref(),
    );
    if !identity.has_browser() {
        let tab_profiles = profiles_from_tabs(&tabs_list);
        identity = select_identity(
            &tab_profiles,
            default_browser_id,
            default_profile_id,
            args.browser.as_deref(),
            args.profile.as_deref(),
        );
    }

    let extension_stale = health
        .as_ref()
        .and_then(|health| health.pointer("/extension/stale"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let default_session_key = health
        .as_ref()
        .and_then(|health| health.pointer("/connection/default_session_key"))
        .and_then(Value::as_str);
    let persist_count = injections
        .as_ref()
        .and_then(|result| result.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hidden_default = default_tab_hidden(&tabs_list, default_session_key);
    let browser_label_missing = identity.browser_label.is_none();
    let warnings = build_env_warnings(
        persist_count,
        extension_stale,
        hidden_default,
        browser_label_missing,
    );
    // 摘要里先放默认目标与当前激活标签：`tabs` 的原始顺序是 browser→profile→id，
    // 100+ 标签时前 N 个里通常看不到「我正在操作的那个」，env 就失去意义了。
    let ordered_tabs = order_env_tabs(&tabs_list, default_session_key);
    let (_, shown, hidden) = tab_digest(
        &ordered_tabs,
        args.tab_limit,
        tabs_total,
        default_session_key,
    );

    let mut hints: Vec<String> = Vec::new();
    if health_error.is_some() {
        push_hint(
            &mut hints,
            "先运行 atb status 或 atb doctor 确认 daemon/扩展连接，再重试 atb env",
        );
    }
    if let Some(warning) = tabs_meta_warning {
        push_hint(&mut hints, warning);
    }
    if tabs_error.is_some() {
        push_hint(
            &mut hints,
            "标签页列表不可用：稍后重试，或先用 atb tabs --ids",
        );
    }
    if injections_error.is_some() {
        push_hint(
            &mut hints,
            "持久注入列表依赖可用标签页：先 atb open 一个页面，再重试 atb env",
        );
    }
    if extension_stale {
        push_hint(
            &mut hints,
            "打开 chrome://extensions（Edge 为 edge://extensions）重新加载扩展，然后重试 atb env",
        );
    }
    if persist_count > 0 {
        push_hint(&mut hints, "用 atb inject --list 查看持久注入的剩余时间");
    }
    if hidden_default {
        push_hint(
            &mut hints,
            "需要可见行为（截图 / rAF / 懒加载）时先 atb focus",
        );
    }
    if browser_label_missing {
        push_hint(
            &mut hints,
            "浏览器未上报可读标识：用 atb tabs 查看 browser_id/profile_id，或重新加载扩展",
        );
    }
    if !identity.has_browser()
        && (clean_filter(args.browser.as_deref()).is_some()
            || clean_filter(args.profile.as_deref()).is_some())
    {
        push_hint(
            &mut hints,
            "没有匹配 --browser/--profile 的已连接 Profile：先用 atb browsers 查看可用实例",
        );
    }
    if tabs_total > shown.len() && !tabs_list.is_empty() {
        push_hint(
            &mut hints,
            format!(
                "只显示前 {} 个标签页；完整列表用 atb tabs",
                args.tab_limit.min(tabs_total)
            ),
        );
    }
    let mut tabs_block = json!({
        "available": tabs_error.is_none(),
        "total": tabs_total,
        "shown": shown,
        "hidden": hidden,
    });
    if let Some(error) = &tabs_error {
        tabs_block["error"] = json!(error);
    }

    let mut value = json!({
        "ok": true,
        "browser": identity.browser_name().map(Value::String).unwrap_or(Value::Null),
        "browser_label": identity.browser_label.clone().map(Value::String).unwrap_or(Value::Null),
        "browser_id": identity.browser_id.clone().map(Value::String).unwrap_or(Value::Null),
        "profile": identity.profile_name().map(Value::String).unwrap_or(Value::Null),
        "profile_label": identity.profile_label.clone().map(Value::String).unwrap_or(Value::Null),
        "profile_id": identity.profile_id.clone().map(Value::String).unwrap_or(Value::Null),
        "daemon": daemon_block(health.as_ref(), health_error.as_deref()),
        "extension": extension_block(health.as_ref(), health_error.as_deref()),
        "tabs": tabs_block,
        "injections": injections_block(injections.as_ref(), injections_error.as_deref()),
        "capabilities": capabilities,
        "warnings": warnings,
        "hints": hints,
    });
    // 显式给出默认目标（若 /health 有）：调用方不用再解析 default_session_key。
    // `visible` 只在标签确实出现在 /tabs 时给布尔值，列表不可用时是 null，
    // 不拿「没查过」冒充「可见」。
    let default_tab_id = default_session_key.map(|key| key.rsplit(':').next().unwrap_or_default());
    let default_visible = default_session_key.and_then(|key| {
        tabs_list
            .iter()
            .find(|tab| tab.get("session_key").and_then(Value::as_str) == Some(key))
            .and_then(|tab| tab.get("active").and_then(Value::as_bool))
    });
    value["default_target"] = json!({
        "session_key": default_session_key,
        "tab_id": default_tab_id,
        "visible": default_visible,
    });
    print_json(value);
    Ok(())
}

pub(crate) fn cmd_extensions(args: ExtensionsArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("extensions", is_async)?;
    // 非有限值（inf / NaN）不能进 JSON：serde_json 会写成 null，daemon 反序列化失败。
    let timeout = if args.timeout.is_finite() {
        args.timeout
    } else {
        15.0
    };
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "dry_run": true,
            "command": "extensions",
            "would": {
                "method": "POST",
                "path": "/extensions",
                "browser": args.browser,
                "profile": args.profile,
                "include_disabled": args.include_disabled,
                "timeout": timeout,
                "note": "去掉 --dry-run 才会请求 daemon；需要先在扩展设置页授权 management",
            }
        }));
        return Ok(());
    }
    if let Err(err) = ensure_server() {
        return print_daemon_unavailable("extensions", &err);
    }
    let payload = json!({
        "browser": args.browser,
        "profile": args.profile,
        "include_disabled": args.include_disabled,
        "timeout": timeout,
    });
    // 回包里的 ok:false / error_code 由 print_json 决定退出码：
    // permission_required / extension_outdated 都是「执行失败」→ 1。
    let value = request("POST", "/extensions", Some(payload), http_budget(timeout))?;
    // CLI 升级后旧 daemon 仍在跑（协议号没变，ensure_server 不会拦）：路由是新增的，
    // 旧二进制回 404 纯文本。这属于「daemon 需要重启」，不是「动作结果未知」。
    let route_missing = value
        .get("error_code")
        .and_then(Value::as_str)
        .map(|code| code == "invalid_response")
        .unwrap_or(false)
        && value
            .get("error")
            .and_then(Value::as_str)
            .map(|error| error.contains("404"))
            .unwrap_or(false);
    if route_missing {
        print_json(json!({
            "ok": false,
            "error_code": "daemon_outdated",
            "error": "当前 daemon 不认识 POST /extensions（HTTP 404）：常驻进程还是旧二进制",
            "hint": "运行 atb restart 重启 daemon 加载新路由，再重试 atb extensions",
        }));
        return Ok(());
    }
    print_json(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(browser_id: &str, label: &str, profile_id: &str, profile_label: &str) -> Value {
        json!({
            "browser_id": browser_id,
            "browser_label": label,
            "profile_id": profile_id,
            "profile_label": profile_label,
            "is_default": false,
        })
    }

    #[test]
    fn identity_prefers_health_default_flag() {
        let mut chrome = profile("b1", "Chrome · Windows", "p1", "默认");
        let edge = profile("b2", "Edge · Windows", "p2", "工作");
        chrome["is_default"] = json!(true);
        let identity = select_identity(&[chrome, edge], Some("b2"), Some("p2"), None, None);
        assert_eq!(identity.browser_id.as_deref(), Some("b1"));
        assert_eq!(identity.browser_label.as_deref(), Some("Chrome · Windows"));
        assert_eq!(identity.profile_id.as_deref(), Some("p1"));
        assert_eq!(identity.profile_label.as_deref(), Some("默认"));
    }

    #[test]
    fn identity_falls_back_to_default_ids() {
        let identity = select_identity(&[], Some("b9"), Some("p9"), None, None);
        assert_eq!(identity.browser_id.as_deref(), Some("b9"));
        assert_eq!(identity.profile_id.as_deref(), Some("p9"));
        assert!(identity.browser_label.is_none());
        assert_eq!(identity.browser_name().as_deref(), Some("b9"));
        assert_eq!(identity.profile_name().as_deref(), Some("p9"));
    }

    #[test]
    fn identity_respects_explicit_filters() {
        let chrome = profile("b1", "Chrome · Windows", "p1", "默认");
        let edge = profile("b2", "Edge · Windows", "p2", "工作");
        let identity = select_identity(&[chrome, edge], None, None, Some("edge"), None);
        assert_eq!(identity.browser_id.as_deref(), Some("b2"));
        assert_eq!(identity.profile_label.as_deref(), Some("工作"));
    }

    #[test]
    fn identity_is_empty_when_multiple_profiles_have_no_default() {
        let chrome = profile("b1", "Chrome", "p1", "默认");
        let edge = profile("b2", "Edge", "p2", "工作");
        let identity = select_identity(&[chrome, edge], None, None, None, None);
        assert!(!identity.has_browser(), "多 profile 无默认时不应乱猜");
    }

    #[test]
    fn warnings_follow_documented_order() {
        let warnings = build_env_warnings(2, true, true, true);
        assert_eq!(warnings.len(), 4);
        assert!(warnings[0].starts_with("有 2 个持久注入"));
        assert!(warnings[1].contains("扩展版本低于随包版本"));
        assert!(warnings[2].contains("默认目标标签页在后台"));
        assert_eq!(warnings[3], "扩展未上报浏览器标识");
        assert!(build_env_warnings(0, false, false, false).is_empty());
    }

    #[test]
    fn tab_digest_hides_overflow() {
        let tabs = vec![
            json!({"tab_id":"1","title":"a","url":"https://a","active":false}),
            json!({"tab_id":"2","title":"b","url":"https://b","active":true}),
            json!({"tab_id":"3","title":"c","url":"https://c","active":false}),
        ];
        let (total, shown, hidden) = tab_digest(&tabs, 2, 3, None);
        assert_eq!(total, 3);
        assert_eq!(shown.len(), 2);
        assert_eq!(hidden, 1);
        assert_eq!(shown[1]["tab_id"], "2");
        assert_eq!(shown[1]["active"], true);
    }

    #[test]
    fn default_tab_hidden_only_when_present_and_inactive() {
        let tabs = vec![
            json!({"session_key":"b:p:1","active":false}),
            json!({"session_key":"b:p:2","active":true}),
        ];
        assert!(default_tab_hidden(&tabs, Some("b:p:1")));
        assert!(!default_tab_hidden(&tabs, Some("b:p:2")));
        // 不在列表里（刚关闭 / 快照未到）不算「在后台」。
        assert!(!default_tab_hidden(&tabs, Some("b:p:9")));
        assert!(!default_tab_hidden(&tabs, None));
    }

    #[test]
    fn profiles_from_tabs_deduplicates() {
        let tabs = vec![
            json!({"browser_id":"b1","browser_label":"Chrome","profile_id":"p1","profile_label":"默认"}),
            json!({"browser_id":"b1","browser_label":"Chrome","profile_id":"p1","profile_label":"默认"}),
            json!({"browser_id":"b2","browser_label":"Edge","profile_id":"p2","profile_label":"工作"}),
        ];
        let profiles = profiles_from_tabs(&tabs);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0]["browser_id"], "b1");
        assert_eq!(profiles[1]["browser_id"], "b2");
    }

    #[test]
    fn tabs_path_only_escapes_present_filters() {
        assert_eq!(tabs_path(None, None), "/tabs");
        assert_eq!(
            tabs_path(Some("Chrome 156"), Some("默认")),
            "/tabs?browser=Chrome%20156&profile=%E9%BB%98%E8%AE%A4"
        );
    }
}
