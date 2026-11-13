//! 目标身份富化中间件（2026-11-13）。
//!
//! 目标：所有单目标命令的响应顶层都带 `resolved_target` / `browser` / `profile` /
//! `warnings`，调用方不用再猜「这条命令实际打到了哪个标签、哪个浏览器」。
//!
//! 实现方式：中间件用 task-local 放一个可写槽位；`select_tab` 解析成功后写入
//! 目标信息；响应返回前把槽位内容合并进 JSON 顶层。完整的字段定义见本模块
//! 与 README 的「目标身份富化」一节。
//!
//! 约束：
//! - 槽位用 `Arc<std::sync::Mutex<...>>`，保证 handler future 仍是 `Send`；
//! - 只改**顶层**字段，且只在响应是 JSON 对象、已有 `ok` 字段时注入；
//! - `/health` / `/ping` 等运维端点不注入（它们不是「打到了哪个标签」的语义）；
//! - 后台标签页（`active == false`）额外给 `warnings` 与 `hint`：rAF /
//!   IntersectionObserver / 定时器在不可见标签里会被暂停或节流，懒加载与动画可能不触发。

use super::*;
use axum::body::HttpBody as _;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use std::sync::{Arc, Mutex as StdMutex};

/// 中间件愿意读进内存的响应体上限。超过就原样透传，只失去富化能力。
const ENRICH_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// 后台标签页提示的机器码（同时进 `warnings`）。
const HIDDEN_TAB_WARNINGS: [&str; 2] = ["tab_hidden", "timers_throttled"];

/// 后台标签页提示的人读文案（追加到顶层 `hint`）。
const HIDDEN_TAB_HINT: &str =
    "目标标签页在后台（不可见）。确实需要真实可见行为时先执行 atb focus --tab <id> 或 --tab @latest";

/// `select_tab` 解析到的目标身份快照。
///
/// 记录时刻的快照与响应时刻的会话现状可能不同（标签被关闭、标题变化），
/// 中间件优先取响应时刻的最新值，取不到再回落到这里。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTargetInfo {
    pub(crate) session_key: String,
    pub(crate) tab_id: String,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) browser_id: String,
    pub(crate) profile_id: String,
    pub(crate) browser_label: Option<String>,
    pub(crate) profile_label: Option<String>,
    pub(crate) active: bool,
    /// 扩展版本：随快照记录，便于后续诊断/中间件扩展；`resolved_target` 的
    /// 契约字段固定，当前不进入响应 JSON。
    #[allow(dead_code)]
    pub(crate) extension_version: Option<String>,
}

impl ResolvedTargetInfo {
    fn from_session(session: &Session) -> Self {
        Self {
            session_key: session.session_key.clone(),
            tab_id: session.tab_id.clone(),
            url: session.info.url.clone(),
            title: session.info.title.clone(),
            browser_id: session.browser_id.clone(),
            profile_id: session.profile_id.clone(),
            browser_label: session.info.browser_label.clone(),
            profile_label: session.profile_label.clone(),
            active: session.info.active,
            extension_version: session.extension_version.clone(),
        }
    }

    /// 顶层 `browser`：可读标签优先，没有就回落 browser_id。
    fn browser_name(&self) -> &str {
        non_empty(self.browser_label.as_deref()).unwrap_or(&self.browser_id)
    }

    /// 顶层 `profile`：profile label 优先，没有就回落 profile_id。
    fn profile_name(&self) -> &str {
        non_empty(self.profile_label.as_deref()).unwrap_or(&self.profile_id)
    }

    fn resolved_target_json(&self) -> Value {
        json!({
            "tab_id": &self.tab_id,
            "session_key": &self.session_key,
            "url": &self.url,
            "title": &self.title,
            "browser_id": &self.browser_id,
            "profile_id": &self.profile_id,
            "browser": self.browser_name(),
            "profile": self.profile_name(),
            "active": self.active,
            // `visible` 与 `active` 同义：B 文档用 visible，扩展字段用 active。
            "visible": self.active,
        })
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

tokio::task_local! {
    /// 当前请求解析到的目标身份。每个 HTTP 请求一个独立槽位；`/multi` 的并发
    /// 子任务即使调用 `select_tab`，拿不到本请求的 task-local 也不会互相串。
    pub(crate) static TARGET_INFO: Arc<StdMutex<Option<ResolvedTargetInfo>>>;
}

/// 把目标身份写进当前请求的 task-local 槽位（由 `select_tab` 调用）。
///
/// 取不到会话（标签已被移除）时保留旧值不变：调用方刚刚通过 `resolve_session`
/// 选中它，紧接着被关闭属于并发竞态，响应里继续用旧快照比丢掉目标信息更有用。
pub(crate) async fn record_resolved_target(state: &AppState, session_key: &str) {
    let Ok(slot) = TARGET_INFO.try_with(Arc::clone) else {
        // 单测 / 后台任务 / /multi 的 spawn 分支：没有请求级槽位，不算错误。
        return;
    };
    let snapshot = {
        let driver = state.driver.read().await;
        driver
            .sessions
            .get(session_key)
            .map(ResolvedTargetInfo::from_session)
    };
    if let Some(snapshot) = snapshot {
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(snapshot);
        }
    }
}

/// 响应富化中间件：在 `select_tab` 记录后，把 `resolved_target` 等字段并入 JSON。
pub(crate) async fn enrich_target_info(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    // 只富化「单目标页面命令」。这些端点在 handler 里调用 `select_tab`，
    // task-local 记录的就是本次命令真正作用的目标。
    //
    // 不能富化的路径：
    // - `/open`：open 会先为预检选一次旧目标、再创建新标签，槽位里的旧目标
    //   会与响应 `targets[]` 里的新标签冲突，比没有更误导；
    // - `/multi`：每个 result 已经带 tab_id/url/title，槽位没有稳定单值；
    // - `/run` `/batch` `/scrape` `/broadcast`：多步/多目标，单值没有意义；
    // - `/tabs` `/health` `/browsers` 等运维端点：不是「命令打到哪个标签」的语义。
    const ENRICHED_PATHS: &[&str] = &[
        "/scan",
        "/exec",
        "/read",
        "/find",
        "/snapshot",
        "/action",
        "/wait",
        "/click",
        "/fill",
        "/send-keys",
        "/mouse-click",
        "/hover",
        "/select",
        "/check",
        "/scroll",
        "/back",
        "/reload",
        "/screenshot",
        "/save-pdf",
        "/cookies",
        "/upload",
        "/frames",
        "/inject",
        "/challenge",
        "/stealth",
        "/download",
        "/save-pdf",
    ];
    if !ENRICHED_PATHS.contains(&path.as_str()) {
        return next.run(request).await;
    }
    let slot: Arc<StdMutex<Option<ResolvedTargetInfo>>> = Arc::new(StdMutex::new(None));
    let response = TARGET_INFO.scope(slot.clone(), next.run(request)).await;
    let recorded = slot.lock().ok().and_then(|mut guard| guard.take());
    let Some(recorded) = recorded else {
        return response;
    };
    // 响应返回时再取一次会话：标题 / URL / active 可能在 handler 执行期间变化；
    // 会话已被清理时用记录时的快照。
    let fresh = {
        let driver = state.driver.read().await;
        driver
            .sessions
            .get(&recorded.session_key)
            .map(ResolvedTargetInfo::from_session)
    };
    let info = fresh.unwrap_or(recorded);
    enrich_response(response, &info).await
}

/// 把目标身份合并进响应 JSON；返回是否真的改写了内容。
///
/// 只在「对象且已有 `ok` 字段」时注入：错误响应同样需要 `resolved_target`
/// （否则调用方不知道失败发生在哪个标签上），但非 JSON / 非命令回包不能动。
pub(crate) fn merge_target_info(payload: &mut Value, info: &ResolvedTargetInfo) -> bool {
    let Some(object) = payload.as_object_mut() else {
        return false;
    };
    if !object.contains_key("ok") {
        return false;
    }
    object.insert("resolved_target".to_string(), info.resolved_target_json());
    object.insert("browser".to_string(), json!(info.browser_name()));
    object.insert("profile".to_string(), json!(info.profile_name()));
    if !info.active && crate::settings::hidden_tab_warning() {
        // 合并已有 warnings（数组或单个字符串），再按机器码去重追加。
        let mut warnings = match object.get("warnings") {
            Some(Value::Array(items)) => items.clone(),
            Some(Value::String(text)) => vec![Value::String(text.clone())],
            _ => Vec::new(),
        };
        for code in HIDDEN_TAB_WARNINGS {
            if !warnings.iter().any(|item| item.as_str() == Some(code)) {
                warnings.push(Value::String(code.to_string()));
            }
        }
        object.insert("warnings".to_string(), Value::Array(warnings));
        let hint = match object.get("hint").and_then(Value::as_str) {
            Some(existing) if !existing.trim().is_empty() => {
                format!("{existing}；{HIDDEN_TAB_HINT}")
            }
            _ => HIDDEN_TAB_HINT.to_string(),
        };
        object.insert("hint".to_string(), Value::String(hint));
    }
    true
}

async fn enrich_response(response: Response, info: &ResolvedTargetInfo) -> Response {
    let (mut parts, body) = response.into_parts();
    let is_json = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false);
    if !is_json {
        return Response::from_parts(parts, body);
    }
    // 超限原样透传：把正文换掉等于「命令执行了、结果丢了」，比不富化更糟。
    // 长度未知（流式响应）同样跳过：读不出上限就无法保证「超限时还能原样返回」。
    let Some(upper) = body.size_hint().upper() else {
        return Response::from_parts(parts, body);
    };
    if upper > ENRICH_BODY_LIMIT as u64 {
        return Response::from_parts(parts, body);
    }
    let bytes = match axum::body::to_bytes(body, ENRICH_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("target_info: 读取响应体失败，本次跳过富化: {err}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": "内部错误：读取响应体失败，本次响应无法返回",
                    "error_code": "response_body_read_failed",
                    "hint": "重试一次；持续失败请附 daemon 日志反馈",
                })),
            )
                .into_response();
        }
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if !merge_target_info(&mut payload, info) {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let Ok(encoded) = serde_json::to_vec(&payload) else {
        // 序列化理论上是不可失败的兜底；用原 body 而不是空体。
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Ok(length) = axum::http::HeaderValue::from_str(&encoded.len().to_string()) {
        parts.headers.insert(CONTENT_LENGTH, length);
    }
    Response::from_parts(parts, Body::from(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info(active: bool) -> ResolvedTargetInfo {
        ResolvedTargetInfo {
            session_key: "browser-1:profile-1:123".to_string(),
            tab_id: "123".to_string(),
            url: "https://example.com/order".to_string(),
            title: "订单页".to_string(),
            browser_id: "browser-1".to_string(),
            profile_id: "profile-1".to_string(),
            browser_label: Some("Edge · Windows".to_string()),
            profile_label: Some("默认".to_string()),
            active,
            extension_version: Some("5.4.2".to_string()),
        }
    }

    #[test]
    fn latest_spec_parses() {
        // @latest 必须由 TargetSpec 解析成 Latest，而不是被当成 tab id。
        assert_eq!(
            crate::target_spec::TargetSpec::parse("@latest").unwrap(),
            crate::target_spec::TargetSpec::Latest
        );
        assert_eq!(
            crate::target_spec::TargetSpec::parse("latest").unwrap(),
            crate::target_spec::TargetSpec::Latest
        );
    }

    #[test]
    fn merge_adds_identity_and_hidden_warnings() {
        let mut payload = json!({
            "ok": true,
            "result": { "value": 1 },
            "warnings": ["existing"],
            "hint": "原提示",
        });
        assert!(merge_target_info(&mut payload, &sample_info(false)));
        // 既有内容不动。
        assert_eq!(payload["result"]["value"], 1);
        assert_eq!(payload["resolved_target"]["tab_id"], "123");
        assert_eq!(
            payload["resolved_target"]["session_key"],
            "browser-1:profile-1:123"
        );
        assert_eq!(payload["resolved_target"]["browser"], "Edge · Windows");
        assert_eq!(payload["resolved_target"]["profile"], "默认");
        assert_eq!(payload["resolved_target"]["active"], false);
        assert_eq!(payload["resolved_target"]["visible"], false);
        assert_eq!(payload["browser"], "Edge · Windows");
        assert_eq!(payload["profile"], "默认");
        // 已有 warnings 合并、机器码去重。
        assert_eq!(
            payload["warnings"],
            json!(["existing", "tab_hidden", "timers_throttled"])
        );
        assert_eq!(payload["hint"], format!("原提示；{HIDDEN_TAB_HINT}"));
    }

    #[test]
    fn merge_skips_payloads_without_ok() {
        let mut payload = json!({ "result": { "value": 1 } });
        assert!(!merge_target_info(&mut payload, &sample_info(true)));
        assert!(payload.get("resolved_target").is_none());
        assert!(payload.get("browser").is_none());
        assert!(payload.get("warnings").is_none());
    }

    #[test]
    fn visible_tab_gets_no_hidden_warning() {
        let mut payload = json!({ "ok": true });
        assert!(merge_target_info(&mut payload, &sample_info(true)));
        assert_eq!(payload["resolved_target"]["visible"], true);
        assert!(payload.get("warnings").is_none());
        assert!(payload.get("hint").is_none());
    }
}
