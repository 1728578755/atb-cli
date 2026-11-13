use super::*;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CdpCallResult {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl CdpCallResult {
    /// 把 CDP 回包收敛成 `Result`。
    ///
    /// `cdp_call` 的 `Ok(_)` 只代表「传输层拿到了回包」，扩展/Chrome 侧拒绝执行时
    /// 回的是 `Ok(CdpCallResult { ok: false, error: ... })`。挑战点击链路以前只看
    /// `?`（传输层），于是 `Input.dispatchMouseEvent` 被拒绝（未 attach / 参数非法 /
    /// 目标已销毁）时被当成「鼠标事件已经发出」：点击次数照加、确认窗口照开，
    /// 整轮求解在「什么都没发生」上把预算烧完，最后只报一句 click-no-effect。
    ///
    /// 错误里必须带 `method`：同一段代码会派发 mouseMoved / mousePressed /
    /// mouseReleased / dispatchKeyEvent，只报一句 "failed" 无法定位是哪一步。
    pub(crate) fn into_ok(self, method: &str) -> Result<Value> {
        if self.ok {
            return Ok(self.data.unwrap_or(Value::Null));
        }
        Err(anyhow!(
            "cdp {method} 被拒绝：{}",
            self.error.unwrap_or_else(|| "扩展未给出原因".to_string())
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Click,
    Fill,
    MouseClick,
}

#[derive(Debug, Clone)]
pub(crate) struct OperationOptions {
    pub(crate) monitor: bool,
    pub(crate) wait_js: Option<String>,
    pub(crate) wait_timeout: f64,
    pub(crate) wait_interval: f64,
    pub(crate) timeout: f64,
    /// 指定 iframe：此时强制走页面内动作（CDP 的 backendNodeId 只对主框架有效）。
    pub(crate) frame_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AxNode {
    #[serde(rename = "nodeId")]
    pub(crate) node_id: String,
    #[serde(rename = "childIds", default)]
    pub(crate) child_ids: Vec<String>,
    #[serde(rename = "backendDOMNodeId")]
    pub(crate) backend_dom_node_id: Option<i64>,
    pub(crate) role: Option<AxValue>,
    pub(crate) name: Option<AxValue>,
    pub(crate) value: Option<AxValue>,
    pub(crate) description: Option<AxValue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AxValue {
    pub(crate) value: Option<Value>,
    #[serde(rename = "type")]
    pub(crate) value_type: Option<String>,
}

pub(crate) async fn snapshot(
    State(state): State<AppState>,
    Json(req): Json<SnapshotRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(match snapshot_page(&state, req).await {
        Ok(value) => json!({ "ok": true, "result": value }),
        // 与 json_result 同口径：typed 会话选择错误展开成结构化 JSON，其余纯文本。
        Err(err) => session_error_json(&err),
    })
}

pub(crate) async fn click(
    State(state): State<AppState>,
    Json(req): Json<TargetActionRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(
        target_operation_value(&state, OperationKind::Click, req).await,
    ))
}

pub(crate) async fn target_operation_value(
    state: &AppState,
    kind: OperationKind,
    req: TargetActionRequest,
) -> Result<Value> {
    let options = OperationOptions {
        monitor: req.monitor,
        wait_js: req.wait_js,
        wait_timeout: req.wait_timeout,
        wait_interval: req.wait_interval,
        timeout: req.timeout,
        frame_id: req.frame_id,
    };
    run_target_operation(
        state,
        kind,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        &req.target,
        None,
        options,
    )
    .await
}

pub(crate) fn json_result(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => {
            // 受限站点错误在这里 downcast，把稳定 error_code / hint 提到
            // 响应顶层——智能体不用解析中文原文就能按码分支（换无 eval 通道）。
            // 任何只读 `error` 文本的消费方也能直接读懂（Display 即人话结论）。
            if let Some(protected) = err.downcast_ref::<PageProtectedError>() {
                return json!({
                    "ok": false,
                    "error": protected.to_string(),
                    "error_code": "page_protected",
                    "hint": "改用 read / scan / snapshot（走无 eval 通道），或 mouse-click 直接坐标点击",
                    "raw_error": protected.raw,
                });
            }
            // 结果未知（动作可能已执行）单独成一个码 + 退出码 4。
            // 不能和普通失败共用退出码 1：1 的语义是「确定失败，可以放心重试」，
            // 而这里重试就是二次点击。
            if let Some(unknown) = err.downcast_ref::<OutcomeUnknownError>() {
                return json!({
                    "ok": false,
                    "error": unknown.to_string(),
                    "error_code": "outcome_unknown",
                    "outcome": "unknown",
                    "hint": "动作可能已经生效：先读页面状态确认，不要盲目重试；动作类命令可带 --idempotency-key 重发",
                    "raw_error": unknown.raw,
                });
            }
            // typed 会话选择错误（session_gone / session_mismatch /
            // extension_disconnected / target_not_found / tab_closed / no_target_tabs）：
            // downcast 成 SessionSelectError 时展开成结构化 JSON（error_code /
            // retryable / retry_after_ms / hint）；其余错误退回纯文本，文本不变。
            // 以前这里手写 `{"ok":false,"error":err.to_string()}`，typed 信息被剥掉。
            session_error_json(&err)
        }
    }
}

/// 用于「请求被处理了」不等于「业务成功」的命令：外层 `ok` 跟随内层的业务结论。
///
/// 以前 challenge / downloads wait 的超时走 `json_result(Ok(...))`，外层固定 `ok: true`，
/// HTTP 直调方看到 `ok:true` 会以为等待成功。
/// `result` 内部结构与字段名一律不动：CLI 依赖 `/result/passed`、`/result/cleared`、
/// `/result/ok`；只把外层 `ok` 修正为真实结论。
pub(crate) fn json_result_with_inner_ok(result: Result<Value>) -> Value {
    match result {
        Ok(value) => {
            // 没有 `ok` 字段的分支（status/detect）表示「识别本身成功」，按成功处理。
            let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
            if ok {
                return json!({ "ok": true, "result": value });
            }
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("命令未达成目标")
                .to_string();
            let error_code = value.get("error_code").cloned();
            let mut out = json!({ "ok": false, "error": error, "result": value });
            if let Some(code) = error_code {
                out["error_code"] = code;
            }
            // 内层已经给出可执行提示时不要丢：调用方拿到 ok:false 后最需要的就是下一步。
            for key in ["hint", "retryable", "retry_after_ms"] {
                if let Some(field) = value.get(key) {
                    out[key] = field.clone();
                }
            }
            out
        }
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    }
}

/// 判断错误是否为 **CDP 层错误**（协议 / 批回包缺结果），供 click 的 mouse-click
/// 自动降级使用。
///
/// 只匹配 CDP 方法名与「回包缺结果」类短语：页面脚本自己的异常（`TypeError: ...`
/// 之类，经 `exceptionDetails` 上抛）**不在**判定集里——那种错误说明页面脚本已
/// 执行到抛异常点，是否已派发点击不可知，盲目降级有二次点击风险。
/// 中文短语不受 `to_lowercase` 影响，按原文匹配。
pub(crate) fn is_cdp_layer_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    const NEEDLES: [&str; 6] = [
        "runtime.callfunctionon",
        "dom.resolvenode",
        "runtime.evaluate",
        "response missing",
        "没有返回 result.value",
        "batch 回包缺少",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// 错误是否属于「请求已经发出、但结果没回来」。
///
/// 这类错误**不能重放**：页面脚本可能已经执行过 `this.click()` / `setValue`，
/// 只是结果在回程丢了。重试一次 = 第二次点击（重复下单、重复提交表单、
/// 开关被点回去）。判据只认「结果丢失」语义的短语，与 `is_cdp_layer_error`
/// （协议层错误，可能代表「从未派发」）刻意分开。
pub(crate) fn is_ambiguous_outcome(message: &str) -> bool {
    let lower = message.to_lowercase();
    const NEEDLES: [&str; 6] = [
        // 扩展执行超时：ACK 收到了、结果没回来。
        "exec_timeout",
        "timeout",
        "timed out",
        // 回包缺结果：命令发出去了，结果字段没了。
        "response missing",
        "batch 回包缺少",
        // 已经标记过结果未知的错误再被包一层时不要重复处理。
        "outcome_unknown",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// 「动作已发出、结果未知」的结构化错误。
///
/// 与 `PageProtectedError` 同款：`Display` 以 `outcome_unknown:` 开头，任何只读
/// 错误文本的消费方都能看出「不知道成没成」而不是「确定失败」；HTTP 出口
/// （`json_result`）再 downcast 出稳定 `error_code:"outcome_unknown"`，
/// CLI 据此映射退出码 4（与「确定失败」的 1 区分开）。
#[derive(Debug)]
pub(crate) struct OutcomeUnknownError {
    pub(crate) raw: String,
}

impl std::fmt::Display for OutcomeUnknownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "outcome_unknown: {}（请求已发出但没拿到结果，未自动重试：重放可能造成二次点击/输入）",
            self.raw
        )
    }
}

impl std::error::Error for OutcomeUnknownError {}

pub(crate) async fn fill(
    State(state): State<AppState>,
    Json(req): Json<FillRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(fill_value(&state, req).await))
}

pub(crate) async fn fill_value(state: &AppState, req: FillRequest) -> Result<Value> {
    if req.clear && req.has_value {
        return Err(anyhow!("fill: --clear 不能和 value 同时使用"));
    }
    if req.clear && req.append {
        return Err(anyhow!("fill: --clear 不能和 --append 同时使用"));
    }
    if !req.clear && !req.has_value {
        return Err(anyhow!("fill: value is required unless --clear is used"));
    }
    let value = if req.clear { String::new() } else { req.value };
    let options = OperationOptions {
        monitor: req.monitor,
        wait_js: req.wait_js,
        wait_timeout: req.wait_timeout,
        wait_interval: req.wait_interval,
        timeout: req.timeout,
        frame_id: req.frame_id,
    };
    let payload = json!({ "value": value, "append": req.append, "clear": req.clear });
    run_target_operation(
        state,
        OperationKind::Fill,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        &req.target,
        Some(payload),
        options,
    )
    .await
}

pub(crate) async fn mouse_click(
    State(state): State<AppState>,
    Json(req): Json<TargetActionRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(
        target_operation_value(&state, OperationKind::MouseClick, req).await,
    ))
}

pub(crate) async fn send_keys(
    State(state): State<AppState>,
    Json(req): Json<SendKeysRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(send_keys_value(&state, req).await))
}

pub(crate) async fn send_keys_value(state: &AppState, req: SendKeysRequest) -> Result<Value> {
    let options = OperationOptions {
        monitor: req.monitor,
        wait_js: req.wait_js,
        wait_timeout: req.wait_timeout,
        wait_interval: req.wait_interval,
        timeout: req.timeout,
        frame_id: req.frame_id,
    };
    let selector = SessionSelector::new(req.switch_tab_id, req.browser, req.profile);
    let payload = json!({ "keys": req.keys, "target": req.target });
    run_send_keys_operation(state, selector, payload, options).await
}

pub(crate) async fn cookies(
    State(state): State<AppState>,
    Json(req): Json<CookiesRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(cookies_value(&state, req).await))
}

pub(crate) async fn cookies_value(state: &AppState, req: CookiesRequest) -> Result<Value> {
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let action = req.action.as_deref().unwrap_or("list").to_ascii_lowercase();
    if action != "list" && action != "clear" {
        return Err(anyhow!("cookies: action 只能是 list / clear"));
    }
    // url 缺省时用「当前标签的地址」：扩展侧看不到 daemon 的 sender.tab，
    // 不显式传就只能靠 getAll({}) —— 而分区 cookie（cf_clearance 这类）不在里面。
    // 这直接决定 `cookies --clear` 能不能真的清掉挑战 cookie。
    // 只给了 --domain 时不要拿标签 URL 覆盖范围：那会变成「清这个页面」，
    // 而不是「清这个域」。两者语义不同，混用会误删范围。
    let url = match req.url {
        Some(url) => Some(url),
        None if req.domain.is_some() => None,
        None => {
            let driver = state.driver.read().await;
            driver
                .sessions
                .get(&session_key)
                .map(|session| session.info.url.clone())
                .filter(|url| url.starts_with("http"))
        }
    };
    let mut payload = json!({ "action": action });
    if let Some(url) = url {
        payload["url"] = json!(url);
    }
    if let Some(domain) = req.domain {
        payload["domain"] = json!(domain);
    }
    extension_control(
        state,
        &session_key,
        "cookies",
        payload,
        DEFAULT_EXTENSION_CONTROL_TIMEOUT,
    )
    .await
}

pub(crate) async fn inject(
    State(state): State<AppState>,
    Json(req): Json<InjectRequest>,
) -> Json<Value> {
    touch(&state).await;
    // 脚本抛错时 inject_value 会返回 `{ok:false, executed:true, threw:...}`，
    // 必须让顶层 ok 跟着变 false（旧实现用 json_result 会包成 ok:true，调用方
    // 以为执行成功）。同时把执行结果字段提到顶层，形状与改进意见文档一致。
    let value = inject_value(&state, req).await;
    let inner = value.as_ref().ok().cloned();
    let mut out = json_result_with_inner_ok(value);
    if let Some(inner) = inner {
        if let Some(object) = inner.as_object() {
            for key in [
                "executed",
                "returned",
                "threw",
                "duration_ms",
                "action",
                "name",
                "count",
                "warning",
                "warnings",
                "note",
                "cleanup",
            ] {
                if let Some(field) = object.get(key) {
                    out[key] = field.clone();
                }
            }
            if let Some(hint) = object.get("hint") {
                out["hint"] = hint.clone();
            }
        }
    }
    Json(out)
}

pub(crate) async fn inject_value(state: &AppState, req: InjectRequest) -> Result<Value> {
    let action = req
        .action
        .clone()
        .unwrap_or_else(|| "run".to_string())
        .to_lowercase();
    if !["run", "persist", "remove", "list"].contains(&action.as_str()) {
        return Err(anyhow!(
            "inject: action 只能是 run / persist / remove / list"
        ));
    }
    if (action == "run" || action == "persist")
        && req.code.as_deref().unwrap_or_default().trim().is_empty()
    {
        return Err(anyhow!("inject: 需要脚本内容（位置参数或 --file）"));
    }
    if (action == "persist" || action == "remove")
        && req.name.as_deref().unwrap_or_default().trim().is_empty()
    {
        return Err(anyhow!("inject: persist/remove 需要 --name"));
    }
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let mut payload = json!({ "action": action, "userscript": req.userscript });
    if let Some(code) = req.code {
        payload["code"] = json!(code);
    }
    if let Some(name) = req.name {
        payload["name"] = json!(name);
    }
    if let Some(world) = req.world {
        payload["world"] = json!(world.to_uppercase());
    }
    if let Some(matches) = req.matches {
        payload["matches"] = json!(matches);
    }
    if let Some(frame_id) = req.frame_id {
        payload["frameId"] = json!(frame_id);
    }
    if req.all_frames {
        payload["allFrames"] = json!(true);
    }
    // 清理参数对 run / persist 都有效（扩展在真正执行前先跑清理）。
    // 字段始终下发（空数组 / null），新旧扩展都不需要区分「缺省」与「清空」。
    payload["cleanupSelectors"] = json!(req.cleanup_selectors);
    payload["cleanupCode"] = req
        .cleanup_code
        .as_deref()
        .filter(|code| !code.trim().is_empty())
        .map_or(Value::Null, |code| json!(code));
    payload["gmNamespace"] = req
        .gm_namespace
        .as_deref()
        .filter(|namespace| !namespace.trim().is_empty())
        .map_or(Value::Null, |namespace| json!(namespace));
    match action.as_str() {
        // `--persist` 默认 2 小时 TTL；永久必须显式 --persist-forever。
        // 旧扩展忽略这两个字段（保持旧行为：永久），新扩展按 TTL 落库。
        "persist" => {
            // 显式 --ttl 才下发 ttlSec；否则下发 defaultTtlSec，让扩展优先采用
            // 用户在扩展设置页选的 persistTtlSec（没有设置时回落到这个默认值）。
            let ttl_sec = if req.persist_forever {
                None
            } else {
                req.ttl_sec
                    .map(|secs| secs.clamp(1, crate::settings::inject_persist_max_ttl_sec()))
            };
            payload["ttlSec"] = ttl_sec.map_or(Value::Null, |secs| json!(secs));
            payload["persistForever"] = json!(req.persist_forever);
            payload["defaultTtlSec"] = json!(crate::settings::inject_persist_default_ttl_sec());
        }
        // 扩展用 defaultTtlSec 迁移「没有 expiresAt」的旧条目。
        "list" => {
            payload["defaultTtlSec"] = json!(crate::settings::inject_persist_default_ttl_sec());
        }
        _ => {}
    }
    // 不用 extension_control：它把 `ok:false` 一律收敛成 `Err`，会丢掉需要的
    // executed / returned / threw / duration_ms 结构。这里自己组包并保底判错；
    // 其余行为（会话锁、tabId 解析、错误文案）与 extension_control 一致。
    let tab_id = session_tab_id(state, &session_key).await?;
    let mut body = serde_json::Map::new();
    body.insert("cmd".to_string(), json!("inject"));
    body.insert("tabId".to_string(), json!(parse_tab_id(&tab_id)?));
    if let Value::Object(map) = payload {
        for (key, value) in map {
            body.insert(key, value);
        }
    }
    let response = execute_raw_js_on_tab(
        state,
        &Value::Object(body).to_string(),
        &session_key,
        DEFAULT_EXTENSION_CONTROL_TIMEOUT,
    )
    .await
    .map_err(|err| anyhow!(humanize_extension_error(&err.to_string())))?;
    let mut value = response.data.or(response.result).unwrap_or(Value::Null);
    // inject 正常只回对象；数组/标量一律判错。
    if !value.is_object() {
        return Err(anyhow!(
            "invalid extension payload: inject 返回了非对象，期望对象 {{ok, ...}}"
        ));
    }
    // 兼容旧扩展（回包没有 executed）：run 成功时补出字段，CLI 不用按扩展版本分支。
    if action == "run"
        && value.get("executed").is_none()
        && value.get("ok").and_then(Value::as_bool) != Some(false)
    {
        let returned = value.get("value").cloned().unwrap_or(Value::Null);
        value["executed"] = json!(true);
        value["returned"] = returned.clone();
        value["threw"] = Value::Null;
        if value.get("value").is_none() {
            value["value"] = returned;
        }
    }
    let has_error = value
        .get("error")
        .map(|error| !error.is_null())
        .unwrap_or(false);
    let failed = value.get("ok").and_then(Value::as_bool) == Some(false) || has_error;
    // 脚本已经执行后抛错（executed=true 且 threw 非空）保留完整结构，
    // 交给 CLI 决定退出码；注入/编译阶段失败仍按 Err 返回（沿用退出码 1）。
    let script_threw = value.get("executed").and_then(Value::as_bool) == Some(true)
        && value
            .get("threw")
            .map(|threw| !threw.is_null())
            .unwrap_or(false);
    if failed && !script_threw {
        let raw = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("extension command failed");
        return Err(anyhow!(humanize_extension_error(raw)));
    }
    Ok(value)
}

/// `frames` 只需要定位标签：以前复用 `ReadRequest`（带 selector / max_chars /
/// links / budget_ms 等一堆与 iframe 枚举无关的字段），语义误导且校验不到。
/// 字段名与 `ReadRequest` 一致，线协议（CLI 发的 JSON）保持不变。
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct FramesRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

pub(crate) async fn frames(
    State(state): State<AppState>,
    Json(req): Json<FramesRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(frames_value(&state, req).await))
}

pub(crate) async fn frames_value(state: &AppState, req: FramesRequest) -> Result<Value> {
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let value = extension_control(
        state,
        &session_key,
        "frames",
        json!({}),
        DEFAULT_EXTENSION_CONTROL_TIMEOUT,
    )
    .await?;
    Ok(value.get("data").cloned().unwrap_or(value))
}

// ---------------------------------------------------------------- 下载监听

pub(crate) async fn upload(
    State(state): State<AppState>,
    Json(req): Json<UploadRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(upload_value(&state, req).await))
}

pub(crate) async fn upload_value(state: &AppState, req: UploadRequest) -> Result<Value> {
    if req.files.is_empty() {
        return Err(anyhow!("upload: 至少需要一个文件路径"));
    }
    for file in &req.files {
        if !Path::new(file).is_file() {
            return Err(anyhow!("upload: 文件不存在或不是普通文件: {file}"));
        }
    }
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let timeout = secs(req.timeout.clamp(1.0, 300.0));
    let object_id = resolve_target_object(state, &session_key, &req.target, timeout).await?;
    dom_set_files(state, &session_key, &object_id, &req.files, timeout).await
}

/// 用 CDP 的 DOM.setFileInputFiles 设置文件，比 DataTransfer 更接近真实用户选择。
pub(crate) async fn dom_set_files(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    files: &[String],
    timeout: Duration,
) -> Result<Value> {
    if let Some(raw) = object_id.strip_prefix("__backend:") {
        let backend: i64 = raw
            .parse()
            .map_err(|_| anyhow!("upload: backendNodeId 解析失败: {raw}"))?;
        batch_cdp_call(
            state,
            tab_id,
            vec![json!({
                "cmd": "cdp",
                "method": "DOM.setFileInputFiles",
                "params": { "backendNodeId": backend, "files": files }
            })],
            timeout,
        )
        .await?;
    } else if let Some(selector) = object_id.strip_prefix("__selector:") {
        // 用 nodeId 而不是 objectId：部分 Chrome 版本会「接受 objectId 但什么都不做」，
        // 命令返回成功、文件却没挂上（另一个真实触发面）。
        // getDocument → querySelector → setFileInputFiles 是文档化的标准链路。
        batch_cdp_call(
            state,
            tab_id,
            vec![
                json!({ "cmd": "cdp", "method": "DOM.getDocument", "params": { "depth": 1 } }),
                json!({ "cmd": "cdp", "method": "DOM.querySelector", "params": { "nodeId": "$0.root.nodeId", "selector": selector } }),
                json!({ "cmd": "cdp", "method": "DOM.setFileInputFiles", "params": { "nodeId": "$1.nodeId", "files": files } }),
            ],
            timeout,
        )
        .await?;
    } else {
        return Err(anyhow!("upload: 只支持 CSS selector 或 @e 引用"));
    }
    // 证据校验：CDP 对「不是 file input」的目标不一定会报错，扩展的 batch 也可能把
    // 失败吞在 results[i].ok 里。对 CSS 选择器目标，直接回读 `el.files.length`，
    // 只有真的挂上了才报成功——否则 Agent 会以为附件已上传并继续点提交。
    // `@e` 引用（backendNodeId）走 CDP 自身的错误，不做回读：那条链路在部分页面上
    // 无法解析对象（跨扩展 iframe），硬校验会把正常的上传也判失败。
    let mut attached: Option<usize> = None;
    if let Some(selector) = object_id.strip_prefix("__selector:") {
        let script = format!(
            concat!(
                "const el = document.querySelector({sel});",
                "if (!el) return {{ ok: false, reason: 'not_found' }};",
                "const tag = String(el.tagName || '').toUpperCase();",
                "const type = String(el.type || '').toLowerCase();",
                "if (tag !== 'INPUT' || type !== 'file') return {{ ok: false, reason: 'not_file_input', tag: tag }};",
                "const n = el.files ? el.files.length : 0;",
                "return {{ ok: n > 0, count: n }};"
            ),
            sel = serde_json::to_string(selector)?
        );
        let value = execute_page_js(
            state,
            &script,
            SessionSelector::new(Some(tab_id.to_string()), None, None),
            true,
            timeout,
        )
        .await?;
        let verdict = value.get("js_return").cloned().unwrap_or(Value::Null);
        let ok = verdict.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let count = verdict.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
        if !ok {
            let reason = verdict
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("files_not_attached");
            let detail = if reason == "not_file_input" {
                format!(
                    "该元素是 {}，不是文件输入框",
                    verdict
                        .get("tag")
                        .and_then(Value::as_str)
                        .unwrap_or("未知元素")
                )
            } else {
                reason.to_string()
            };
            return Err(anyhow!(
                "upload 失败：目标没有接收文件（{detail}）；请确认选择器指向 <input type=file>"
            ));
        }
        if count != files.len() {
            return Err(anyhow!(
                "upload 失败：期望挂载 {} 个文件，实际只有 {} 个（文件可能不存在或不可读）",
                files.len(),
                count
            ));
        }
        attached = Some(count);
    }
    Ok(json!({
        "status": "success",
        "files": files,
        "count": files.len(),
        "verified": attached.is_some(),
        "attached": attached,
    }))
}

pub(crate) async fn run_target_operation(
    state: &AppState,
    kind: OperationKind,
    selector: SessionSelector,
    target: &str,
    payload: Option<Value>,
    options: OperationOptions,
) -> Result<Value> {
    let tab_id = select_tab(state, selector).await?;
    // `--monitor`：动作前在页面内装 MutationObserver（一次轻量注入），
    // 不再序列化整页 HTML 做前后 diff。装不上时要显式告诉调用方，不能静默。
    let monitor = if options.monitor {
        match start_mutation_watch(state, &tab_id).await {
            Some(monitor_id) => MonitorState::Armed(monitor_id),
            None => MonitorState::Unsupported,
        }
    } else {
        MonitorState::Off
    };
    // 动作体拆成独立函数：内部任何 `?` 提前返回都只会变成这里的 Err，
    // 由 `finish_operation` 统一收掉观察器。旧写法把回收只写在
    // `finish_operation` 的 Armed 分支里，动作链上任何一步提前报错
    // （页面动作 ok:false、wait-js 超时）都会把 observer 留在页面里，
    // 动画/轮询页面会一直累积 mutation 记录。
    let action = run_target_action(state, kind, &tab_id, target, payload, &options).await;
    finish_operation(state, tab_id, action, monitor, options).await
}

// 「目标不存在」类错误：页面动作**从未落到 DOM 上**的失败。
///
/// 只有这种失败可以安全地「重新定位 + 重试」：`page_action.js` 在 `__abcAct` 开头
/// 就做 `querySelector`，命中不到时直接 `{ok:false, error:'target not found: ...'}`，
/// 后面任何 click / setValue 都没执行。反过来，`disabled`、`not a <select>`、
/// 页面脚本中途抛异常都说明动作已经开始，不能重放。
pub(crate) fn is_target_missing_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("target not found")
        || lower.contains("no such element")
        || lower.contains("找不到")
        || lower.contains("unknown_ref")
        || lower.contains("ref_not_found")
}

/// 真正的动作链：只负责「做什么」，不负责收尾。
///
/// 所有失败都以 `Err` 返回而不是就地 `return Err(...)` 之后跳过清理——
/// 观察器的回收统一由 `finish_operation` 处理，成功失败都会走到。
///
/// 复核（哪些 Err 分支不许重放）：
/// 1. 超时 / 回包缺失 / `batch 回包缺少` 在**任何**重定位、页面回退、`dom_mouse_click`
///    之前短路（本函数 Err 分支的第一条判断，以及 selector 早返回分支的同款判断）；
/// 2. `@e` 带 selector 的早返回分支同样先判歧义，再考虑「目标不存在」的重定位重试；
/// 3. `dom_mouse_click` 的降级只在 `matches!(kind, Click)` 且 `is_cdp_layer_error` 时触发，
///    因此 mouse-click 自身失败不会退化成 JS click。
pub(crate) async fn run_target_action(
    state: &AppState,
    kind: OperationKind,
    tab_id: &str,
    target: &str,
    payload: Option<Value>,
    options: &OperationOptions,
) -> Result<Value> {
    let timeout = secs(options.timeout.max(0.1));
    // 指定 frame 时只能用页面内动作：CDP 的 DOM.resolveNode 与 Input 事件永远作用于主框架。
    if let Some(frame_id) = options.frame_id {
        if matches!(kind, OperationKind::MouseClick) {
            return Err(anyhow!(
                "mouse-click 不支持 --frame：真实鼠标事件只能打在主框架的当前视口，页面内没有 frame 级实现"
            ));
        }
        if !is_ref_target(target) {
            let script = if matches!(kind, OperationKind::Fill) {
                "setValue"
            } else {
                "click"
            };
            let operation = page_action_with_fallback(
                state,
                tab_id,
                script,
                json!(target),
                payload.unwrap_or_else(|| json!({})),
                timeout,
                Some(frame_id),
            )
            .await?;
            ensure_page_action_ok(&operation)?;
            return Ok(operation);
        }
        // @e 引用：CDP 的 backendNodeId 只对主框架有效，必须换成 CSS 选择器走页面内动作。
        // 没有选择器时不能退回 CDP 分支，否则会点到主框架里同 id 的另一个节点。
        let lookup = resolve_ref_lookup(state, tab_id, target, timeout).await?;
        let relocated = lookup.as_ref().is_some_and(|value| value.relocated);
        let Some(selector) = lookup.as_ref().and_then(|value| value.selector.clone()) else {
            return Err(anyhow!(
                "该 @e 引用没有可用的 CSS 选择器，不支持跨 frame 操作；请先 snapshot（会生成 selector）或改用 CSS 选择器"
            ));
        };
        let script = if matches!(kind, OperationKind::Fill) {
            "setValue"
        } else {
            "click"
        };
        let operation = page_action_with_fallback(
            state,
            tab_id,
            script,
            json!(selector.clone()),
            payload.unwrap_or_else(|| json!({})),
            timeout,
            Some(frame_id),
        )
        .await?;
        ensure_page_action_ok(&operation)?;
        let mut operation = operation;
        if let Value::Object(map) = &mut operation {
            map.insert("ref_selector".to_string(), json!(selector));
            map.insert("frame_scoped".to_string(), json!(true));
            if relocated {
                map.insert("ref_relocated".to_string(), json!(true));
            }
        }
        return Ok(operation);
    }
    let lookup = resolve_ref_lookup(state, tab_id, target, timeout).await?;
    let relocated = lookup.as_ref().is_some_and(|value| value.relocated);
    // JS 快照产生的 @e 带 CSS 选择器：直接走页面内动作，避免为了一个 ref
    // 去调 CDP（在部分页面上 CDP 根本 attach 不上）。
    if let Some(selector) = lookup.as_ref().and_then(|value| value.selector.clone()) {
        // 第 2 点：URL 没变，但节点被 SPA 整体重渲染时旧 selector 一样会失效
        // （`document.querySelector` 报 target not found）。这种失败是**确定没派发**的，
        // 可以安全地按 role/name 语义重定位一次再试；结果未知（超时 / 回包丢失）
        // 一律不重放 —— 重放等于第二次点击 / 二次输入。
        let mut operation = page_action_for(
            kind,
            state,
            tab_id,
            selector.clone(),
            payload.clone(),
            timeout,
        )
        .await?;
        if let Err(err) = ensure_page_action_ok(&operation) {
            if is_ambiguous_outcome(&err.to_string()) {
                return Err(anyhow::Error::new(OutcomeUnknownError {
                    raw: err.to_string(),
                }));
            }
            // 只有「目标不存在」这一种**从未落到 DOM 上**的失败才值得重定位重试。
            // 其它（disabled / 读到一半抛异常）不重放，避免把「确定失败」变成二次动作。
            if !is_target_missing_error(&err.to_string()) {
                return Err(err);
            }
            let Some(fresh) = retry_relocate_selector(state, tab_id, target, timeout).await? else {
                return Err(err);
            };
            if fresh == selector {
                return Err(err);
            }
            let mut retried =
                page_action_for(kind, state, tab_id, fresh.clone(), payload, timeout).await?;
            ensure_page_action_ok(&retried)?;
            if let Value::Object(map) = &mut retried {
                map.insert("ref_relocated".to_string(), json!(true));
                map.insert("ref_selector".to_string(), json!(fresh));
                map.insert("relocated_after_error".to_string(), json!(err.to_string()));
            }
            return Ok(retried);
        }
        if relocated {
            if let Value::Object(map) = &mut operation {
                map.insert("ref_relocated".to_string(), json!(true));
                map.insert("ref_selector".to_string(), json!(selector));
            }
        }
        return Ok(operation);
    }
    let object_id = match lookup {
        Some(value) => format!("__backend:{}", value.backend_dom_node_id),
        None => format!("__selector:{target}"),
    };
    let operation = match kind {
        OperationKind::Click => dom_click(state, tab_id, &object_id, timeout).await,
        OperationKind::Fill => {
            dom_fill(
                state,
                tab_id,
                &object_id,
                payload.clone().unwrap_or_else(|| json!({})),
                timeout,
            )
            .await
        }
        // 受限页面上 `@e` 只有 backendNodeId 时两条回落都不可用 ——
        // CDP 的 Input.* 被 Chrome 拒绝，页面内合成事件又只接受 CSS 选择器。
        // 失败后按快照里的 role/name 重定位出唯一 selector，再走
        // `mouse_click_selector`（CDP 仍不可用时它内部会回落合成事件）。
        // 重定位不到就原样返回 CDP 错误并补 hint，绝不猜一个节点去点。
        OperationKind::MouseClick => {
            match dom_mouse_click(state, tab_id, &object_id, timeout).await {
                Ok(value) => Ok(value),
                Err(err)
                    if is_ref_target(target)
                        && (is_page_protected_error(&err.to_string())
                            || err.to_string().contains("禁止调试器注入")) =>
                {
                    match retry_relocate_selector(state, tab_id, target, timeout).await? {
                        Some(selector) => {
                            let mut value =
                                mouse_click_selector(state, tab_id, &selector, timeout).await?;
                            if let Value::Object(map) = &mut value {
                                map.insert("ref_relocated".to_string(), json!(true));
                                map.insert("ref_selector".to_string(), json!(selector));
                                map.insert(
                                    "relocated_after_error".to_string(),
                                    json!(err.to_string()),
                                );
                            }
                            Ok(value)
                        }
                        None => {
                            let hint = "该 @e 只有 backendNodeId 且页面禁止调试器注入，\
                                        无法按 role/name 重定位；请重新 snapshot\
                                        （JS 快照会带上 CSS 选择器）或改用 --selector / 坐标点击";
                            if is_page_protected_error(&err.to_string()) {
                                // 保留受限站点的结构化形态（error_code:"page_protected"），
                                // 把 hint 塞进 raw 便于排障。
                                Err(anyhow::Error::new(PageProtectedError {
                                    raw: format!("{err}；重定位失败：{hint}"),
                                }))
                            } else {
                                Err(anyhow!("{err}；hint: {hint}"))
                            }
                        }
                    }
                }
                Err(err) => Err(err),
            }
        }
    };
    let operation = match operation {
        Ok(value) => value,
        Err(err) => {
            // 先分类，再决定要不要「补救」。
            //
            // 恢复链里每一条都会**重新执行一次动作**（语义重定位后走页面脚本、
            // no-eval 降级、mouse-click 降级）。对「结果未知」的错误来说这是
            // 二次点击 / 二次输入：exec_timeout 与 response missing 都发生在
            // **命令已经派发之后**，页面脚本可能已经把 click / setValue 执行完了。
            // 因此这里直接上抛，并标成 outcome_unknown（CLI → 退出码 4），
            // 让调用方先查状态，而不是替它再点一次。
            // 只有「连接级失败」（未连接 / channel closed / 扩展未连接 / 没有这个
            // tab / 节点已失效）才继续走下面的重定位与降级 —— 那种情况请求根本没
            // 落到页面上，重放是安全的。
            if is_ambiguous_outcome(&err.to_string()) {
                return Err(anyhow::Error::new(OutcomeUnknownError {
                    raw: err.to_string(),
                }));
            }
            // SPA 重渲染后 backendNodeId 会失效；这里再试一次语义重定位，
            // 让「页面没换但节点被重建」的场景不必重新 snapshot。
            // `retry_relocate_selector` 在「多个同名候选」时返回 ref_ambiguous（Err），
            // 绝不在歧义里挑一个点。
            if let Some(selector) = retry_relocate_selector(state, tab_id, target, timeout).await? {
                let mut value =
                    page_action_for(kind, state, tab_id, selector.clone(), payload, timeout)
                        .await?;
                ensure_page_action_ok(&value)?;
                if let Value::Object(map) = &mut value {
                    // `ref_relocated` 是布尔标记，新的选择器放 `ref_selector`。
                    map.insert("ref_relocated".to_string(), json!(true));
                    map.insert("ref_selector".to_string(), json!(selector));
                    map.insert("relocated_after_error".to_string(), json!(err.to_string()));
                }
                return Ok(value);
            }
            // CDP 不可用（其他扩展 iframe）且页面禁 eval 时，至少把 CSS 选择器类操作救回来。
            // 只对 click / fill 有「等价的页面内实现」：mouse-click 的页面内等价物就是
            // JS click，那会静默换掉语义，所以这里明确排除。
            if !is_ref_target(target) && matches!(kind, OperationKind::Click | OperationKind::Fill)
            {
                let (op, params) = match kind {
                    OperationKind::Fill => {
                        ("setValue", payload.clone().unwrap_or_else(|| json!({})))
                    }
                    _ => ("click", json!({})),
                };
                if let Ok(mut value) = page_action_with_fallback(
                    state,
                    tab_id,
                    op,
                    json!(target),
                    params,
                    timeout,
                    None,
                )
                .await
                {
                    if let Value::Object(map) = &mut value {
                        map.insert("via".to_string(), json!("no-eval"));
                        map.insert("cdp_error".to_string(), json!(err.to_string()));
                    }
                    // 页面脚本自己报 ok:false（target not found 等）时不能当成成功。
                    // 这是本函数第 5 条恢复路径，前 4 条（frame / ref / selector / 重定位）
                    // 都检查了，唯独这条曾经漏掉。
                    ensure_page_action_ok(&value)?;
                    return Ok(value);
                }
            }
            // CDP 层错误（协议 / 批回包缺结果，如 "Runtime.callFunctionOn
            // response missing"）时，click 自动降级走 mouse-click 路径
            // （Input.dispatchMouseEvent 真实鼠标事件，链路完全不同：bringToFront +
            // resolveNode + 量坐标 + Input 事件）。降级成功带 `used_fallback` /
            // `fallback_reason`，调用方据此知道这次点击换了通道；降级也失败时
            // 原始错误 + 降级错误一起报。页面内点击路径（@e 带 selector、--frame）
            // 在上面已提前 return，不会走到这里，符合「页面内路径不降级」的约定。
            //
            // 之后这条降级只在「确定没派发」的 CDP 层错误上生效：
            // 凡是命中 `is_ambiguous_outcome` 的（exec_timeout / timeout /
            // response missing / batch 回包缺少）已经在函数开头直接上抛，
            // 不再走到这里 —— 那些错误意味着页面脚本可能已经执行过，
            // 降级等于第二次点击。剩下能进来的只有协议层拒绝（resolveNode /
            // objectId 失效一类，动作从未落到 DOM 上），降级是安全的。
            // `blocked` / `covered_by` 遮挡告警由 mouse-click 结果原样携带，
            // action_warnings 照常提升为顶层 warnings，不受影响。
            if matches!(kind, OperationKind::Click) && is_cdp_layer_error(&err.to_string()) {
                return match dom_mouse_click(state, tab_id, &object_id, timeout).await {
                    Ok(mut value) => {
                        if let Value::Object(map) = &mut value {
                            map.insert("used_fallback".to_string(), json!(true));
                            map.insert("fallback_reason".to_string(), json!(err.to_string()));
                        }
                        Ok(value)
                    }
                    Err(fallback_err) => {
                        // 原始错误是受限站点结构化错误时原样返回：anyhow! 拼接会把
                        // error_code / hint 的 downcast 能力剥掉，HTTP 出口就拿不到
                        // 结构化字段了（受限页面上 mouse-click 必然同样失败，原始
                        // 错误对调用方更有用）。
                        if err.downcast_ref::<PageProtectedError>().is_some() {
                            return Err(err);
                        }
                        Err(anyhow!("{err}；mouse-click 降级也失败：{fallback_err}"))
                    }
                };
            }
            return Err(err);
        }
    };
    let mut operation = operation;
    if relocated {
        if let Value::Object(map) = &mut operation {
            map.insert("ref_relocated".to_string(), json!(true));
        }
    }
    Ok(operation)
}

/// 页面动作自己报告失败（例如 target not found）时，命令必须也失败，
/// 否则调用方会以为"点了/填了"，实际什么都没发生。
pub(crate) fn ensure_page_action_ok(operation: &Value) -> Result<()> {
    match operation.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err(anyhow!(
            "{}",
            operation
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("page action failed")
        )),
        // 没有 `ok` 字段 = 页面脚本没有给出结论。以前这里直接放行，于是
        // `null` / `{}` / 失联扩展的空回包全被当成成功。
        // 只接受显式 `success:true` 这一种等价写法。
        None => {
            if operation.get("success").and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            Err(anyhow!(
                "页面动作没有返回结论（缺少 ok 字段）：{}",
                truncate_text(&operation.to_string(), 200)
            ))
        }
    }
}

/// focus 的结论必须看**落点**，不能只看 `ok`。
///
/// 对不可聚焦元素（`<div>` / `<span>` / 没有 tabindex 的容器，或 `focus` 被页面
/// `preventDefault`）调用 `el.focus()` 是 no-op：页面脚本照样回 `ok:true`，但
/// `active:false`。daemon 放行后 `dispatch_keys` 会把按键发给**当前实际聚焦的元素**
/// （通常是 body），命令却报 `success` + `dispatched:N` —— 按键全废且完全静默。
///
/// 缺 `active` 字段（旧扩展）时按「无法证明失败」放行，保持向后兼容。
pub(crate) fn ensure_focus_landed(operation: &Value, target: &str) -> Result<()> {
    ensure_page_action_ok(operation)?;
    match operation.get("active").and_then(Value::as_bool) {
        Some(false) => Err(anyhow!(
            "目标元素无法获得焦点（{target}）：它可能是不可聚焦的容器元素，或页面拦截了 focus。\
             按键没有发送——否则会打进当前实际聚焦的元素（通常是 body）"
        )),
        _ => Ok(()),
    }
}

/// 目标在页面上无法用真实鼠标点击时的结构化错误。
///
/// `mouse-click` 的语义是「派发可信鼠标事件」（`Input.dispatchMouseEvent`）。
/// 拿不到节点 / 量不到几何 / 元素零尺寸时，正确的结论是「这个目标不支持 mouse-click」，
/// 而**不是**悄悄换成 `el.click()`：那会让调用方以为发生了真实点击（站点看到的
/// 是 `isTrusted:false` 的合成事件，也不经过命中测试与遮挡判断）。
pub(crate) fn unsupported_target_error(selector: &str, reason: &str) -> anyhow::Error {
    SessionSelectError {
        code: "unsupported_target",
        message: format!(
            "unsupported_target: {selector} 无法用真实鼠标点击（{reason}）；mouse-click 不会退化成页面内 JS click"
        ),
        hint: "换用唯一的 CSS 选择器 / @e 引用，或改用 click（页面内 JS 点击，语义不同）；\
               元素被隐藏或零尺寸时先让它可见"
            .to_string(),
        retryable: false,
        retry_after_ms: None,
    }
    .into_anyhow()
}

pub(crate) async fn page_action_for(
    kind: OperationKind,
    state: &AppState,
    tab_id: &str,
    selector: String,
    payload: Option<Value>,
    timeout: Duration,
) -> Result<Value> {
    let (op, params) = match kind {
        OperationKind::Fill => ("setValue", payload.unwrap_or_else(|| json!({}))),
        OperationKind::Click => ("click", json!({})),
        // mouse-click 以前落到 `_ => click`，实际执行的是页面内 JS click。
        // 真实鼠标事件走 CDP（Input.dispatchMouseEvent），必须由 `dom_mouse_click` 完成。
        OperationKind::MouseClick => {
            return mouse_click_selector(state, tab_id, &selector, timeout).await
        }
    };
    page_action_with_fallback(state, tab_id, op, json!(selector), params, timeout, None).await
}

/// 用真实鼠标事件点一个 CSS 选择器。
///
/// 拿不到节点 / 量不到几何时返回 `unsupported_target`，**不**退化成页面内 JS click：
/// 那会把「真实鼠标点击」悄悄变成「合成 DOM 事件」，语义、命中测试、遮挡判断全都变了。
pub(crate) async fn mouse_click_selector(
    state: &AppState,
    tab_id: &str,
    selector: &str,
    timeout: Duration,
) -> Result<Value> {
    match dom_mouse_click(state, tab_id, &format!("__selector:{selector}"), timeout).await {
        Ok(value) => Ok(value),
        Err(err) => {
            // 受限站点 / 结果未知这两类必须原样上抛：前者带结构化 error_code，
            // 后者是「可能已经点过」，改写成 unsupported_target 会误导调用方重试。
            if err.downcast_ref::<PageProtectedError>().is_some()
                || is_ambiguous_outcome(&err.to_string())
            {
                return Err(err);
            }
            Err(unsupported_target_error(selector, &err.to_string()))
        }
    }
}

/// 页面动作 + 无 eval 回退：先走「eval 一次页面脚本」，CSP 拦下时改走扩展注入固定文件。
pub(crate) async fn page_action_with_fallback(
    state: &AppState,
    session_key: &str,
    op: &str,
    target: Value,
    params: Value,
    timeout: Duration,
    frame_id: Option<i64>,
) -> Result<Value> {
    let executed = run_page_action_in_frame(
        state,
        session_key,
        op,
        target.clone(),
        params.clone(),
        timeout,
        frame_id,
    )
    .await;
    match executed {
        Ok(value) => Ok(value),
        Err(err) if is_csp_error(&err.to_string()) => {
            let payload = json!({ "op": op, "target": target, "params": params });
            page_op_via_extension(state, session_key, "action", payload, frame_id, timeout).await
        }
        Err(err) => Err(err),
    }
}

/// 引用在快照里，但 CDP/页面动作失败时，按 role+name 再定位一次。
///
/// 返回 `Ok(None)` 表示「不值得/无法重定位」（不是 ref 目标、快照里没有该引用、
/// 或新页面上找不到同名元素）；`ref_ambiguous`（新页面有多个同名候选）以 `Err`
/// 上抛 —— 那种情况下任何自动重放都是在猜，必须让调用方重新 snapshot。
pub(crate) async fn retry_relocate_selector(
    state: &AppState,
    session_key: &str,
    target: &str,
    timeout: Duration,
) -> Result<Option<String>> {
    if !is_ref_target(target) {
        return Ok(None);
    }
    let (entry, generation, snapshot_url) = {
        let driver = state.driver.read().await;
        let Some(cache) = driver.snapshots.get(session_key) else {
            return Ok(None);
        };
        let Some(entry) = cache.refs.get(target).cloned() else {
            return Ok(None);
        };
        (entry, cache.generation, cache.url.clone())
    };
    let selector = match relocate_ref_selector(state, session_key, &entry, timeout).await {
        Ok(selector) => selector,
        // 多个候选：不能挑一个用，原样把结构化错误交回调用方。
        Err(err) if is_ref_ambiguous(&err) => return Err(err),
        // 找不到：让调用方继续走其它恢复路径 / 报原始错误。
        Err(_) => return Ok(None),
    };
    {
        // 写回前校验 generation/URL：重定位期间有人重新 snapshot 或
        // 页面导航，写回就会把新快照的 @e 指向错误节点。
        let mut driver = state.driver.write().await;
        write_back_relocated_selector(
            &mut driver,
            session_key,
            target,
            generation,
            &snapshot_url,
            &selector,
        );
    }
    Ok(Some(selector))
}

pub(crate) async fn run_send_keys_operation(
    state: &AppState,
    selector: SessionSelector,
    payload: Value,
    options: OperationOptions,
) -> Result<Value> {
    let tab_id = select_tab(state, selector).await?;
    // `--monitor`：动作前在页面内装 MutationObserver（一次轻量注入），
    // 不再序列化整页 HTML 做前后 diff。装不上时要显式告诉调用方，不能静默。
    let monitor = if options.monitor {
        match start_mutation_watch(state, &tab_id).await {
            Some(monitor_id) => MonitorState::Armed(monitor_id),
            None => MonitorState::Unsupported,
        }
    } else {
        MonitorState::Off
    };
    let frame_id = options.frame_id;
    // 与 run_target_operation 相同：动作体内部任何提前返回都交给 finish_operation
    // 统一收观察器，focus 失败也不会把 observer 留在页面里。
    let action = run_send_keys_action(state, &tab_id, payload, &options).await;
    let mut result = finish_operation(state, tab_id, action, monitor, options).await?;
    if frame_id.is_some() {
        result["frame_scoped"] = json!(true);
    }
    Ok(result)
}

pub(crate) async fn run_send_keys_action(
    state: &AppState,
    tab_id: &str,
    payload: Value,
    options: &OperationOptions,
) -> Result<Value> {
    let timeout = secs(options.timeout.max(0.1));
    let frame_id = options.frame_id;
    if let Some(frame_id) = frame_id {
        // CDP 的 Input.dispatchKeyEvent 只会打到「当前聚焦文档」，所以必须先把焦点
        // 送进目标子 frame；没有 --target 就没有可控的落点，直接报错而不是静默打到主框架。
        let target = payload
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("send-keys: --frame 需配合 --target 才能把焦点送进子 frame（CDP 按键作用于当前聚焦文档）")
            })?;
        let selector = match resolve_ref_lookup(state, tab_id, target, timeout).await? {
            Some(lookup) => lookup.selector,
            None => None,
        };
        let focus_target = match selector {
            Some(selector) => selector,
            None if is_ref_target(target) => {
                return Err(anyhow!(
                    "该 @e 引用没有可用的 CSS 选择器，不支持跨 frame 操作；请先 snapshot（会生成 selector）或改用 CSS 选择器"
                ))
            }
            None => target.to_string(),
        };
        let focused = page_action_with_fallback(
            state,
            tab_id,
            "focus",
            json!(focus_target),
            json!({}),
            timeout,
            Some(frame_id),
        )
        .await?;
        ensure_focus_landed(&focused, &focus_target)?;
    } else if let Some(target) = payload.get("target").and_then(Value::as_str) {
        let mut focused = None;
        if let Some(lookup) = resolve_ref_lookup(state, tab_id, target, timeout).await? {
            if let Some(selector) = lookup.selector {
                focused = Some(selector);
            }
        }
        match focused {
            Some(selector) => {
                let focused =
                    run_page_action(state, tab_id, "focus", json!(selector), json!({}), timeout)
                        .await?;
                // 页面动作可能以 ok:false 返回而不是 Err；而且「元素找到了、但没聚焦
                // 成功」（`active:false`）也必须拦——否则按键会静默打进 body。
                ensure_focus_landed(&focused, &selector).map_err(|err| {
                    anyhow!("无法把焦点移到目标元素（{selector}），按键未发送：{err}")
                })?;
            }
            None => {
                let object_id = resolve_target_object(state, tab_id, target, timeout).await?;
                focus_object(state, tab_id, &object_id, timeout).await?;
            }
        }
    }
    dispatch_keys(
        state,
        tab_id,
        payload
            .get("keys")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        timeout,
    )
    .await
}

/// 收尾：等 wait-js、收 `--monitor` 观察器、组装返回体。
///
/// `action` 是动作链的结果：**失败也走这里**，先把观察器收掉再原样返回错误。
/// 这是的核心——旧实现只在成功路径回收，任何提前 `return Err`
/// （页面动作 ok:false、wait-js 超时）都会让 observer 与 mutation 缓冲
/// 跟着页面一直活着，动画/轮询页面会持续吃内存。
pub(crate) async fn finish_operation(
    state: &AppState,
    session_key: String,
    action: Result<Value>,
    monitor: MonitorState,
    options: OperationOptions,
) -> Result<Value> {
    let operation = match action {
        Ok(value) => value,
        Err(err) => {
            // 丢弃观察数据：动作已经失败，调用方要的是错误本身。
            let _ = collect_mutation_watch(state, &session_key, monitor).await;
            return Err(err);
        }
    };
    let physical_tab_id = session_tab_id(state, &session_key)
        .await
        .unwrap_or_default();
    let mut result = json!({ "status": "success", "tab_id": physical_tab_id, "session_key": session_key, "operation": operation });
    // 「已执行，但可能不是你以为的那样」必须提到顶层：页面脚本早就把
    // covered_by / hit.matches 算出来了，但以前只有页面侧有、daemon 侧没人读，
    // 于是「目标被 cookie 弹窗盖住、真实鼠标根本点不到」和成功长得一模一样。
    let warnings = action_warnings(&result["operation"]);
    if !warnings.is_empty() {
        result["warnings"] = Value::Array(warnings);
    }
    if let Some(wait_js) = options.wait_js.as_deref() {
        // --wait-js 必须和动作落在同一个 frame：`frame_id` 就在 options 里，旧实现丢掉它，
        // 条件在主框架求值 —— 子 frame 里的动作要么假命中（主框架恰好满足）、
        // 要么永远等不到而误报超时。
        let wait_target = ExecTargetOptions {
            frame_id: options.frame_id,
            ..Default::default()
        };
        let wait = match wait_for_js_condition_with_target(
            state,
            Some(session_key.as_str()),
            wait_js,
            options.wait_timeout,
            options.wait_interval,
            None,
            &wait_target,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                // 等待本身失败属于错误路径，同样要回收观察器。
                let _ = collect_mutation_watch(state, &session_key, monitor).await;
                return Err(err);
            }
        };
        if !wait.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let _ = collect_mutation_watch(state, &session_key, monitor).await;
            return Err(anyhow!(json!({ "error": format!("wait-js timeout after {}s", options.wait_timeout), "operation": result["operation"].clone(), "wait": wait }).to_string()));
        }
        result["wait"] = wait;
    }
    let (change, monitor_note) = collect_mutation_watch(state, &session_key, monitor).await;
    if let Some(change) = change {
        result["change"] = change;
    }
    if let Some(note) = monitor_note {
        result["monitor"] = note;
    }
    Ok(result)
}

/// 把页面动作结果里「已经执行、但真实用户可能得不到同样结果」的信号提成顶层
/// `warnings` 数组。
///
/// 目前只有一类：`blocked` —— 目标中心点被别的元素（cookie 弹窗 / 登录浮层 /
/// 全屏 click-catcher）盖住。页面脚本仍然把事件派发给了目标（行为不前移），
/// 但真实鼠标会落在遮罩上，调用方需要知道这一点，而不是从 `operation.hit` 里
/// 自己翻。字段保持机器可读：code / message / covered_by。
pub(crate) fn action_warnings(operation: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if operation.get("blocked").and_then(Value::as_bool) == Some(true) {
        out.push(json!({
            "code": "action_target_covered",
            "message": operation
                .get("hint")
                .and_then(Value::as_str)
                .unwrap_or("目标中心点被其它元素遮住：真实鼠标会点到遮罩上，事件已派发到目标本身"),
            "covered_by": operation.get("covered_by").cloned().unwrap_or(Value::Null),
        }));
    }
    out
}

/// 收掉一次 `--monitor` 观察器，返回 `(change, 降级说明)`。
///
/// 变更观察的输出结构与 `js --monitor` 的 `change` 同构
/// （changed/added/removed/attributes/text/samples），不回传原始 HTML 片段。
/// 三种降级都用 `monitor.reason` 给出机器可读原因，别让调用方去猜中文。
pub(crate) async fn collect_mutation_watch(
    state: &AppState,
    session_key: &str,
    monitor: MonitorState,
) -> (Option<Value>, Option<Value>) {
    match monitor {
        MonitorState::Off => (None, None),
        MonitorState::Unsupported => (
            None,
            Some(json!({
                "ok": false,
                "reason": "unsupported",
                "error": "CSP 或页面环境不支持变更观察（动作本身已执行）"
            })),
        ),
        MonitorState::Armed(monitor_id) => {
            match stop_mutation_watch(state, session_key, &monitor_id).await {
                Ok(MonitorOutcome::Collected(watch)) => (Some(watch), None),
                Ok(MonitorOutcome::Navigated) => {
                    // 导航本身就是「页面变了」：这个结论不需要观察器证明。
                    // 旧实现（前后整页对比）在这种情况下也是 changed:1。
                    // 零值键必须补齐：`change` 恒定有 6 个键，调用方才能无条件读
                    // `change.samples.length` / `added` 而不抛错（导航时没有明细）。
                    (
                        Some(json!({
                            "changed": 1,
                            "navigated": true,
                            "added": 0,
                            "removed": 0,
                            "attributes": 0,
                            "text": 0,
                            "samples": [],
                        })),
                        Some(json!({
                            "ok": false,
                            "reason": "navigated",
                            "error": "观察期间页面发生导航，变更计数以导航为准（无样本）"
                        })),
                    )
                }
                Ok(MonitorOutcome::Superseded) => (
                    None,
                    Some(json!({
                        "ok": false,
                        "reason": "superseded",
                        "error": "同一标签上有更新的 --monitor 动作覆盖了本次观察"
                    })),
                ),
                // 收数本身失败（注入被拒 / 上下文已销毁）：动作已经执行完，
                // 不能把整条命令判失败，但必须让调用方看出「观察没拿到」。
                Err(err) => (
                    None,
                    Some(json!({
                        "ok": false,
                        "reason": "injection_failed",
                        "error": err.to_string()
                    })),
                ),
            }
        }
    }
}
