use super::*;

/// 扩展控制命令的默认超时。
///
/// 只有「调用方没有自己的超时语义」的简单控制命令（cookies / frames /
/// network 启停）才用它兜底；pageOp 等长命令一律传调用方的 timeout，
/// 否则 `snapshot --timeout 300` 落到 CSP 回退通道上会被硬编码的 30s 截断。
pub(crate) const DEFAULT_EXTENSION_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// payload 形状摘要：用于「扩展回了非对象」时的报错。
///
/// 只带类型与一两个规模数字，不带内容——这里往往就是异常值本身，原样回显
/// 会把一个几 MB 的字符串塞进错误消息。
fn payload_shape(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(text) => format!("string(len={})", text.chars().count()),
        Value::Array(items) => format!("array(len={})", items.len()),
        Value::Object(map) => {
            let keys: Vec<&str> = map.keys().take(6).map(String::as_str).collect();
            format!("object(keys={})", keys.join(","))
        }
    }
}

pub(crate) async fn extension_control(
    state: &AppState,
    session_key: &str,
    command: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value> {
    let mut body = serde_json::Map::new();
    body.insert("cmd".to_string(), json!(command));
    let tab_id = session_tab_id(state, session_key).await?;
    body.insert("tabId".to_string(), json!(parse_tab_id(&tab_id)?));
    if let Value::Object(map) = payload {
        for (key, value) in map {
            body.insert(key, value);
        }
    }
    let response = execute_raw_js_on_tab(
        state,
        &Value::Object(body).to_string(),
        session_key,
        timeout,
    )
    .await
    .map_err(|err| anyhow!(humanize_extension_error(&err.to_string())))?;
    let value = response.data.or(response.result).unwrap_or(Value::Null);
    // `cookies` 等命令的合法回包就是数组：数组没有 ok/error 字段，
    // 不能按「非对象 = 异常」处理，否则会把正常结果判成 invalid payload。
    if value.is_array() {
        return Ok(value);
    }
    // 非对象 payload 一律判错：旧实现只在 `ok===false` 时报错，
    // 扩展返回 null / 数组 / 纯字符串时被当成成功，调用方拿着 `Value::Null`
    // 继续往下走，最终表现成「命令成功但没有结果」。
    if !value.is_object() {
        return Err(anyhow!(
            "invalid extension payload: {command} 返回了 {}，期望对象 {{ok, ...}}",
            payload_shape(&value)
        ));
    }
    // 判错条件同时看 `error` 字段：有些扩展命令用 `{ok:true, error:"..."}`
    // 表示「命令跑了但内部失败」，只认 `ok===false` 会把这类失败吞掉。
    let has_error = value
        .get("error")
        .map(|error| !error.is_null())
        .unwrap_or(false);
    if value.get("ok").and_then(Value::as_bool) == Some(false) || has_error {
        let raw = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("extension command failed");
        return Err(anyhow!(humanize_extension_error(raw)));
    }
    Ok(value)
}

/// 扩展有时把错误再包一层 JSON（`{"error":"..."}` 或 `{"message":"..."}`），
/// 直接拼到命令错误里会出现双重转义的引号，这里剥到最内层文本。
pub(crate) fn humanize_extension_error(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    for _ in 0..3 {
        if !(text.starts_with('{') && text.ends_with('}')) {
            break;
        }
        let parsed = match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(_) => break,
        };
        // 扩展的脚本错误常是嵌套对象：`{"error":{"name":"SyntaxError",
        // "message":"Unexpected token","stack":"..."},"newTabs":[]}`。
        // 旧实现只认字符串形式，于是用户看到整段 JSON（含 eval 栈），
        // 反而找不到真正原因。
        let inner = parsed
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| {
                parsed
                    .get("error")
                    .and_then(Value::as_object)
                    .and_then(|object| object.get("message").and_then(Value::as_str))
            })
            .or_else(|| parsed.get("message").and_then(Value::as_str));
        match inner {
            Some(next) if next.trim() != text => text = next.trim().to_string(),
            _ => break,
        }
    }
    text
}

/// `network_list` / `console_list` 的 limit 钳制。
///
/// 扩展侧对 `limit` 只有 `Math.max(1, ...)` 一级保护，daemon 以前原样透传，
/// 于是 `--limit 1000000` 会让扩展把整份缓冲序列化回来。这里统一夹到 `[1, 500]`，
/// 并把实际值回报（仿 `clamped_note`），避免「我设的值没生效」。
fn clamp_debug_list_limit(requested: usize) -> (usize, Option<Value>) {
    let applied = requested.clamp(1, 500);
    let note = (applied != requested).then(|| {
        json!({
            "field": "limit",
            "requested": requested,
            "applied": applied,
            "min": 1,
            "max": 500,
        })
    });
    (applied, note)
}

pub(crate) async fn network_start(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let max_entries = crate::settings::net_max_entries();
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "networkStart",
            // max_entries：扩展侧按此上限裁剪缓冲；idempotent：已在运行时只更新配置，
            // 不重置已捕获的条目（重复 start 以前会把缓冲清空）。
            json!({ "max_entries": max_entries, "idempotent": true }),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value, "max_entries": max_entries }),
        Err(err) => json!({ "ok": false, "error": err.to_string(), "max_entries": max_entries }),
    })
}

pub(crate) async fn network_list(
    State(state): State<AppState>,
    Json(req): Json<NetworkListRequest>,
) -> Json<Value> {
    touch(&state).await;
    let (limit, clamped) = clamp_debug_list_limit(req.limit);
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "networkList",
            json!({ "filter": req.filter, "limit": limit }),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => {
            let mut out = json!({ "ok": true, "result": value, "limit": limit });
            if let Some(note) = clamped {
                out["clamped"] = note;
            }
            out
        }
        Err(err) => {
            let mut out = json!({ "ok": false, "error": err.to_string(), "limit": limit });
            if let Some(note) = clamped {
                out["clamped"] = note;
            }
            out
        }
    })
}

pub(crate) async fn network_detail(
    State(state): State<AppState>,
    Json(req): Json<NetworkDetailRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "networkDetail",
            json!({ "requestId": req.request_id }),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}

pub(crate) async fn network_clear(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "networkClear",
            json!({}),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}

pub(crate) async fn network_stop(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "networkStop",
            json!({}),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}

pub(crate) async fn console_start(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let max_entries = crate::settings::console_max_entries();
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "consoleStart",
            // 同 network_start：给缓冲一个上界，并让重复 start 只更新配置。
            json!({ "max_entries": max_entries, "idempotent": true }),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => {
            // 把「本次 start 之前是否已经在采集」显式提到顶层。扩展在
            // idempotent 已运行时回 `already_running:true`，`console history`
            // 据此决定采样后要不要 stop（不能把别人开的采集停掉）。
            let already_running = value
                .get("already_running")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            json!({
                "ok": true,
                "result": value,
                "max_entries": max_entries,
                "already_running": already_running,
            })
        }
        Err(err) => json!({ "ok": false, "error": err.to_string(), "max_entries": max_entries }),
    })
}

pub(crate) async fn console_list(
    State(state): State<AppState>,
    Json(req): Json<ConsoleListRequest>,
) -> Json<Value> {
    touch(&state).await;
    let (limit, clamped) = clamp_debug_list_limit(req.limit);
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "consoleList",
            json!({ "level": req.level, "limit": limit }),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => {
            // 扩展的 `consoleList` 回 `{status:"started"|"stopped", count, logs}`
            // （没有 `active` 布尔）。这里补一个顶层的 `active`，让
            // `console history` 不必解析扩展私有字段；不认识 status 时给 null
            // （= 未知），调用方按「未知不擅自 stop」处理。
            let active = value
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "started");
            let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
            let mut out = json!({
                "ok": true,
                "result": value,
                "limit": limit,
                "active": active,
                "count": count,
            });
            if let Some(note) = clamped {
                out["clamped"] = note;
            }
            out
        }
        Err(err) => {
            let mut out = json!({ "ok": false, "error": err.to_string(), "limit": limit });
            if let Some(note) = clamped {
                out["clamped"] = note;
            }
            out
        }
    })
}

pub(crate) async fn console_clear(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "consoleClear",
            json!({}),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}

pub(crate) async fn console_stop(
    State(state): State<AppState>,
    Json(req): Json<TabRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = async {
        let tab_id = select_tab(
            &state,
            SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        )
        .await?;
        extension_control(
            &state,
            &tab_id,
            "consoleStop",
            json!({}),
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await
    }
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}
