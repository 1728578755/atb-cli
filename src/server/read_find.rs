use super::*;

pub(crate) async fn read_page(
    State(state): State<AppState>,
    Json(req): Json<ReadRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(read_value(&state, req).await))
}

/// 结构化读取页面：一次注入返回正文、标题、元信息、表单与计数。
pub(crate) async fn read_value(state: &AppState, req: ReadRequest) -> Result<Value> {
    let include_hidden = req.include_hidden
        || req
            .mode
            .as_deref()
            .map(|mode| mode.eq_ignore_ascii_case("all") || mode.eq_ignore_ascii_case("hidden"))
            .unwrap_or(false);
    // clamp 只用来兜住极端值；实际值与请求值不同时必须显式告诉调用方。
    let applied_max_chars = req.max_chars.clamp(1_000, 400_000);
    let clamped_note = (applied_max_chars != req.max_chars).then(|| {
        json!({
            "field": "maxChars",
            "requested": req.max_chars,
            "applied": applied_max_chars,
            "min": 1_000,
            "max": 400_000,
        })
    });
    let mut opts = json!({
        "selector": req.selector,
        "maxChars": applied_max_chars,
        "links": req.links,
        "forms": req.forms,
        "includeHidden": include_hidden,
        "budgetMs": req.budget_ms,
        "maxNodes": req.max_nodes,
    });
    // `innerTextLimit` 是 page_read.js 的**节点数**阈值（页面侧默认 30000），
    // 不是字符数：节点数 <= 该值时用 innerText 精确抽取，超过则退化为近似可见
    // 文本（textMode:'visible-approx'）。此前 daemon 侧根本没有下发通道，
    // CLI 传什么都不会生效（read 侧）。
    // - None 不下发，保持页面默认；
    // - `0` 视为「未指定」而不是「最小上限」：页面脚本的约定就是 <=0 用默认
    //   （见 page_read.js 的参数约定注释），把它夹成 1000 会让「顺手写 0」的
    //   调用方在不知情的情况下切到激进抽取路径；
    // - 正数 clamp 到 [1000, 500000]：太小会让正常长页面直接走近似分支、
    //   太大则可能把几 MB 文本一次性塞进回包。
    if let Some(limit) = req.inner_text_limit.filter(|limit| *limit > 0) {
        opts["innerTextLimit"] = json!(limit.clamp(1_000, 500_000));
    }
    let script = format!("{}\nreturn __abcRead({});", html::js_page_read(), opts);
    let timeout = secs(req.timeout.unwrap_or(30.0).clamp(1.0, 120.0));
    let target = ExecTargetOptions {
        frame_id: req.frame_id,
        ..Default::default()
    };
    let selector = SessionSelector::new(
        req.switch_tab_id.clone(),
        req.browser.clone(),
        req.profile.clone(),
    );
    let executed =
        execute_page_js_with_target(state, &script, selector.clone(), true, timeout, &target).await;
    match executed {
        Ok(result) => {
            // v11：页面脚本明确回 `ok:false`（非法选择器 / 找不到文档）时，
            // 旧实现照样包成顶层 `ok:true`，CLI 以退出码 0 结束 —— 调用方以为读到了。
            ensure_page_result_ok(&result, "read")?;
            let mut payload = page_payload(result, "page");
            // 多浏览器并存且本次走隐式默认时，在顶层说明「默认落在哪个浏览器」。
            attach_multi_browser_hint(state, &mut payload, &selector).await;
            if let Some(note) = &clamped_note {
                payload["clamped"] = note.clone();
            }
            attach_challenge_if_blocked(state, &mut payload).await;
            Ok(payload)
        }
        Err(err) if is_csp_error(&err.to_string()) => {
            let session_key = select_tab(state, selector.clone()).await?;
            let page = page_op_via_extension(
                state,
                &session_key,
                "read",
                opts.clone(),
                req.frame_id,
                timeout,
            )
            .await?;
            let mut payload = json!({
                "tab_id": tab_id_of_session(&session_key),
                "session_key": session_key,
                "page": page,
                "via": "no-eval",
            });
            // 同一条 read 的降级路径也要给同样的提示（顶层附加字段）。
            attach_multi_browser_hint(state, &mut payload, &selector).await;
            if let Some(note) = &clamped_note {
                payload["clamped"] = note.clone();
            }
            attach_challenge_if_blocked(state, &mut payload).await;
            Ok(payload)
        }
        Err(err) => Err(err),
    }
}

pub(crate) async fn find_page(
    State(state): State<AppState>,
    Json(req): Json<FindRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(find_value(&state, req).await))
}

/// 元素查找：CSS / 文本 / XPath，返回可操作信息与简短 CSS 路径。
pub(crate) async fn find_value(state: &AppState, req: FindRequest) -> Result<Value> {
    if req.selector.is_none() && req.text.is_none() && req.xpath.is_none() {
        return Err(anyhow!("find: selector / text / xpath 至少需要一个"));
    }
    let spec = json!({
        "selector": req.selector,
        "text": req.text,
        "xpath": req.xpath,
        "limit": req.limit.clamp(1, 200),
        "visible": req.visible,
        "exact": req.exact,
    });
    let script = format!("{}\nreturn __abcFind({});", html::js_page_find(), spec);
    let timeout = secs(req.timeout.unwrap_or(20.0).clamp(1.0, 120.0));
    let target = ExecTargetOptions {
        frame_id: req.frame_id,
        ..Default::default()
    };
    let executed = execute_page_js_with_target(
        state,
        &script,
        SessionSelector::new(
            req.switch_tab_id.clone(),
            req.browser.clone(),
            req.profile.clone(),
        ),
        true,
        timeout,
        &target,
    )
    .await;
    match executed {
        Ok(result) => {
            // 同上：find 的 invalid_selector / invalid_xpath 必须变成失败，
            // 而不是「0 个匹配」的成功。
            ensure_page_result_ok(&result, "find")?;
            Ok(page_payload(result, "find"))
        }
        Err(err) if is_csp_error(&err.to_string()) => {
            let session_key = select_tab(
                state,
                SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
            )
            .await?;
            let found = page_op_via_extension(
                state,
                &session_key,
                "find",
                spec.clone(),
                req.frame_id,
                timeout,
            )
            .await?;
            Ok(json!({
                "tab_id": tab_id_of_session(&session_key),
                "session_key": session_key,
                "find": found,
                "via": "no-eval",
            }))
        }
        Err(err) => Err(err),
    }
}

pub(crate) async fn action(
    State(state): State<AppState>,
    Json(req): Json<ActionRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(json_result(action_value(&state, req).await))
}

/// 单往返动作：hover / click / focus / select / check / uncheck / scroll / setValue。
pub(crate) async fn action_value(state: &AppState, req: ActionRequest) -> Result<Value> {
    let params = if req.params.is_null() {
        json!({})
    } else {
        req.params
    };
    let timeout = secs(req.timeout.unwrap_or(15.0).clamp(1.0, 120.0));
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let action = page_action_with_fallback(
        state,
        &session_key,
        &req.op,
        req.target.unwrap_or(Value::Null),
        params,
        timeout,
        req.frame_id,
    )
    .await?;
    ensure_page_action_ok(&action)?;
    Ok(json!({
        "tab_id": session_key.rsplit(':').next().unwrap_or_default(),
        "session_key": session_key,
        "action": action,
    }))
}

/// 在指定会话里执行一次页面动作，返回 `__abcAct` 的原始结果。
pub(crate) async fn run_page_action(
    state: &AppState,
    session_key: &str,
    op: &str,
    target: Value,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    run_page_action_in_frame(state, session_key, op, target, params, timeout, None).await
}

pub(crate) async fn run_page_action_in_frame(
    state: &AppState,
    session_key: &str,
    op: &str,
    target: Value,
    params: Value,
    timeout: Duration,
    frame_id: Option<i64>,
) -> Result<Value> {
    let script = format!(
        "{}\nreturn __abcAct({}, {}, {});",
        html::js_page_action(),
        serde_json::to_string(op)?,
        serde_json::to_string(&target)?,
        params
    );
    let target_options = ExecTargetOptions {
        frame_id,
        ..Default::default()
    };
    let result = execute_page_js_with_target(
        state,
        &script,
        SessionSelector::new(Some(session_key.to_string()), None, None),
        true,
        timeout,
        &target_options,
    )
    .await?;
    Ok(page_payload(result, "action")
        .get("action")
        .cloned()
        .unwrap_or(Value::Null))
}

pub(crate) async fn wait_page(
    State(state): State<AppState>,
    Json(req): Json<WaitRequest>,
) -> Json<Value> {
    touch(&state).await;
    // 等待超时的真实结论在内层 `ok` 上；用外层恒 true 的 `json_result` 会让
    // `atb wait --selector .never` 退出码 0。
    Json(json_result_with_inner_ok(
        wait_value(&state, req, None).await,
    ))
}

/// 等待条件成立：js / selector / text 三选一，轮询在 daemon 侧（后台标签页定时器会被节流）。
///
/// `cancel` 由异步任务传入：`--js` 轮询每轮开始前检查，selector/text 的观察器注入
/// 返回后再检查（页面里的观察器本身无法中断，只能等这一步做完）。
pub(crate) async fn wait_value(
    state: &AppState,
    req: WaitRequest,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    // `wait` 是唯一没做 clamp 的入口 —— `--interval 0` 会忙轮询把页面
    // 主线程占满，极大 / NaN 的 `--timeout` 会把 Duration 饱和成近乎无限等。
    // 统一在这里收敛，并把实际生效值回显在回包里（timeout / interval）。
    let mut req = req;
    let max_total = crate::settings::wait_max_total_sec();
    let min_interval_sec = crate::settings::wait_min_interval_ms() as f64 / 1000.0;
    req.timeout = if req.timeout.is_finite() {
        req.timeout.clamp(0.1, max_total)
    } else {
        max_total.min(30.0)
    };
    req.interval = if req.interval.is_finite() {
        req.interval.max(min_interval_sec)
    } else {
        min_interval_sec
    };
    // 失败码要区分 js 条件与 selector/text 条件（下文的 `wait_failure_code`）：
    // 两者对调用方的含义不同（一个是谓词没成立，一个是元素/文本没出现）。
    //
    // 模式优先级：js > selector > text > dom-stable > network-idle。
    // 同时给多个条件时只执行优先级最高的那个，并在回包里用 `mode_used` 说明；
    // 不做 exit 2 拒绝，避免改变「selector + text 同时给」这类旧调用的行为。
    let js_condition = req
        .js
        .as_deref()
        .filter(|js| !js.trim().is_empty())
        .map(str::to_string);
    let selector = req
        .selector
        .as_deref()
        .filter(|selector| !selector.trim().is_empty())
        .map(str::to_string);
    let text = req
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    // 0 = 用户写了不带值的 `--dom-stable` / `--network-idle`，取配置默认值；
    // 显式值夹到 [10, 60000]ms，避免 1ms 的空转与超大值把注入预算吃光。
    let dom_stable_ms = req.dom_stable.map(|requested| {
        if requested == 0 {
            crate::settings::wait_dom_stable_ms()
        } else {
            requested.clamp(10, 60_000)
        }
    });
    let network_idle_ms = req.network_idle.map(|requested| {
        if requested == 0 {
            crate::settings::screenshot_network_idle_ms()
        } else {
            requested.clamp(10, 60_000)
        }
    });
    let is_js_condition = js_condition.is_some();
    let condition = if let Some(js) = js_condition.as_deref() {
        js.to_string()
    } else if let Some(selector) = selector.as_deref() {
        if req.visible {
            format!(
                "const el = document.querySelector({}); if (!el) return false; const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';",
                serde_json::to_string(selector)?
            )
        } else {
            format!(
                "return !!document.querySelector({});",
                serde_json::to_string(selector)?
            )
        }
    } else if let Some(text) = text.as_deref() {
        if req.visible {
            // `--text --visible` 以前被静默忽略（visible 只服务 selector 分支），
            // 隐藏文案也判命中。这里直接生成「按文本节点回溯到元素、
            // 再逐层判可见」的条件：不依赖页面脚本，也不改 assets/**。
            format!(
                concat!(
                    "const __t = {needle};",
                    "const __w = document.createTreeWalker(document.body || document.documentElement, NodeFilter.SHOW_TEXT);",
                    "while (__w.nextNode()) {{",
                    "  const __n = __w.currentNode;",
                    "  if (!__n.nodeValue || __n.nodeValue.indexOf(__t) < 0) continue;",
                    "  const __el = __n.parentElement; if (!__el) continue;",
                    "  const __r = __el.getBoundingClientRect();",
                    "  const __cs = getComputedStyle(__el);",
                    "  if (__r.width > 0 && __r.height > 0 && __cs.visibility !== 'hidden' && __cs.display !== 'none' && Number(__cs.opacity) > 0) return true;",
                    "}}",
                    "return false;"
                ),
                needle = serde_json::to_string(text)?
            )
        } else {
            format!(
                "return ((document.body && document.body.innerText) || '').includes({});",
                serde_json::to_string(text)?
            )
        }
    } else if let Some(ms) = dom_stable_ms {
        format!("dom-stable: {ms}ms 内无 DOM 变更")
    } else if let Some(ms) = network_idle_ms {
        format!("network-idle: 在途 fetch/XHR 归零并静默 {ms}ms")
    } else {
        return Err(anyhow!(
            "wait: 需要 --js / --selector / --text / --dom-stable / --network-idle 之一"
        ));
    };
    // 本次实际使用的模式（与 page_wait.js 的优先级保持一致）。
    let mode_used = if is_js_condition {
        "js"
    } else if selector.is_some() {
        "selector"
    } else if text.is_some() {
        "text"
    } else if dom_stable_ms.is_some() {
        "dom-stable"
    } else {
        "network-idle"
    };

    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Ok(json!({
            "condition": condition,
            "matched": false,
            "ok": false,
            "cancelled": true,
            "error": "cancelled",
            "mode": "cancelled",
            "mode_used": mode_used,
        }));
    }

    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let started = Instant::now();

    // selector / text / dom-stable / network-idle 这类「页面内观察」条件：
    // 一次注入 + 页面内观察器完成，不再每 100ms 注入一次；后台标签页里
    // MutationObserver 不受定时器节流影响（network-idle 的轮询计时器会被节流，
    // 只会让判定偏晚，不会把仍活动的页面误判为空闲）。
    if !is_js_condition {
        let mut opts = json!({
            "selector": selector,
            "text": text,
            "visible": req.visible,
            "timeoutMs": (req.timeout.max(0.1) * 1000.0) as u64,
            // 页面的巡检节流窗口：不传时 page_wait.js 会退回 50ms 默认值，
            // `wait --interval 1` 这种显式设置就被静默忽略了。
            "minIntervalMs": (req.interval * 1000.0).max(0.0) as u64,
        });
        if let Some(ms) = dom_stable_ms {
            opts["domStableMs"] = json!(ms);
        }
        if let Some(ms) = network_idle_ms {
            opts["networkIdleMs"] = json!(ms);
        }
        let script = format!(
            "{}\nreturn await __abcWait({});",
            html::js_page_wait(),
            opts
        );
        let timeout = secs(req.timeout.max(0.1) + 5.0);
        let target = ExecTargetOptions {
            frame_id: req.frame_id,
            ..Default::default()
        };
        // 观察器是「一次注入、页面里等几十秒」——注入完就放锁。
        // 旧实现让整段等待都占着会话串行锁：一个 `wait --selector .never`
        // （默认 30s）能把同一标签上的 read / click 全部堵到超时，
        // 而等待本身并不需要串行。锁不在手上，超时/取消也能尽快返回。
        let executed = execute_page_js_with_target_releasing_lock(
            state,
            &script,
            SessionSelector::new(Some(session_key.clone()), None, None),
            true,
            timeout,
            &target,
        )
        .await;
        let outcome = match executed {
            Ok(result) => result.get("js_return").cloned().unwrap_or(Value::Null),
            Err(err) if is_csp_error(&err.to_string()) => {
                page_op_via_extension(
                    state,
                    &session_key,
                    "wait",
                    opts.clone(),
                    req.frame_id,
                    timeout,
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        // 观察器注入是单次往返，取消只能在这一步返回后才生效。
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(json!({
                "tab_id": tab_id_of_session(&session_key),
                "session_key": session_key,
                "condition": condition,
                "matched": false,
                "ok": false,
                "cancelled": true,
                "error": "cancelled",
                "mode": "observer",
                "mode_used": mode_used,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }));
        }
        let mut body = json!({
            "tab_id": tab_id_of_session(&session_key),
            "session_key": session_key,
            "condition": condition,
            // 回显实际生效的预算（可能已被 clamp）。
            "timeout": req.timeout,
            "interval": req.interval,
            "matched": outcome.get("matched").cloned().unwrap_or(Value::Bool(false)),
            "ok": outcome.get("matched").cloned().unwrap_or(Value::Bool(false)),
            "polls": outcome.get("checks").cloned().unwrap_or(Value::Null),
            "value": outcome.get("value").cloned().unwrap_or(Value::Null),
            "error": outcome.get("error").cloned().unwrap_or(Value::Null),
            "mode": outcome.get("mode").cloned().unwrap_or(json!("observer")),
            // 说明本次实际执行的是哪个条件（多条件同时给时按优先级取一个）。
            "mode_used": outcome.get("mode_used").cloned().unwrap_or(json!(mode_used)),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        // dom-stable / network-idle 的诊断字段，以及 --text 的取样方式。
        // 白名单重建：页面没给的字段保持“不出现”，不要用 null 占位。
        for key in [
            "reason",
            "stable_ms",
            "mutations",
            "last_mutation_ms",
            "idle_ms",
            "inflight",
            "hook_installed",
            "note",
            "text_match_mode",
        ] {
            if let Some(value) = outcome.get(key).filter(|value| !value.is_null()) {
                body[key] = value.clone();
            }
        }
        // 新模式超时页面只给 reason，没有 error；补一个稳定文本与 error_code，
        // 让 `wait --dom-stable` / `--network-idle` 的失败在 CLI 侧可分支。
        if body.get("ok").and_then(Value::as_bool) == Some(false)
            && body.get("error").is_none_or(Value::is_null)
        {
            body["error"] = json!("timeout");
        }
        if body.get("error_code").is_none()
            && body.get("ok").and_then(Value::as_bool) == Some(false)
        {
            body["error_code"] = json!(wait_failure_code(is_js_condition));
        }
        return Ok(body);
    }

    let target = ExecTargetOptions {
        frame_id: req.frame_id,
        ..Default::default()
    };
    let outcome = wait_js_condition_with_history(
        state,
        Some(&session_key),
        &condition,
        req.timeout,
        req.interval,
        cancel,
        &target,
    )
    .await?;
    let mut body = json!({
        "tab_id": session_key.rsplit(':').next().unwrap_or_default(),
        "session_key": session_key,
        "condition": condition,
        // 回显实际生效的预算（可能已被 clamp）。
        "timeout": req.timeout,
        "interval": req.interval,
        "matched": outcome.get("matched").cloned().unwrap_or(Value::Bool(false)),
        "ok": outcome.get("ok").cloned().unwrap_or(Value::Bool(false)),
        "polls": outcome.get("polls").cloned().unwrap_or(Value::Null),
        "checks": outcome.get("checks").cloned().unwrap_or(Value::Null),
        "injections": outcome.get("injections").cloned().unwrap_or(Value::Null),
        "value": outcome.get("value").cloned().unwrap_or(Value::Null),
        "error": outcome.get("error").cloned().unwrap_or(Value::Null),
        "cancelled": outcome.get("cancelled").cloned().unwrap_or(Value::Bool(false)),
        "mode": outcome.get("mode").cloned().unwrap_or(json!("js-observer")),
        "mode_used": outcome.get("mode_used").cloned().unwrap_or(json!("js")),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    // `wait --js` 超时的最近 5 次求值（值或错误）。CDP 回退路径不记录
    // 历史时也给空数组，保证超时回包的字段形状稳定。
    if let Some(history) = outcome.get("last_values").filter(|value| !value.is_null()) {
        body["last_values"] = history.clone();
    } else if body.get("ok").and_then(Value::as_bool) == Some(false) {
        body["last_values"] = json!([]);
    }
    // 同一套白名单重建：页面对 --text 的降级说明不能在这里被吞掉。
    if let Some(mode) = outcome
        .get("text_match_mode")
        .filter(|value| !value.is_null())
    {
        body["text_match_mode"] = mode.clone();
    }
    // 结构化错误码原样上提（起 wait 也会产出 `wait_js_eval_blocked` /
    // `wait_js_failed`）：CLI 只认 `error_code` 分支，别让等待失败退化成纯文本。
    if let Some(code) = outcome.get("error_code").filter(|value| !value.is_null()) {
        body["error_code"] = code.clone();
    }
    // 页面通道（非 CSP 回退）超时本身不带码：这里按条件类型补一个稳定码，
    // 让 `wait --js` / `wait --selector|--text` 的失败在 CLI 侧可分支。
    if body.get("error_code").is_none()
        && body.get("ok").and_then(Value::as_bool) == Some(false)
        && body.get("cancelled").and_then(Value::as_bool) != Some(true)
    {
        body["error_code"] = json!(wait_failure_code(is_js_condition));
    }
    Ok(body)
}

// `wait --js` 的页面内观察器等待，额外收集最近 5 次求值。
///
/// 与 [`wait_for_js_condition_with_target`] 的轮询/切片逻辑一致，差别只有两点：
/// - 每次注入要求页面脚本记录 `last_values`（值或错误的安全摘要）；
/// - 页面通道因 CSP 无法编译字符串谓词、或注入本身失败时，把剩余预算交回原
///   helper（它负责 CDP Runtime.evaluate 回退与重试），并把已收集的历史并回结果。
async fn wait_js_condition_with_history(
    state: &AppState,
    session_key: Option<&str>,
    wait_js: &str,
    timeout: f64,
    interval: f64,
    cancel: Option<&AtomicBool>,
    target: &ExecTargetOptions,
) -> Result<Value> {
    const WAIT_JS_EVAL_BLOCKED_CODE: &str = "wait_js_eval_blocked";
    let max_total = crate::settings::wait_max_total_sec();
    let min_interval_sec = crate::settings::wait_min_interval_ms() as f64 / 1000.0;
    let timeout = if timeout.is_finite() {
        timeout.clamp(0.0, max_total)
    } else {
        0.0
    };
    let interval = if interval.is_finite() {
        interval.max(min_interval_sec)
    } else {
        min_interval_sec
    };
    let interval_ms = (interval * 1000.0).round().max(1.0) as u64;
    let deadline = Instant::now() + Duration::from_millis((timeout * 1000.0).max(1.0) as u64);
    let started = Instant::now();
    let mut injections: u64 = 0;
    let mut checks_total: u64 = 0;
    let mut last_value = Value::Null;
    let mut last_error: Option<String> = None;
    let mut history: Vec<Value> = Vec::new();
    let mut quick_returns: u32 = 0;

    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(json!({
                "ok": false,
                "matched": false,
                "cancelled": true,
                "value": last_value,
                "error": "cancelled",
                "polls": checks_total.max(injections),
                "checks": checks_total,
                "mode": "js-observer",
                "mode_used": "js",
                "injections": injections,
                "last_values": history,
            }));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // 可取消时切成 ≤1s 的片，让 cancel 能在片间生效；同步等待整段注入。
        let slice_ms = if cancel.is_some() {
            (remaining.as_millis() as u64).clamp(1, 1_000)
        } else {
            (remaining.as_millis() as u64).max(1)
        };
        injections += 1;
        let injection_offset = started.elapsed().as_millis() as u64;
        let opts = json!({
            "js": wait_js,
            "timeoutMs": slice_ms,
            // 键名必须是 minIntervalMs（与 page_wait.js 的约定一致）。
            "minIntervalMs": interval_ms,
            // 页面只保留最近 5 次求值；daemon 这里再裁一次，防止旧页面脚本多给。
            "lastValuesLimit": 5,
        });
        let script = format!(
            "{}\nreturn await __abcWait({});",
            html::js_page_wait(),
            opts
        );
        let remaining_now = deadline.saturating_duration_since(Instant::now());
        if remaining_now.is_zero() {
            break;
        }
        let budget_ms = remaining_now.as_millis() as u64;
        let exec_timeout =
            Duration::from_millis((slice_ms.saturating_add(3_000)).min(budget_ms.max(1)));
        match execute_raw_js_releasing_lock(state, &script, exec_timeout, session_key, target).await
        {
            Ok(execution) => {
                let payload = execution
                    .result
                    .data
                    .or(execution.result.result)
                    .unwrap_or(Value::Null);
                let checks = payload.get("checks").and_then(Value::as_u64).unwrap_or(0);
                checks_total += checks.max(1);
                merge_wait_history(&mut history, payload.get("last_values"), injection_offset);
                // 页面 CSP 拦下字符串谓词的编译：交给原 helper 走 CDP 回退。
                if payload.get("error_code").and_then(Value::as_str)
                    == Some(WAIT_JS_EVAL_BLOCKED_CODE)
                {
                    return delegate_js_wait_with_history(
                        state,
                        session_key,
                        wait_js,
                        deadline,
                        interval,
                        cancel,
                        target,
                        history,
                    )
                    .await;
                }
                if payload
                    .get("matched")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    let mut result = json!({
                        "ok": true,
                        "matched": true,
                        "value": payload.get("value").cloned().unwrap_or(Value::Null),
                        "error": Value::Null,
                        "polls": checks_total,
                        "checks": checks_total,
                        "mode": payload.get("mode").cloned().unwrap_or(json!("js-observer")),
                        "mode_used": "js",
                        "injections": injections,
                        "elapsed_ms": payload.get("elapsed_ms").cloned().unwrap_or(Value::Null),
                    });
                    if !history.is_empty() {
                        result["last_values"] = json!(history);
                    }
                    return Ok(result);
                }
                if let Some(value) = payload.get("value") {
                    last_value = value.clone();
                }
                if let Some(err) = payload.get("error").and_then(Value::as_str) {
                    last_error = Some(err.to_string());
                }
                if Instant::now() >= deadline {
                    break;
                }
                // 同步调用是「整段注入」，本片未命中就等于整体超时（保持旧行为）。
                if cancel.is_none() {
                    break;
                }
                // 页面几乎没等就返回（观察器没装起来之类）时别热转。
                let page_elapsed_ms = payload
                    .get("elapsed_ms")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let quick = slice_ms >= 200 && page_elapsed_ms < (slice_ms as f64) * 0.5;
                quick_returns = if quick { quick_returns + 1 } else { 0 };
                if quick_returns >= 3 {
                    break;
                }
            }
            Err(_err) => {
                // 注入失败（换页 / 上下文销毁 / CSP 被升级成命令错误）：
                // 剩余预算交给原 helper，它已有重试与 CDP 回退逻辑。
                return delegate_js_wait_with_history(
                    state,
                    session_key,
                    wait_js,
                    deadline,
                    interval,
                    cancel,
                    target,
                    history,
                )
                .await;
            }
        }
    }

    Ok(json!({
        "ok": false,
        "matched": false,
        "value": last_value,
        "error": last_error,
        "polls": checks_total.max(injections),
        "checks": checks_total,
        "mode": "js-observer",
        "mode_used": "js",
        "injections": injections,
        "last_values": history,
    }))
}

/// 把剩余预算交回原 helper（CDP 回退 / 注入重试），并保留已收集的最近求值。
#[allow(clippy::too_many_arguments)]
async fn delegate_js_wait_with_history(
    state: &AppState,
    session_key: Option<&str>,
    wait_js: &str,
    deadline: Instant,
    interval: f64,
    cancel: Option<&AtomicBool>,
    target: &ExecTargetOptions,
    history: Vec<Value>,
) -> Result<Value> {
    let remaining_secs = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64();
    let mut result = wait_for_js_condition_with_target(
        state,
        session_key,
        wait_js,
        remaining_secs,
        interval,
        cancel,
        target,
    )
    .await?;
    if !history.is_empty() {
        if let Some(object) = result.as_object_mut() {
            if object.get("last_values").is_none() {
                object.insert("last_values".to_string(), json!(history));
            }
        }
    }
    Ok(result)
}

/// 合并页面侧返回的最近求值；`offset_ms` 把「本次注入内的相对时间」换算成
/// 整条 wait 的相对时间，最多保留 5 条。
fn merge_wait_history(history: &mut Vec<Value>, incoming: Option<&Value>, offset_ms: u64) {
    let Some(items) = incoming.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let mut item = item.clone();
        if let Some(object) = item.as_object_mut() {
            let at_ms = object.get("at_ms").and_then(Value::as_u64).unwrap_or(0);
            object.insert("at_ms".to_string(), json!(at_ms.saturating_add(offset_ms)));
        }
        history.push(item);
    }
    if history.len() > 5 {
        let drop = history.len() - 5;
        history.drain(0..drop);
    }
}

/// `wait` 失败但通道没给码时的稳定兜底码。
///
/// `--js` 的失败语义是「谓词没成立」，选择器/文本的语义是「目标没出现」，
/// 调用方的处置不同（前者改条件、后者多半是页面没加载完），用两个码区分。
pub(crate) fn wait_failure_code(is_js_condition: bool) -> &'static str {
    if is_js_condition {
        "wait_js_failed"
    } else {
        "wait_timeout"
    }
}

/// 页面脚本回包的 `ok:false` 必须提升为 daemon 失败。
///
/// `read` / `find` 以前只看 `execute_page_js*` 的 `Ok`，不看页面自己的结论：
/// 非法选择器 / 文档没有 body 时页面回 `{ok:false,error_code:"invalid_selector"}`，
/// daemon 仍包成顶层 `ok:true`，CLI 退出码 0。这里把已知的用法类错误码保留下来
/// （CLI 的 `invalid_` 前缀会映射成退出码 2），未知页面错误归为 `page_op_failed`。
fn ensure_page_result_ok(result: &Value, op: &str) -> Result<()> {
    let payload = result.get("js_return").unwrap_or(result);
    match payload.get("ok").and_then(Value::as_bool) {
        Some(false) => {
            let page_code = payload
                .get("error_code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("页面脚本返回 ok:false");
            // 旧版 page_read / page_find 的选择器错误只带 message、不带 error_code
            // （`invalid selector: ...` / `invalid xpath: ...`）。这里按文案兜底归类，
            // 否则会退化成 `page_op_failed`，CLI 映射成 exit 1，Agent 会以为可重试。
            let code = match page_code {
                "invalid_selector" => "invalid_selector",
                "invalid_xpath" => "invalid_xpath",
                "selector_not_found" => "selector_not_found",
                "no_document_body" => "no_document_body",
                _ if message.trim_start().starts_with("invalid selector") => "invalid_selector",
                _ if message.trim_start().starts_with("invalid xpath") => "invalid_xpath",
                _ => "page_op_failed",
            };
            Err(SessionSelectError {
                code,
                message: format!("{op}: {message}"),
                hint: "按页面返回的 error 修正选择器/参数；find 可用 --text 或更宽的选择器，read 先确认页面已加载出 body".to_string(),
                retryable: false,
                retry_after_ms: None,
            }
            .into_anyhow())
        }
        // 没有 ok 字段：保持旧行为（部分老扩展/第三方回包可能不带），
        // 真正的「没有结论」由动作类命令的 ensure_page_action_ok 守。
        _ => Ok(()),
    }
}

#[cfg(test)]
mod page_ok_contract_tests {
    use super::*;

    // v11 契约：页面脚本回 `ok:false` 时 daemon 必须报失败，且 `error_code`
    /// 原样保留（CLI 的 `is_usage_error_code` 会把它映射成退出码 2）。旧实现无条件
    /// 当成成功，非法选择器会以 `ok:true` + exit 0 结束。
    #[test]
    fn page_ok_false_becomes_error_with_code() {
        let result = json!({
            "js_return": { "ok": false, "error_code": "invalid_selector", "error": "bad selector '>>>'" }
        });
        let err = ensure_page_result_ok(&result, "find").expect_err("ok:false 必须变成 Err");
        let typed = err
            .downcast_ref::<SessionSelectError>()
            .expect("应保留结构化的 SessionSelectError");
        assert_eq!(typed.code, "invalid_selector");
        assert!(!typed.retryable, "选择器写错不可重试");
        assert!(typed.message.starts_with("find: "), "{}", typed.message);

        let xpath = json!({ "js_return": { "ok": false, "error_code": "invalid_xpath" } });
        let err = ensure_page_result_ok(&xpath, "find").expect_err("invalid_xpath");
        assert_eq!(
            err.downcast_ref::<SessionSelectError>()
                .map(|typed| typed.code),
            Some("invalid_xpath")
        );

        // 未知页面错误码兜到 page_op_failed，仍然是失败（不再是假成功）。
        let other = json!({ "js_return": { "ok": false } });
        let err = ensure_page_result_ok(&other, "read").expect_err("未知错误也要失败");
        assert_eq!(
            err.downcast_ref::<SessionSelectError>()
                .map(|typed| typed.code),
            Some("page_op_failed")
        );
    }

    /// `wait` 的兜底错误码：js 条件与 selector/text 条件必须区分（CLI 按码分支）。
    #[test]
    fn wait_failure_codes_are_distinct() {
        assert_eq!(wait_failure_code(true), "wait_js_failed");
        assert_eq!(wait_failure_code(false), "wait_timeout");
    }

    /// 没有 `ok` 字段的回包保持旧行为（老扩展 / 第三方回包），不能凭空判失败。
    #[test]
    fn missing_ok_keeps_legacy_behavior() {
        assert!(ensure_page_result_ok(&json!({ "js_return": { "title": "x" } }), "read").is_ok());
        assert!(ensure_page_result_ok(&json!({ "js_return": { "ok": true } }), "read").is_ok());
    }
}
