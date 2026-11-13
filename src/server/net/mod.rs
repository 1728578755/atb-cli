//! `net` 命令族：页面级请求拦截、CDP 网络层规则、hook 注入。
//!
//! 原来是一个 1 283 行的单文件，按「谁在做事」拆开：
//! - [`settings`]：profile 键、站点匹配、配置读写（含锁语义）
//! - [`log`]：日志合并（页面侧 + CDP 侧）
//! - [`page`]：页面引擎的调用与跨标签广播
//! - [`cdp`]：CDP 引擎的规则下发与调试通道
//! - [`hook`]：持久注入（document_start）的安装与移除
//!
//! 入口 [`net`] / [`net_value`] 留在本文件：它只做参数整形与分派，
//! 具体动作都在上面几个子模块里。

use super::*;

/// `net` 请求体。
///
/// **入站线协议是 snake_case**（`parse_json` / `debug_pass` / `max_chars` …）：
/// daemon 的 HTTP 请求体由本 crate 的 CLI 构造，字段名与这里一致。下发给页面脚本
/// / 扩展时才转成 camelCase（`parseJson` / `debugPass` / `maxChars`），两套命名
/// 不要混用。
///
/// `deny_unknown_fields`：字段名写错（例如把 `parse_json` 写成 `parseJson`
/// 之外的第三种拼法）以前会被静默忽略，命令看起来成功但参数没生效。这里只对
/// **内部请求 DTO** 收紧；易混字段另外挂了 camelCase alias，所以两种写法都收。
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetRequest {
    /// fetch | hook | log | route | unroute | routes | reset
    pub(crate) action: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) method: Option<String>,
    pub(crate) headers: Option<Value>,
    pub(crate) body: Option<Value>,
    /// 响应体上限（字符）与返回形式 text | base64
    #[serde(default, alias = "maxChars")]
    pub(crate) max_chars: Option<usize>,
    #[serde(default, alias = "responseType")]
    pub(crate) response_type: Option<String>,
    /// fetch 的凭据模式：include（默认）/ omit / same-origin。
    /// 页面脚本早就读 `opts.credentials`，但 CLI 侧一直没有下发通道。
    #[serde(default)]
    pub(crate) credentials: Option<String>,
    /// fetch 是否把 JSON 响应体解析成对象（页面默认 true）。
    /// 入站字段是 snake_case `parse_json`（兼容 camelCase `parseJson`）；
    /// 下发给页面脚本时才叫 `parseJson` —— 页面读的是那个名字。
    #[serde(default, alias = "parseJson")]
    pub(crate) parse_json: Option<bool>,
    /// hook 选项
    #[serde(default, alias = "captureBodies")]
    pub(crate) capture_bodies: Option<bool>,
    #[serde(default, alias = "maxLog")]
    pub(crate) max_log: Option<usize>,
    /// log / unroute 的过滤条件
    #[serde(rename = "match")]
    pub(crate) pattern: Option<String>,
    pub(crate) limit: Option<usize>,
    pub(crate) clear: Option<bool>,
    /// route 规则：{match, method, status, body, headers, abort, requestHeaders, requestBody, response}
    pub(crate) rule: Option<Value>,
    /// 拦截引擎：page（默认，包装 fetch/XHR）/ cdp（扩展侧 Fetch 域，能改二进制体）
    /// 对 `log` 动作还接受 `both`（合并页面侧与扩展侧，缺省行为）。
    pub(crate) engine: Option<String>,
    /// log：是否记录被规则放行（pass）的请求，供排查「为什么没命中 mock」。
    /// 入站字段是 snake_case `debug_pass`（兼容 camelCase `debugPass`）；
    /// 下发给扩展时才叫 `debugPass` —— 扩展侧读的就是那个名字。
    #[serde(default, alias = "debugPass")]
    pub(crate) debug_pass: Option<bool>,
    /// hook 是否持久化（导航后自动重装）
    #[serde(default)]
    pub(crate) persist: bool,
    /// 持久化的 URL 匹配模式（默认 <all_urls>）
    pub(crate) matches: Option<Vec<String>>,
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) frame_id: Option<i64>,
    pub(crate) timeout: Option<f64>,
}

/// 等该 profile 落盘锁的上限。
///
/// 锁在整个读-改-写期间被持有，持锁内的每个网络等待都有自己的 timeout；
/// 这里再给"等锁"本身一个上限，避免一条卡死的 net 命令把**同一 profile** 的
/// 后续 net 命令无限堵住。锁按 profile 分配，其它 profile 不受影响。
pub(crate) const NET_RMW_LOCK_WAIT: Duration = Duration::from_secs(150);

/// `net reset` 的成功判据：三个维度都必须成立。
///
/// 旧实现只看页面 reset 与 CDP stop，持久注入（hook）删除失败时照样报 `ok:true`，
/// 旧 mock 会在下次导航时「复活」。这里把判据收成一个纯函数，便于直接断言。
pub(crate) fn reset_outcome_ok(page_ok: bool, cdp_stopped: bool, hook_removed: &Value) -> bool {
    page_ok && cdp_stopped && hook_removed.get("ok").and_then(Value::as_bool) == Some(true)
}

// 页面持久注入重放时的裁剪信息。
///
/// 页面侧 `page_net.js` 的重放只接受前 `MAX_ROUTES`（64）条，并回报
/// `routes_dropped`；daemon 侧以前完全不读它，于是 env 把 `ATB_NET_RULE_MAX_RULES`
/// 放大到 64 以上时会出现「`net routes` 显示 N 条、浏览器只生效 64 条、没有任何提示」。
/// 返回 `(丢弃条数, 人话告警)`；没有丢弃（或页面没回该字段）返回 `None`。
///
/// 文案里的上限优先取页面回报的 `routes_cap`（页面侧 `page_net.js` 的 `MAX_ROUTES`
/// 直接派生，天然不会漂）；页面没回该字段时回落到 **64** —— 这个 64 是**页面侧常量**
/// 的当前值，不是 daemon 的 `ATB_NET_RULE_MAX_RULES`（后者可被环境变量放大到 500）。
/// 两处同时调整时必须一起改；页面一旦开始回 `routes_cap`，这里会自动跟随，不必再动 Rust。
pub(crate) fn routes_dropped_note(payload: &Value) -> Option<(u64, String)> {
    let dropped = payload.get("routes_dropped").and_then(Value::as_u64)?;
    if dropped == 0 {
        return None;
    }
    let cap = payload
        .get("routes_cap")
        .and_then(Value::as_u64)
        .unwrap_or(64);
    Some((
        dropped,
        format!(
            "持久注入只重放前 {cap} 条页面规则，本次丢弃 {dropped} 条（页面侧 MAX_ROUTES 上限）；\
             `net routes` 显示的是配置全量，浏览器实际生效的是裁剪后的子集。\
             要放宽上限需同时调大页面侧常量"
        ),
    ))
}

pub(crate) async fn net(State(state): State<AppState>, Json(req): Json<NetRequest>) -> Json<Value> {
    touch(&state).await;
    // 只有 reset 在顶层带 `ok`（页面级 reset 可能失败）；其它 action 没有该字段，
    // 外层仍按「请求被处理了」处理，语义不变。
    Json(json_result_with_inner_ok(net_value(&state, req).await))
}

pub(crate) async fn net_value(state: &AppState, req: NetRequest) -> Result<Value> {
    let action = req
        .action
        .clone()
        .unwrap_or_else(|| "fetch".to_string())
        .to_lowercase();
    let raw_engine = req.engine.clone().map(|value| value.to_lowercase());
    let engine = raw_engine.clone().unwrap_or_else(|| "page".to_string());
    // `log` 的 engine 是日志来源过滤（page / cdp / both，缺省 both=合并双源），
    // 其它动作的 engine 才是拦截引擎（page / cdp）。
    let log_source = if action == "log" {
        let source = raw_engine.clone().unwrap_or_else(|| "both".to_string());
        if !matches!(source.as_str(), "page" | "cdp" | "both") {
            return Err(anyhow!(
                "net log: 未知 engine {source}（可选 page / cdp / both）"
            ));
        }
        source
    } else {
        if !matches!(engine.as_str(), "page" | "cdp") {
            return Err(anyhow!("net: 未知 engine {engine}（可选 page / cdp）"));
        }
        // cdp 引擎只实现 route / mock / unroute；其它 action 实际走页面注入。
        // 以前放行后仍回报 engine:"cdp"，语义与结果都对不上。
        if engine == "cdp" && !matches!(action.as_str(), "route" | "mock" | "unroute") {
            return Err(anyhow!(
                "net {action}: --engine cdp 只支持 route / mock / unroute（该 action 只支持页面引擎，请去掉 --engine cdp）"
            ));
        }
        "both".to_string()
    };
    let mut opts = json!({ "action": action });
    for (key, value) in [
        ("url", req.url.clone()),
        ("method", req.method.clone()),
        ("responseType", req.response_type.clone()),
        ("match", req.pattern.clone()),
    ] {
        if let Some(value) = value {
            opts[key] = json!(value);
        }
    }
    if let Some(headers) = req.headers.clone().filter(|value| !value.is_null()) {
        let object = if let Some(text) = headers.as_str() {
            // 解析失败不能再静默变 `{}`：请求会少一堆头，用户却看到成功。
            serde_json::from_str::<Value>(text).map_err(|err| {
                let preview: String = text.chars().take(120).collect();
                anyhow!("net: --headers 不是合法的 JSON（{preview}）：{err}")
            })?
        } else {
            headers
        };
        opts["headers"] = object;
    }
    if let Some(body) = req.body.clone().filter(|value| !value.is_null()) {
        opts["body"] = body;
    }
    // fetch 的两个页面侧开关：页面脚本读的是 `credentials` / `parseJson`，
    // 不传时不覆盖页面默认值（include / true）。
    if action == "fetch" {
        if let Some(credentials) = req.credentials.as_deref() {
            let applied = credentials.trim().to_lowercase();
            if !matches!(applied.as_str(), "include" | "omit" | "same-origin") {
                return Err(anyhow!(
                    "net fetch: --credentials 只支持 include / omit / same-origin"
                ));
            }
            opts["credentials"] = json!(applied);
        }
        if let Some(parse_json) = req.parse_json {
            opts["parseJson"] = json!(parse_json);
        }
    }
    // max-chars 保留 clamp 兜住极端值，但实际值被改写时必须告诉调用方，
    // 否则 `--max-chars 200` 被抬到 1000 会表现为「我设的上限没生效」。
    let mut clamped_note: Option<Value> = None;
    if let Some(max_chars) = req.max_chars {
        let applied = max_chars.clamp(100, 2_000_000);
        if applied != max_chars {
            clamped_note = Some(json!({
                "field": "maxChars",
                "requested": max_chars,
                "applied": applied,
                "min": 100,
                "max": 2_000_000,
            }));
        }
        opts["maxChars"] = json!(applied);
    }
    if let Some(capture) = req.capture_bodies {
        opts["captureBodies"] = json!(capture);
    }
    if let Some(max_log) = req.max_log {
        opts["maxLog"] = json!(max_log.clamp(10, 5000));
    }
    if let Some(limit) = req.limit {
        opts["limit"] = json!(limit.clamp(1, 500));
    }
    if let Some(clear) = req.clear {
        opts["clear"] = json!(clear);
    }
    if let Some(rule) = req.rule.clone().filter(|value| !value.is_null()) {
        let rule = if let Some(text) = rule.as_str() {
            // 解析失败必须直接报错：以前写入 Value::Null，键存在但值为空，
            // 既绕过了下面的必填校验，又把错误推迟到 cdp 引擎。
            serde_json::from_str::<Value>(text).map_err(|err| {
                let preview: String = text.chars().take(120).collect();
                anyhow!("net: --rule 不是合法的 JSON（{preview}）：{err}")
            })?
        } else {
            rule
        };
        // 规则在进入任何引擎之前统一校验：body 大小/类型、规则条数。
        validate_net_rule(&action, &rule)?;
        opts["rule"] = rule;
    }
    if action == "fetch"
        && opts
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .is_empty()
    {
        return Err(anyhow!("net fetch: 需要 url"));
    }
    // 值为 Null 也算「没有 rule」：JSON 里的 null 不是可用的规则。
    if action == "route" && opts.get("rule").filter(|value| !value.is_null()).is_none() {
        return Err(anyhow!(
            "net route: 需要 --match / --url，以及可选的 --status / --body"
        ));
    }

    let session_key = select_tab(
        state,
        SessionSelector::new(
            req.switch_tab_id.clone(),
            req.browser.clone(),
            req.profile.clone(),
        ),
    )
    .await?;
    // net 配置按 browser:profile 存：同一 profile 的所有标签共享同一套规则。
    let profile_key = net_profile_key(state, &session_key).await?;
    let timeout = secs(req.timeout.unwrap_or(30.0).clamp(1.0, 120.0));

    // 会修改共享配置的 action 必须把「读 → 下发 → 写」放进同一把锁：
    // 旧实现读完之后夹着一次页面往返（await）才写回，两个并发 `net route` 后写
    // 覆盖先写；`net reset` 与 `route` 交错时，route 还会把已删除的 profile 写回来。
    //
    // 锁粒度是 **per-profile**：全局单锁会让 A profile 的慢命令把
    // B profile 的 net 命令一起堵在 `NET_RMW_LOCK_WAIT` 上，叠加后调用方预算先耗尽。
    //
    // 持锁期间的所有网络等待都有自己的 timeout（`extension_control` / `net_page_call`），
    // 拿锁本身也设上限：否则一条卡死的 net 命令会把同 profile 的后续 net 命令永久堵住。
    let mutating = matches!(
        action.as_str(),
        // `routes` 通常只读，但扩展重启后的重建会把 `cdpRebuildRequired` 清掉并落盘，
        // 所以也进锁：它和 `route` 并发时不能一个读到标记、另一个刚清掉。
        "routes" | "hook" | "route" | "unroute" | "mock" | "reset"
    );
    let _rmw_guard = if mutating {
        match tokio::time::timeout(NET_RMW_LOCK_WAIT, profile_persist_lock(state, &profile_key))
            .await
        {
            Ok(guard) => Some(guard),
            Err(_) => {
                return Err(anyhow!(
                    "net: 该 profile 的另一个 net 配置操作已持续超过 {}s 未结束，请稍后重试",
                    NET_RMW_LOCK_WAIT.as_secs()
                ))
            }
        }
    } else {
        None
    };
    let mut config = {
        let driver = state.driver.read().await;
        driver
            .net_hooks
            .get(&profile_key)
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(net_config_default)
    };

    // routes 是只读查询：顺带把网络层规则重新下发给该 profile 的每个标签
    // （新开的标签还没有 Fetch.enable，重新下发一次就补齐了）。
    if action == "routes" {
        let mut applied = Value::Null;
        // 只读命令只换规则：传 `rules` 而不是 `start`——后者会重置扩展侧统计/日志，
        // 还要重新走一遍 Fetch.disable + Fetch.enable（同一套 patterns 也会被判为需要重开）。
        // 同理第 4 个实参必须是 cdp 规则数组，整份 config 会被当成 CDP 规则解析。
        let cdp_rules = config.get("cdpRules").cloned().unwrap_or_else(|| json!([]));
        let cdp_enabled = config.get("cdpEnabled").and_then(Value::as_bool) == Some(true);
        // 扩展重启过：浏览器侧 Fetch 已失效，光发 `rules` 不会重新 enable，
        // 必须用 `start` 整体重建。重建会清掉扩展侧日志/统计，所以只在真重启后做一次。
        let rebuild = config.get("cdpRebuildRequired").and_then(Value::as_bool) == Some(true);
        if cdp_enabled || rebuild {
            // 下发失败不能静默：旧实现 `if let Ok(value)` 吞错，`applied` 保持 null，
            // 调用方分不清「没有规则要下发」和「扩展根本没收到」。
            applied = match net_cdp_broadcast(
                state,
                &profile_key,
                if rebuild { "start" } else { "rules" },
                &cdp_rules,
                timeout,
            )
            .await
            {
                Ok(value) => value,
                Err(err) => json!({
                    "ok": false,
                    "applied": 0,
                    "error": humanize_extension_error(&err.to_string()),
                }),
            };
            // 只有「所有标签都确认」才算重建完成：部分失败要留着标记，
            // 下一次查询继续补齐（`start` 幂等，失败的标签会再次尝试）。
            let fully_applied = applied.get("ok").and_then(Value::as_bool) == Some(true)
                && applied
                    .get("failed")
                    .and_then(Value::as_array)
                    .map(|failed| failed.is_empty())
                    .unwrap_or(true);
            if rebuild && fully_applied {
                if let Some(object) = config.as_object_mut() {
                    object.insert("cdpRebuildRequired".to_string(), json!(false));
                }
                store_net_config_locked(state, &profile_key, &config).await;
            }
        }
        let routes = config.get("routes").cloned().unwrap_or_else(|| json!([]));
        // match pattern 是 host 级（Chrome 语法里 host 段不接受端口），
        // 调用方需要知道「这个 pattern 会覆盖同一 host 的其它端口」。
        // 只读回显，不改配置；要按端口隔离请用 `--match` 或 `--engine cdp`。
        let page_pattern = current_site_match_pattern(state, &session_key)
            .await
            .map(|(pattern, _)| pattern);
        return Ok(json!({
            "tab_id": tab_id_of_session(&session_key),
            "session_key": session_key,
            "profile": profile_key,
            "engine": engine,
            "net": {
                "ok": true,
                "action": "routes",
                "count": routes.as_array().map(Vec::len).unwrap_or(0),
                "routes": routes,
                "persist": config.get("persist").cloned().unwrap_or(json!(false)),
                "matches": config.get("matches").cloned().unwrap_or_else(|| json!(["<all_urls>"])),
                // 当前页面对应的站点 pattern，以及它的端口覆盖范围（只读）。
                "pattern": page_pattern,
                "port_scope": "host-only",
                "cdp": {
                    "enabled": config.get("cdpEnabled").cloned().unwrap_or(json!(false)),
                    "count": cdp_rules.as_array().map(Vec::len).unwrap_or(0),
                    "rules": cdp_rules,
                    "applied": applied,
                }
            }
        }));
    }

    // reset 语义统一：清空该 profile 的页面级规则与网络层规则。
    if action == "reset" {
        let had_cdp = config.get("cdpEnabled").and_then(Value::as_bool) == Some(true)
            || config
                .get("cdpRules")
                .and_then(Value::as_array)
                .is_some_and(|rules| !rules.is_empty());
        // 顺序必须是「先停 CDP，再删配置」。旧实现先 `remove_net_config`，
        // 再 `let _ = stop` 吞掉错误，还无条件回报 `cdp_stopped: had_cdp`：一旦
        // 全 tab 停止失败，配置已经没了，`net routes` 看到 cdpEnabled:false、
        // `unroute --engine cdp` 也不再下发 —— 那些标签的拦截从此没有任何命令能停掉。
        let cdp_result = if had_cdp {
            match net_cdp_broadcast(state, &profile_key, "stop", &json!([]), timeout).await {
                Ok(value) => value,
                Err(err) => json!({
                    "ok": false,
                    "applied": 0,
                    "error": humanize_extension_error(&err.to_string()),
                }),
            }
        } else {
            Value::Null
        };
        let cdp_failed: Vec<Value> = cdp_result
            .get("failed")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // 有标签没停下来就保留配置：下一次 reset / unroute 还能再命中它们。
        let cdp_stopped = !had_cdp
            || (cdp_result.get("ok").and_then(Value::as_bool) == Some(true)
                && cdp_failed.is_empty());
        if cdp_stopped {
            // reset 要连持久化条目一起删，否则 daemon 重启后旧规则会「复活」。
            remove_net_config_locked(state, &profile_key).await;
        }
        // 页面侧 reset 也按 profile 下发到每个标签：配置是 profile 级的，
        // 只 reset 当前标签会出现「B 报成功、A 仍在 mock」。
        let (mut payload, page_failures) = match net_page_broadcast(
            state,
            &profile_key,
            &session_key,
            &json!({ "action": "reset" }),
            timeout,
            req.frame_id,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                let message = err.to_string();
                (
                    json!({ "ok": false, "action": "reset", "error": message.clone() }),
                    vec![json!({
                        "tab_id": tab_id_of_session(&session_key),
                        "error": message,
                    })],
                )
            }
        };
        // 持久注入不论 `persist` 现在是真是假都要尝试移除：
        // `net hook --persist` 拿不到站点时只把 persist 置 false 而不注册，
        // 旧注入却还留在扩展存储里，每次匹配页面都重装旧 mock。
        let hook_removed = match remove_net_hook(state, &session_key, &profile_key).await {
            Ok(value) => value,
            Err(err) => json!({
                "ok": false,
                "error": humanize_extension_error(&err.to_string()),
            }),
        };
        payload["cdp_stopped"] = json!(cdp_stopped);
        // 休眠标签的「超时」不是 reset 失败的证据，只是没回应。把它算成失败会让
        // 100+ 标签的浏览器里 `net reset` 永远返回非 0（实测 86/108 都是休眠页）。
        // 明确报错才让命令失败；未确认的单列出来，提示「下次导航自然失效」。
        let (page_failed, page_unconfirmed) = split_reset_failures(&page_failures);
        let page_ok = page_failed.is_empty();
        // 持久注入删除失败必须算失败：旧实现只把「页面 reset + CDP stop」
        // 计入 reset_ok，hook 删不掉时照样报 ok:true —— 扩展存储里的旧注入还在，
        // 下次导航会把已经 reset 掉的 mock 重新装回来（「reset 成功但规则还生效」）。
        let hook_ok = hook_removed.get("ok").and_then(Value::as_bool) == Some(true);
        let reset_ok = reset_outcome_ok(page_ok, cdp_stopped, &hook_removed);
        let mut out = json!({
            "ok": reset_ok,
            "tab_id": tab_id_of_session(&session_key),
            "session_key": session_key,
            "profile": profile_key,
            "engine": engine,
            "net": payload,
            "page_reset": {
                "ok": page_ok,
                // failed：页面明确报错（脚本异常 / 注入被拒）；unconfirmed：超时或标签已断连。
                "failed": page_failed.clone(),
                "failed_count": page_failed.len(),
                "unconfirmed": page_unconfirmed.clone(),
                "unconfirmed_count": page_unconfirmed.len(),
            },
            "cdp_stopped": cdp_stopped,
            "cdp": cdp_result,
            "hook_removed": hook_removed,
            "hook_removed_ok": hook_ok,
            // 没停下来时配置被刻意保留，才能再试一次；如实告诉调用方。
            "config_removed": cdp_stopped,
        });
        if !reset_ok {
            if !cdp_stopped {
                out["error"] =
                    json!("网络层拦截未能在所有标签上停止，已保留配置以便重试（见 cdp.failed）");
                out["hint"] = json!("先 `atb net reset --engine cdp` 重试；持续失败时用 `atb browsers` 确认目标浏览器是否还连着");
            } else if !hook_ok {
                // 旧注入仍留在扩展存储里：下一次导航会重装已 reset 的规则，必须让命令失败。
                out["error_code"] = json!("hook_remove_failed");
                out["error"] = json!(
                    "页面包装与 CDP 拦截已停止，但持久注入删除失败（见 hook_removed）；\
                     该注入会在下次导航时重新生效"
                );
                out["hint"] = json!(
                    "先 `atb net hook --off`（或重试 `atb net reset`）；持续失败时用 `atb status` \
                     确认扩展已连接，必要时重载扩展后重试"
                );
            } else {
                let failed = page_failed.len();
                out["error"] = json!(format!(
                    "CDP 拦截已停止，但有 {failed} 个标签页面脚本报错（见 page_reset.failed）"
                ));
                out["hint"] = json!(
                    "常见原因：页面禁 eval / 跨扩展 iframe；对这些标签可重试 `atb net reset --tab <id>`"
                );
            }
        }
        // 成功但有标签没回应：给出解释，别让调用方以为「全都处理干净了」。
        if reset_ok && !page_unconfirmed.is_empty() {
            out["hint"] = json!(format!(
                "{} 个标签未确认（多为休眠/无响应），它们的页面包装会在下次导航时自然失效",
                page_unconfirmed.len()
            ));
        }
        return Ok(out);
    }

    // 网络层拦截：规则存在 profile 上，逐标签下发（Fetch 是 per-tab 的）。
    if engine == "cdp" && matches!(action.as_str(), "route" | "mock" | "unroute") {
        let payload =
            net_cdp_action(state, &profile_key, &action, &opts, &mut config, timeout).await?;
        store_net_config_locked(state, &profile_key, &config).await;
        return Ok(json!({
            "tab_id": tab_id_of_session(&session_key),
            "session_key": session_key,
            "profile": profile_key,
            "engine": engine,
            "net": payload,
        }));
    }

    // 页面级：持久化模式下，hook 需要带上已有规则，导航后才不会丢。
    if action == "hook" && req.persist {
        opts["routes"] = config.get("routes").cloned().unwrap_or_else(|| json!([]));
    }
    // log 要合并页面侧与 CDP 侧两份记录，页面侧先多取一些，合并排序后再截断，
    // 否则会先被页面脚本按 limit 砍掉一半，合并结果不完整。
    let log_limit = req.limit.unwrap_or(50).clamp(1, 500);
    let mut cdp_debug_pass_sync: Option<Value> = None;
    if action == "log" {
        opts["limit"] = json!((log_limit * 2).min(500));
        // 线协议字段是 camelCase：扩展侧/CDP 日志读的是 `debugPass`。
        opts["debugPass"] = json!(req.debug_pass.unwrap_or(false));
        // engine=cdp 只读扩展侧，别顺手把页面侧日志清掉。
        if log_source == "cdp" {
            if let Some(object) = opts.as_object_mut() {
                object.remove("clear");
            }
        }
        // CDP 侧的 pass 计数由扩展的 per-tab 开关控制，必须在查 status 之前
        // 同步过去，否则 `net log --debug-pass` 打开后仍然一条 pass 都看不到。
        //
        // 每次都要下发**最终值**：旧实现只在 `--debug-pass` 为 true 时
        // 同步，于是开关只能开不能关，pass 日志越积越多、也无法复位。
        let debug_pass = req.debug_pass.unwrap_or(false);
        if log_source != "page" && config.get("cdpEnabled").and_then(Value::as_bool) == Some(true) {
            cdp_debug_pass_sync = Some(
                net_cdp_set_debug_pass(state, &profile_key, &config, timeout, debug_pass).await,
            );
        }
    }
    // 页面引擎与 CDP 引擎共用同一套条数上限（每 profile 默认 64 条）。
    // CDP 分支已在上面 return，这里只覆盖页面引擎；同 match+method 是原位替换，
    // 不占新名额，所以只对「新增」报 too_many_rules。
    if matches!(action.as_str(), "route" | "mock") {
        ensure_page_rule_capacity(&config, &action, opts.get("rule"))?;
    }

    // 修改页面级规则的动作要下发到该 profile 的**每个**标签：配置按
    // `profile_key` 存，但规则实际生效在页面里。旧实现只发当前标签，
    // `net unroute X` 在 B 标签返回 `removed:1, ok:true`，A 标签却仍在 mock X。
    let (page_payload, page_failures) = if action == "log" && log_source == "cdp" {
        (
            json!({ "ok": true, "action": "log", "items": [] }),
            Vec::new(),
        )
    } else if matches!(action.as_str(), "route" | "unroute" | "mock") {
        net_page_broadcast(
            state,
            &profile_key,
            &session_key,
            &opts,
            timeout,
            req.frame_id,
        )
        .await?
    } else {
        (
            net_page_call_bounded(state, &session_key, &opts, timeout, req.frame_id).await?,
            Vec::new(),
        )
    };
    let mut payload = if action == "log" {
        merge_net_log(
            state,
            &session_key,
            &config,
            page_payload,
            log_limit,
            req.clear,
            req.pattern.as_deref(),
            &log_source,
            timeout,
        )
        .await
    } else {
        page_payload
    };

    // 持久化：记录配置，并把「页面脚本 + 当前规则」注册成 document_start 注入。
    // matches 为空时不注册（fail-closed），并把原因带回给调用方。
    let mut persist_warning: Option<String> = None;
    // 与 persist_warning 分开存：注册持久注入的 warning 会覆盖前者，
    // 但「端口未保留」这条与注册成败无关，必须两段都能看到。
    let mut match_warning: Option<String> = None;
    if action == "hook" && req.persist {
        // matches 收窄：显式 --match 就按它来；否则默认只覆盖当前页面所属站点，
        // 而不是给所有网站注册 content script。已有配置取并集，跨站点累积。
        let matches = if let Some(explicit) = req.matches.clone().filter(|list| !list.is_empty()) {
            explicit
        } else {
            let mut merged: Vec<String> = config
                .get("matches")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .filter(|item| *item != "<all_urls>")
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if let Some((pattern, port_warning)) =
                current_site_match_pattern(state, &session_key).await
            {
                if !merged.contains(&pattern) {
                    merged.push(pattern);
                }
                // 端口无法写进 match pattern：如实告警，别让调用方以为
                // `--persist` 只影响了当前端口。
                if let Some(warning) = port_warning {
                    match_warning = Some(warning);
                }
            }
            // 不再退回 <all_urls>：提取不到站点就保持空列表，由下面跳过注册。
            merged
        };
        let matches_available = !matches.is_empty();
        if !matches_available {
            persist_warning = Some(
                "无法从当前页面提取站点（只支持 http/https），未注册持久化注入；\
                 不会退回 <all_urls>，请用 --match 显式指定要覆盖的站点"
                    .to_string(),
            );
        }
        // 没有可用 matches 时 persist 必须是 false，否则 daemon 显示「已持久化」
        // 而扩展侧根本没有注入，daemon / 磁盘 / 扩展三方状态会再次失同步。
        config["persist"] = json!(matches_available);
        config["matches"] = json!(matches);
        // 只有显式给了才覆盖。第二次 `net hook --persist` 通常只想刷新 matches，
        // 旧实现用 `unwrap_or(false)` / `unwrap_or(200)` 把已存的开关静默压回默认值，
        // 用户看到的是「body 抓不到了 / 日志只剩 200 条」却找不到改过什么。
        match req.capture_bodies {
            Some(capture) => config["captureBodies"] = json!(capture),
            None => {
                if config.get("captureBodies").is_none() {
                    config["captureBodies"] = json!(false);
                }
            }
        }
        match req.max_log {
            // 与页面 opts 路径（上方 `opts["maxLog"]`）用同一个区间：`--max-log 1`
            // 会让日志几乎不记录，`--max-log 1000000` 则把持久注入里整份日志撑爆，
            // 两条路径的钳制必须一致。
            Some(max_log) => config["maxLog"] = json!(max_log.clamp(10, 5000)),
            None => {
                if config.get("maxLog").is_none() {
                    config["maxLog"] = json!(200);
                }
            }
        }
        // hook 分支的回包里 `routes` 是「条数」（历史字段，兼容旧调用方），
        // 真实规则在 `routes_list`。把数字存进 config 会在下次 install_net_hook 时
        // 被塞回 `opts.routes`，页面侧 `Array.isArray` 判断失败 → 规则静默丢失。
        config["routes"] = payload
            .get("routes_list")
            .or_else(|| payload.get("routes").filter(|value| value.is_array()))
            .cloned()
            .unwrap_or_else(|| json!([]));
        store_net_config_locked(state, &profile_key, &config).await;
        if matches_available {
            // 注册持久注入同样会等会话锁，必须给总超时（见 `net_page_call_bounded`）。
            let installed = match tokio::time::timeout(
                timeout,
                install_net_hook(state, &session_key, &profile_key, &config),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(anyhow!(
                        "net hook: 注册持久注入超时（{}s，含等待会话锁）；规则已保存，可稍后重试",
                        timeout.as_secs()
                    ))
                }
            };
            if let Some(warning) = installed.get("warning").and_then(Value::as_str) {
                persist_warning = Some(warning.to_string());
            }
        } else {
            // 这次注册不上了，但**上一次**的持久注入可能还在：旧实现只把
            // persist 置 false 就走人，旧 hook 每次匹配页面都重装旧 mock，
            // 而 reset 又按 persist 判断是否要移除 → CLI 再无手段清除。
            // 现在拿不到站点就把残留注入清掉，并在 warning 里说明。
            let removed = match remove_net_hook(state, &session_key, &profile_key).await {
                Ok(value) => value,
                Err(err) => json!({
                    "ok": false,
                    "error": humanize_extension_error(&err.to_string()),
                }),
            };
            if let Some(warning) = persist_warning.as_mut() {
                warning.push_str(&format!(
                    "；已尝试移除旧的持久注入（{}）",
                    if removed.get("ok").and_then(Value::as_bool) == Some(false) {
                        "失败，见 hook_removed"
                    } else {
                        "成功"
                    }
                ));
            }
            if let Some(object) = payload.as_object_mut() {
                object.insert("hook_removed".to_string(), removed);
            }
        }
    } else if matches!(action.as_str(), "route" | "unroute" | "mock") {
        if let Some(all) = payload.get("all") {
            config["routes"] = all.clone();
            store_net_config_locked(state, &profile_key, &config).await;
        }
        // 规则变了就同步到持久化注入，刷新后仍然是同一套规则。
        if config.get("persist").and_then(Value::as_bool) == Some(true) {
            // 同上：持着该 profile 的落盘锁时不能无界等待会话锁。
            let installed = match tokio::time::timeout(
                timeout,
                install_net_hook(state, &session_key, &profile_key, &config),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(anyhow!(
                        "net: 同步持久注入超时（{}s，含等待会话锁）；规则已保存，可稍后重试",
                        timeout.as_secs()
                    ))
                }
            };
            if let Some(warning) = installed.get("warning").and_then(Value::as_str) {
                persist_warning = Some(warning.to_string());
            }
        }
    }

    // 的观测缺口：页面侧重放只保留前 `MAX_ROUTES`（64）条，丢弃条数由页面在
    // `routes_dropped` 里回报。daemon 以前完全不读它 —— 用户把 `ATB_NET_RULE_MAX_RULES`
    // 放大到 64 以上、堆了 100+ 条规则时，`net routes` 显示全量、浏览器只生效前 64 条，
    // 却没有任何提示。
    // 先在 `payload` 被移进 `result` 之前取出来。
    let dropped_note = routes_dropped_note(&payload);
    let mut result = json!({
        "tab_id": tab_id_of_session(&session_key),
        "session_key": session_key,
        "profile": profile_key,
        // log 时回报实际使用的日志来源（page / cdp / both），其它动作回报拦截引擎。
        "engine": if action == "log" { log_source.as_str() } else { engine.as_str() },
        "net": payload,
    });
    if let Some(note) = clamped_note {
        result["clamped"] = note;
    }
    // debugPass 同步失败不能静默：否则用户以为开关已经改过，日志口径却是旧的。
    if let Some(sync) =
        cdp_debug_pass_sync.filter(|value| value.get("ok").and_then(Value::as_bool) == Some(false))
    {
        result["cdp_debug_pass"] = sync;
    }
    // 部分标签没收到规则时必须如实报出来：否则"当前标签成功了"会被读成全部生效。
    if !page_failures.is_empty() {
        result["page_failed"] = json!(page_failures);
    }
    if let Some(warning) = match_warning {
        match persist_warning.as_mut() {
            Some(existing) => {
                existing.push('；');
                existing.push_str(&warning);
            }
            None => persist_warning = Some(warning),
        }
    }
    // 页面重放裁剪出的丢条数与告警一并透出（只增不改）：顶层 `routes_dropped` +
    // 合并进 `warning`，调用方不必去翻 `net.*` 的页面回包也能发现「只生效了前 64 条」。
    if let Some((dropped, note)) = dropped_note {
        result["routes_dropped"] = json!(dropped);
        match persist_warning.as_mut() {
            Some(existing) => {
                existing.push('；');
                existing.push_str(&note);
            }
            None => persist_warning = Some(note),
        }
    }
    if let Some(warning) = persist_warning {
        result["warning"] = json!(warning);
    }
    Ok(result)
}

pub(crate) mod cdp;
pub(crate) mod hook;
pub(crate) mod log;
pub(crate) mod page;
pub(crate) mod settings;

pub(crate) use cdp::*;
pub(crate) use hook::*;
pub(crate) use log::*;
pub(crate) use page::*;
pub(crate) use settings::*;

/// 把 `net_page_broadcast` 的失败分成「明确报错」与「未确认」。
///
/// - 明确报错：页面脚本抛出 / 注入被拒 / CSP 拦截 —— reset 真的没做成；
/// - 未确认：执行超时、标签已断连 —— 没回应，不等于失败（休眠标签很常见）。
fn split_reset_failures(failures: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let mut failed = Vec::new();
    let mut unconfirmed = Vec::new();
    for item in failures {
        let text = item
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let transient = text.contains("timeout")
            || text.contains("超时")
            || text.contains("未连接")
            || text.contains("not connected")
            || text.contains("没有可操作");
        if transient {
            unconfirmed.push(item.clone());
        } else {
            failed.push(item.clone());
        }
    }
    (failed, unconfirmed)
}

#[cfg(test)]
mod net_outcome_tests {
    use super::*;

    // v11 契约：`net reset` 的 ok 必须同时覆盖页面 reset、CDP stop、hook 删除。
    /// 旧实现漏掉第三项 —— 持久注入删不掉时报成功，旧 mock 下次导航复活。
    #[test]
    fn reset_requires_hook_removal() {
        let removed = json!({ "ok": true, "data": { "removed": true } });
        let failed = json!({ "ok": false, "error": "extension disconnected" });
        let unknown = json!({ "error": "unexpected payload" });

        assert!(reset_outcome_ok(true, true, &removed));
        assert!(!reset_outcome_ok(true, true, &failed));
        // 回包认不出成败时按失败处理：不能拿「不知道」冒充「删掉了」。
        assert!(!reset_outcome_ok(true, true, &unknown));
        assert!(!reset_outcome_ok(false, true, &removed));
        assert!(!reset_outcome_ok(true, false, &removed));
    }

    /// 的观测缺口：页面重放的 `routes_dropped` 必须能变成顶层字段 + 告警，
    /// 否则 env 放大上限后「配置 N 条、实际生效 64 条」没有任何提示。
    #[test]
    fn dropped_routes_are_surfaced() {
        let dropped = json!({ "ok": true, "action": "hook", "routes_dropped": 6 });
        let (count, note) = routes_dropped_note(&dropped).expect("有丢弃时必须产生告警");
        assert_eq!(count, 6);
        assert!(note.contains("64"), "{note}");
        assert!(note.contains('6'), "{note}");

        // 页面日后回 `routes_cap` 时文案自动跟随，不需要再动 Rust（单一真相源在页面侧）。
        let (_, note) = routes_dropped_note(&json!({ "routes_dropped": 2, "routes_cap": 128 }))
            .expect("有丢弃");
        assert!(note.contains("128"), "{note}");

        // 没丢 / 页面没回该字段：不产生噪音。
        assert!(routes_dropped_note(&json!({ "ok": true, "routes_dropped": 0 })).is_none());
        assert!(routes_dropped_note(&json!({ "ok": true })).is_none());
    }
}
