use super::*;

pub(crate) async fn scan(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Json<Value> {
    touch(&state).await;
    let timeout = secs(req.timeout.unwrap_or(30.0).clamp(5.0, 300.0));
    let result = scan_page(
        &state,
        req.tabs_only,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        req.text_only,
        req.fast,
        req.max_nodes,
        timeout,
    )
    .await;
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    })
}

pub(crate) async fn exec(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Json<Value> {
    touch(&state).await;
    let combined_wait = req.wait_js.is_some() && !is_extension_json(&req.script);
    Json(match exec_value(&state, req, None).await {
        Ok(value) => {
            // 组合 wait 失败时脚本结果仍然有效（script_executed=true），
            // 但顶层 ok 必须体现组合结论 —— 旧实现固定 ok:true，调用方看到成功
            // 就继续下一步，而等待条件其实从未成立。
            // `ATB_RESULT_STRICT=off` 是给旧脚本的逃生口：明确要求「组合命令只看
            // 脚本结果、不看 wait 结论」时才恢复旧行为。默认 warn / strict 都必须
            // 把失败如实报成 ok:false。
            if value.get("wait_js_failed").and_then(Value::as_bool) == Some(true)
                && crate::settings::result_strict() != "off"
            {
                json!({
                    "ok": false,
                    "error": value
                        .get("wait_error")
                        .cloned()
                        .unwrap_or_else(|| json!("--wait-js 条件未成立")),
                    "error_code": "wait_js_failed",
                    "retryable": true,
                    "result": value,
                    "combined_wait": combined_wait,
                })
            } else {
                json!({ "ok": true, "result": value, "combined_wait": combined_wait })
            }
        }
        Err(err) => {
            // 受限站点人话化（impl-actions 交叉依赖）：旧实现把 Chrome 原文
            // 「Cannot access a chrome-extension:// URL of different extension」原样
            // 返回，调用方不知道这是页面隔离、也不知道还有无 eval 通道可用。
            // 这里在错误组装出口统一翻译：稳定 error_code:"page_protected" +
            // 人话 error + 可执行 hint + 原始报错 raw_error（便于排障）。
            // 先过 translate：无论来源是否已翻译，出口行为一致。
            let translated = translate_page_protected_error(err);
            if let Some(protected) = translated.downcast_ref::<PageProtectedError>() {
                json!({
                    "ok": false,
                    "error": protected.to_string(),
                    "error_code": "page_protected",
                    "hint": "改用 read / scan / snapshot（走无 eval 通道），或 mouse-click 直接坐标点击",
                    "raw_error": protected.raw,
                })
            } else {
                // 目标解析错误接入 typed error_code（impl-actions 交叉依赖）：
                // SessionSelectError 命中时输出结构化 error_code/retryable，
                // 未命中保持原 {ok:false, error: 文本} 形态不变（向后兼容）。
                session_error_json(&translated)
            }
        }
    })
}

/// `GET /exec-result/:exec_id`：取回迟到结果。
///
/// 命中即取走（消费语义，避免同一结果被反复读到误判页面状态）；未命中返回
/// `found:false` —— 可能从未迟到，也可能已超过 TTL（`ATB_EXEC_RESULT_TTL_SEC`，
/// 默认 3600s）。响应里的 `found` / `result` / `error` / `ttl_seconds` 全部是
/// 新增字段，不影响既有契约。
pub(crate) async fn exec_result(
    State(state): State<AppState>,
    AxumPath(exec_id): AxumPath<String>,
) -> Json<Value> {
    let mut late = state.late_results.lock().await;
    let now = Instant::now();
    // 每次请求读取 TTL：清理判定与回包里的 ttl_seconds 必须同源。
    let ttl = late_result_ttl();
    // 顺带清理过期项，保持集合规模受在途执行数约束。
    late.retain(|_, (_, at)| now.duration_since(*at) < ttl);
    match late.remove(&exec_id) {
        Some((value, _)) => Json(json!({
            "ok": true,
            "found": true,
            "result": value.get("result").cloned().unwrap_or(Value::Null),
            "error": value.get("error").cloned().unwrap_or(Value::Null),
            "ttl_seconds": ttl.as_secs(),
        })),
        None => Json(json!({
            "ok": true,
            "found": false,
            "ttl_seconds": ttl.as_secs(),
        })),
    }
}

pub(crate) async fn exec_value(
    state: &AppState,
    req: ExecRequest,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Value> {
    let script = req.script.clone();
    let is_extension = is_extension_json(&script);
    // 普通 JS 统一走确定性包装：单行 `a(); return b` 在扩展侧会被自动补 return 的逻辑
    // 误判成 `return a();`，导致返回值错误。把语义收拢到 daemon 侧，避免依赖扩展版本。
    let page_script = if is_extension {
        script
    } else {
        wrap_user_script(&script)
    };
    let exec_timeout = secs(req.timeout.unwrap_or(60.0).clamp(1.0, 600.0));
    let target_options = ExecTargetOptions {
        world: req.world.clone(),
        frame_id: req.frame_id,
        all_frames: req.all_frames,
    };
    let mut result = execute_page_js_with_target(
        state,
        &page_script,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
        req.no_monitor,
        exec_timeout,
        &target_options,
    )
    .await?;
    // 2026-11-13：CLI 侧跳过了本地 JS 配平检查时在结果里留痕，
    // 便于调用方区分「浏览器接受了这段脚本」与「本地检查误报被绕过」。
    if req.no_preflight {
        result["no_preflight"] = json!(true);
    }
    // 预算：`--max-bytes` 显式给了就优先（`0` 表示不限，历史语义），没给就用
    // daemon 侧默认 `ATB_EXEC_MAX_BYTES`。直连 API 的调用方从来不传
    // --max-bytes，旧实现等于完全不截断——一条 exec 就能把几百 MB 回包甩给客户端。
    // 注意：口径是**字符**（与 --max-chars 同口径），字段名沿用历史叫法只为
    // 保持线协议兼容。
    let budget = match req.max_bytes {
        Some(explicit) => explicit,
        None => crate::settings::exec_max_bytes(),
    };
    if budget > 0 {
        let truncated = truncate_json_value_chars(&mut result, budget);
        if truncated {
            result["truncated"] = json!(true);
        }
    }
    // 等待逻辑放在 daemon 侧轮询：后台标签页的 setTimeout 会被 Chrome 节流到分钟级，
    // 在页面里 sleep 会让 --wait-js 在隐藏标签页里几乎必然超时。
    if let Some(wait_js) = req.wait_js.as_deref().filter(|_| !is_extension) {
        let session_key = result
            .get("session_key")
            .and_then(Value::as_str)
            .map(str::to_string);
        let main = result.get("js_return").cloned().unwrap_or(Value::Null);
        // 组合模式下 `result.js_return` 变成 `{result, wait}`（不再是脚本裸值）。
        // 在 result 内也显式打标，读 result 的调用方（skill / CI）无需猜形状；
        // 顶层 `combined_wait` 由 `exec()` 统一附加，两者语义一致。
        result["combined_wait"] = json!(true);
        result["wait_js"] = json!(wait_js);
        match wait_for_js_condition(
            state,
            session_key.as_deref(),
            wait_js,
            req.wait_timeout,
            req.wait_interval,
            cancel.as_deref(),
        )
        .await
        {
            Ok(wait) => {
                // 组合模式的 `js_return` 是 `{result, wait}` 两段式形状
                // （顶层另有 `combined_wait:true` 供调用方判别），与单脚本的裸值不同。
                // 保持旧版输出形状，避免破坏已有 skill 提示词。
                // `wait_for_js_condition*` 的超时/取消是 `Ok({ok:false,
                // matched:false,...})`，不是 Err。旧实现只在这里塞 js_return，
                // 从不检查 `wait.ok`，于是 `exec()` 出口的 `wait_js_failed` 检查
                // 永远不成立 —— 等待超时被报成顶层 ok:true（W 类假成功）。
                let wait_failed = wait.get("ok").and_then(Value::as_bool) == Some(false)
                    || wait.get("matched").and_then(Value::as_bool) == Some(false);
                let wait_error = wait
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                result["js_return"] = json!({ "result": main, "wait": wait });
                if wait_failed {
                    // 脚本已经执行过：结果必须保留，只把组合结论标成失败。
                    result["script_executed"] = json!(true);
                    result["wait_js_failed"] = json!(true);
                    result["wait_error"] = json!(wait_error.unwrap_or_else(|| {
                        "--wait-js 条件未成立（超时 / 取消 / 页面错误）".to_string()
                    }));
                }
            }
            Err(err) => {
                // 脚本**已经执行过**，结果必须原样交回（调用方可能已经下单/提交），
                // 所以这里不能像旧实现那样 `?` 把整条命令打成错误、丢掉脚本结果。
                // 但外层 ok 也绝不能是 true：那等于把「等待失败」报成成功。
                // 用 `wait_js_failed` 标记交给 `exec` 组装点决定顶层 ok / error_code。
                let message = err.to_string();
                result["js_return"] = json!({
                    "result": main,
                    "wait": {
                        "ok": false,
                        "matched": false,
                        "mode": "js",
                        "error": message,
                    },
                });
                result["script_executed"] = json!(true);
                result["wait_js_failed"] = json!(true);
                result["wait_error"] = json!(message.clone());
            }
        }
        // `wait.value` 是谓词返回的任意值（有的站点直接返回整页 innerHTML），
        // 而上面的截断发生在合并之前 —— 它一直不受预算约束。这里用**同一个**预算
        // 再截一次，并标 `truncated` 让调用方知道 wait 段被削过。
        if budget > 0 {
            if let Some(slot) = result
                .get_mut("js_return")
                .and_then(|value| value.get_mut("wait"))
                .and_then(|value| value.get_mut("value"))
            {
                if truncate_json_slot_chars(slot, budget) {
                    result["truncated"] = json!(true);
                }
            }
        }
    }
    Ok(result)
}

pub(crate) async fn execute_page_js(
    state: &AppState,
    script: &str,
    selector: SessionSelector,
    no_monitor: bool,
    timeout: Duration,
) -> Result<Value> {
    execute_page_js_with_target(
        state,
        script,
        selector,
        no_monitor,
        timeout,
        &ExecTargetOptions::default(),
    )
    .await
}

pub(crate) async fn execute_page_js_with_target(
    state: &AppState,
    script: &str,
    selector: SessionSelector,
    no_monitor: bool,
    timeout: Duration,
    options: &ExecTargetOptions,
) -> Result<Value> {
    execute_page_js_inner(state, script, selector, no_monitor, timeout, options, true).await
}

/// 与 [`execute_page_js_with_target`] 相同，但**注入完成就释放会话锁**。
///
/// 用于「一次注入、在页面里等很久」的等待类调用（`wait --selector/--text` 的
/// MutationObserver 分支）：锁跟着等会把同一标签上的 read / click 全部堵死，
/// 而等待本身并不需要串行 —— 注入顺序由页面事件循环保证。
pub(crate) async fn execute_page_js_with_target_releasing_lock(
    state: &AppState,
    script: &str,
    selector: SessionSelector,
    no_monitor: bool,
    timeout: Duration,
    options: &ExecTargetOptions,
) -> Result<Value> {
    execute_page_js_inner(state, script, selector, no_monitor, timeout, options, false).await
}

async fn execute_page_js_inner(
    state: &AppState,
    script: &str,
    selector: SessionSelector,
    no_monitor: bool,
    timeout: Duration,
    options: &ExecTargetOptions,
    hold_lock: bool,
) -> Result<Value> {
    let executor_session_key = select_tab(state, selector).await?;
    // --monitor 改为页面内 MutationObserver：脚本执行期间记录变更，
    // 不再"执行前后各抓一次整页 HTML 再 diff"（大页面上那两次序列化比脚本本身还贵）。
    let watched_script = if no_monitor || is_extension_json(script) {
        None
    } else {
        Some(wrap_with_mutation_watch(script))
    };
    let effective_script = watched_script.as_deref().unwrap_or(script);
    // 只有需要检测「脚本是否新开标签页」时才枚举全部会话。
    // 内部调用（read/find/action/wait）一律跳过，省掉一次全量会话序列化。
    let before_tabs: std::collections::HashSet<String> = if no_monitor {
        std::collections::HashSet::new()
    } else {
        active_tabs(state, true)
            .await
            .into_iter()
            .map(|t| t.session_key)
            .collect()
    };
    let execution = execute_raw_js_inner(
        state,
        effective_script,
        timeout,
        Some(&executor_session_key),
        options,
        hold_lock,
    )
    .await?;
    let current_session_key = execution.session_key;
    let current_tab_id = execution.tab_id;
    let response = execution.result;
    let mut result = json!({
        "status": "success",
        "js_return": response.data.or(response.result).unwrap_or(Value::Null),
        "tab_id": current_tab_id,
        "session_key": current_session_key,
    });
    if let Some(tabs) = response.new_tabs {
        result["newTabs"] = tabs;
    } else if !no_monitor {
        let after_tabs = active_tabs(state, false).await;
        let new_tabs: Vec<_> = after_tabs
            .into_iter()
            .filter(|t| !before_tabs.contains(&t.session_key))
            .map(|t| json!({ "id": t.id, "tab_id": t.tab_id, "session_key": t.session_key, "url": t.url }))
            .collect();
        if !new_tabs.is_empty() {
            result["newTabs"] = json!(new_tabs);
        }
    }
    if watched_script.is_some() {
        unwrap_mutation_watch(&mut result);
        // 包装层捕获到的脚本异常必须变成失败。只有 `script_error` 标记（由
        // unwrap_mutation_watch 设置）才判错：用户脚本正常 `return {error: ...}`
        // 属于业务数据，不带这个标记，不会被误伤。
        if result.get("script_error").and_then(Value::as_bool) == Some(true) {
            let message = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown script error")
                .to_string();
            return Err(anyhow!("script_error: {message}"));
        }
    }
    Ok(result)
}

pub(crate) struct ExecWithSession {
    pub(crate) result: ExecResult,
    pub(crate) session_key: String,
    pub(crate) tab_id: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExecTargetOptions {
    pub(crate) world: Option<String>,
    pub(crate) frame_id: Option<i64>,
    pub(crate) all_frames: bool,
}

pub(crate) async fn execute_raw_js_with_session(
    state: &AppState,
    code: &str,
    timeout: Duration,
    requested_session_key: Option<&str>,
) -> Result<ExecWithSession> {
    execute_raw_js_with_target(
        state,
        code,
        timeout,
        requested_session_key,
        &ExecTargetOptions::default(),
    )
    .await
}

pub(crate) async fn execute_raw_js_with_target(
    state: &AppState,
    code: &str,
    timeout: Duration,
    requested_session_key: Option<&str>,
    options: &ExecTargetOptions,
) -> Result<ExecWithSession> {
    execute_raw_js_inner(state, code, timeout, requested_session_key, options, true).await
}

/// 等待类命令用：消息发出去就放掉会话锁。
///
/// `wait --js` 现在是一次注入、在页面里等几十秒；锁要是跟着等，
/// 同一标签上的 read / click 会被一个「等待」堵死。
/// 注入顺序仍由页面的事件循环保证，串行性不受影响。
pub(crate) async fn execute_raw_js_releasing_lock(
    state: &AppState,
    code: &str,
    timeout: Duration,
    requested_session_key: Option<&str>,
    options: &ExecTargetOptions,
) -> Result<ExecWithSession> {
    execute_raw_js_inner(state, code, timeout, requested_session_key, options, false).await
}

/// 会话串行锁等待超时。
///
/// 语义：「本次请求的**总预算**已经用尽，命令一个字都没派发出去」。
/// 所以它是 `retryable: true` —— 和 `outcome_unknown`（可能已经执行）刚好相反，
/// 调用方可以放心重试，不会造成二次点击 / 二次输入。
/// `message` 里刻意不出现 "timeout" 子串：`is_ambiguous_outcome` 按子串判定
/// 「结果未知」，带上这个词会把「确定没派发」误判成「可能已执行」。
pub(crate) fn session_busy_error(session_key: &str, waited_ms: u128) -> anyhow::Error {
    SessionSelectError {
        code: "session_busy_timeout",
        message: format!(
            "会话 {session_key} 正忙：等待串行锁 {waited_ms}ms 后本次请求的预算已用尽，命令未派发（同一标签的命令串行执行）"
        ),
        hint: "等前一条命令结束后重试（可加大 --timeout，或改用 --async 让耗时命令不占串行位）；本错误可安全重试"
            .to_string(),
        retryable: true,
        retry_after_ms: Some(500),
    }
    .into_anyhow()
}

pub(crate) async fn execute_raw_js_inner(
    state: &AppState,
    code: &str,
    timeout: Duration,
    requested_session_key: Option<&str>,
    options: &ExecTargetOptions,
    hold_lock: bool,
) -> Result<ExecWithSession> {
    // 请求级 deadline。以前 `timeout` 只管最后等回包的那一次 `timeout()`：
    // 冷启动等会话（最多 12s）+ 等 driver 读锁 + 等会话串行锁（可能被一条长命令
    // 占很久）全都不计入，`--timeout 5` 实际能跑 30s 以上，调用方早已断开，
    // 而 daemon 还在为这条已废弃的请求排队 —— 排到后照样派发，动作照常执行
    // （「迟到副作用」）。现在所有等待都从同一个 deadline 派生。
    // `timeout_at` 要的是 tokio 的 Instant（与 std 的不通用）。
    let deadline = tokio::time::Instant::now() + timeout;
    let (session_key, tab_id, sender) = {
        // 冷启动等会话：内部自己有 5s/12s 窗口，但绝不能越过请求预算。
        if tokio::time::timeout_at(deadline, wait_for_sessions(state, WAIT_SESSIONS_ACTION))
            .await
            .is_err()
        {
            return Err(session_busy_error("(等待浏览器会话)", timeout.as_millis()));
        }
        // driver 读锁也可能被长写操作挡住：同样按 deadline 截断，
        // 不给自己一个「无限期排队」的机会。
        let driver = match tokio::time::timeout_at(deadline, state.driver.read()).await {
            Ok(driver) => driver,
            Err(_) => {
                return Err(session_busy_error("(等待会话表)", timeout.as_millis()));
            }
        };
        let session_key = if let Some(requested) = requested_session_key {
            driver
                .sessions
                .get(requested)
                .filter(|session| session.is_active())
                .map(|session| session.session_key.clone())
                .ok_or_else(|| anyhow!("会话ID {requested} 未连接"))?
        } else {
            driver
                .default_session_key
                .as_ref()
                .and_then(|key| {
                    driver
                        .sessions
                        .get(key)
                        .filter(|s| s.is_active())
                        .map(|s| s.session_key.clone())
                })
                .or_else(|| {
                    driver.latest_session_key.as_ref().and_then(|key| {
                        driver
                            .sessions
                            .get(key)
                            .filter(|s| s.is_active())
                            .map(|s| s.session_key.clone())
                    })
                })
                .or_else(|| {
                    driver
                        .sessions
                        .values()
                        .find(|s| s.is_active())
                        .map(|s| s.session_key.clone())
                })
                .ok_or_else(|| {
                    anyhow!("没有可用的浏览器标签页，请先在 Chrome 打开一个 http/https 页面")
                })?
        };
        let session = driver
            .sessions
            .get(&session_key)
            .filter(|s| s.is_active())
            .ok_or_else(|| anyhow!("会话ID {session_key} 未连接"))?;
        (session_key, session.tab_id.clone(), session.sender.clone())
    };
    // 同一标签内串行执行：避免同一标签上并发注入互相干扰，
    // 不同标签各自一把锁，跨标签仍然并发。
    let session_guard =
        match tokio::time::timeout_at(deadline, session_lock(state, &session_key)).await {
            Ok(guard) => guard,
            Err(_) => return Err(session_busy_error(&session_key, timeout.as_millis())),
        };
    // 上面确认会话存在之后、建锁之前，会话可能被 forget_session 删掉。
    // 那种情况下 `entry().or_insert_with()` 会为已消亡的会话新建锁条目，
    // forget 已经跑过、不会再来删它 → 永久残留。建锁后在同一读锁语义下再确认一次。
    {
        let driver = match tokio::time::timeout_at(deadline, state.driver.read()).await {
            Ok(driver) => driver,
            Err(_) => return Err(session_busy_error(&session_key, timeout.as_millis())),
        };
        let alive = driver
            .sessions
            .get(&session_key)
            .is_some_and(|session| session.is_active());
        drop(driver);
        if !alive {
            // 只有「表里 + 本地」两个引用时才移除：别人已经拿到同一把锁时留着，
            // 让那条路径自己复核，避免拆锁导致同一标签出现两把锁。
            // 登记表本身只做一次 insert/remove（不跨 await 持有），但取它也可能撞上
            // 别人正在改表，仍然按 deadline 截断，避免在这里变相无限排队。
            let mut locks =
                match tokio::time::timeout_at(deadline, state.session_locks.lock()).await {
                    Ok(locks) => locks,
                    Err(_) => return Err(session_busy_error(&session_key, timeout.as_millis())),
                };
            if Arc::strong_count(&session_guard) == 2 {
                locks.remove(&session_key);
            }
            return Err(anyhow!("会话ID {session_key} 未连接"));
        }
    }
    // 锁等待也必须受 deadline 约束：另一个请求可能正在同一标签上跑一条 60s 的脚本。
    // 等不到就直接返回 session_busy_timeout，**不得继续 dispatch** —— 请求预算已经
    // 用尽，调用方早就走了，此时再派发就是纯迟到副作用。
    let mut serial = match tokio::time::timeout_at(deadline, session_guard.lock()).await {
        Ok(guard) => Some(guard),
        Err(_) => return Err(session_busy_error(&session_key, timeout.as_millis())),
    };

    let exec_id = Uuid::new_v4().to_string();
    let mut payload_value = json!({
        "id": exec_id,
        "code": code,
        "tabId": parse_tab_id(&tab_id)?
    });
    if let Some(world) = options.world.as_deref() {
        payload_value["world"] = json!(world);
    }
    if let Some(frame_id) = options.frame_id {
        payload_value["frameId"] = json!(frame_id);
    }
    if options.all_frames {
        payload_value["allFrames"] = json!(true);
    }
    let payload = payload_value.to_string();
    // 「迟到副作用」防护：派发前先看剩余预算。剩不到 50ms 时连一次往返都
    // 跑不完，派发出去只会在页面里留下一个**没人等的副作用**（调用方早已超时离开，
    // 这里却还是把 click / POST 打出去了）。明确报「未派发、可重试」。
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining < Duration::from_millis(50) {
        return Err(SessionSelectError {
            code: "request_budget_exhausted",
            message: format!(
                "请求预算（{}s）在派发前已用尽：命令未派发，页面没有任何副作用",
                timeout.as_secs()
            ),
            hint: "加大 --timeout 后重试；本次命令确定没有执行".to_string(),
            retryable: true,
            retry_after_ms: None,
        }
        .into_anyhow());
    }
    let (tx, rx) = oneshot::channel();
    // 顺序必须是「注册 → send → send 失败时 take 回滚」：
    // send 之后扩展可能在下一个事件循环就回包，而回包由另一条任务处理；
    // 若那时还没注册，`take` 取不到，结果直接丢，这条命令只能等到超时。
    // 旧写法把 `?` 挂在 send 上，失败时登记留在表里，tx 永不发结果、条目永不清理。
    state.execs.lock().await.register(
        &exec_id,
        &session_key,
        crate::protocol::PendingExec {
            delivered_at: None,
            tx,
        },
    );
    // 下行命令通道改有界，发送必须走 `send_downstream`，让「队列满」
    // 变成结构化 `ws_backpressure` 而不是无限积压（也不能像旧实现那样把 `send`
    // 的错误直接吞成一句「连接已断开」）。
    if let Err(err) = crate::server::ws::send_downstream(&sender, payload).await {
        state.execs.lock().await.take(&exec_id);
        return Err(err);
    }
    // 只要「注入」这一步串行就够了；等待结果期间没必要占着锁。
    if !hold_lock {
        drop(serial.take());
    }
    // 回包等待同样只吃 deadline 剩下的那部分（不是「再给一个完整 timeout」）：
    // 旧写法每次重试/回退都重新给满额超时，单条命令的总耗时会成倍于 --timeout。
    let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
    let result = match tokio::time::timeout(wait, rx).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => Err(anyhow!("执行结果通道已关闭")),
        Err(_) => {
            let mut execs = state.execs.lock().await;
            let acked = execs.acked.remove(&exec_id);
            execs.take(&exec_id);
            drop(execs);
            // 请求预算已耗尽，页面里的脚本可能还在跑。daemon 不再等它，
            // 但必须**尽力**告诉扩展释放这次 exec 占用的 CDP attach —— 否则只能
            // 等扩展的 idle 定时器回收，期间其他扩展的 attach 会被挡。
            // 只走已建立的 sender 直发：此刻串行锁可能还在手上（hold_lock），
            // 再走 extension_control 会二次排队甚至自锁；发送失败只记 warning，
            // 绝不改变下面的原始超时错误。
            notify_exec_cancel(&sender, &exec_id, &tab_id, "exec_timeout").await;
            // 超时必须作为失败返回：旧行为返回 ok:true + 一段提示字符串，
            // 智能体会误判成功并继续下一步，页面状态已经错乱。
            let detail = if acked {
                "ACK received, script may still be running; 可增大 --timeout 或改用 --async。\
                 若目标长期在后台，Chrome 可能已冻结该标签（内存节省），先 `atb focus --tab <id>` 再重试"
            } else {
                "no ACK, script may not have been delivered"
            };
            // 错误里带上 exec_id 与查询指引。请求被 take 之后扩展若
            // 晚到回包，结果会暂存 `ATB_EXEC_RESULT_TTL_SEC`（默认 3600s），可凭
            // GET /exec-result/:id 或 `atb job result <exec_id>` 查回。
            let ttl_sec = crate::settings::exec_result_ttl_sec();
            let ttl_text = if ttl_sec.is_multiple_of(60) {
                format!("{} 分钟内有效", ttl_sec / 60)
            } else {
                format!("{ttl_sec} 秒内有效")
            };
            Err(anyhow!(
                "exec_timeout: 页面在 {:.1}s 内没有返回结果（{detail}）。exec_id={exec_id}；\
                 结果可能已通过 GET /exec-result/{exec_id} 查询（{ttl_text}），\
                 或运行 `atb job result {exec_id} --wait 60`",
                timeout.as_secs_f64()
            ))
        }
    }?;
    Ok(ExecWithSession {
        result,
        session_key,
        tab_id,
    })
}

// 超时 / 取消后向扩展发一条 `cancel` 帧（best-effort，不等回包）。
///
/// 为什么要绕开 `extension_control`：该函数会走 `execute_raw_js_on_tab`，需要
/// 会话锁 + 一次完整往返；而调用点恰恰是「预算已耗尽、串行锁可能仍被本请求持有」，
/// 再排队会自锁。这里直接复用扩展连接的 sender（无锁、无等待）。
/// 发送失败只记 warning —— 原始超时错误必须原样返回。
async fn notify_exec_cancel(
    sender: &mpsc::Sender<String>,
    exec_id: &str,
    tab_id: &str,
    reason: &str,
) {
    let Ok(tab) = parse_tab_id(tab_id) else {
        eprintln!("exec: tab_id 无法解析（{tab_id}），跳过 cancel 通知 exec_id={exec_id}");
        return;
    };
    // id 刻意与 exec_id 区分：扩展对未知 cmd 会回一条错误，若复用 exec_id，
    // 这条错误会被存成该次执行的「迟到结果」，污染 GET /exec-result/:exec_id。
    let frame = json!({
        "id": format!("{exec_id}#cancel"),
        "cmd": "cancel",
        "execId": exec_id,
        "tabId": tab,
        "reason": reason,
    });
    // 这里不能等满 `send_downstream` 的默认 10s 入队窗口：调用点已判定预算耗尽，
    // 再花 10s 等队列只会把「超时」拖成「超时 + 10s」。给 500ms 即可 ——
    // 队列空时立刻成功，满时说明对端确实读不动，cancel 丢掉也不影响错误语义。
    if crate::server::ws::send_downstream_within(
        sender,
        frame.to_string(),
        Duration::from_millis(500),
    )
    .await
    .is_err()
    {
        eprintln!("exec: cancel 帧未能下发（扩展连接已断开或下行队列拥塞）exec_id={exec_id}");
    }
}

/// 页面元素数（带 2 分钟缓存）：决定 scan 要不要走整页 HTML 序列化。
///
/// 缓存带 URL 校验：同一 session_key 导航到新页面后旧值必须失效，
/// 否则「小页面 → 大页面」的跳转会继续按小页面判断，降级路径永远不触发。
pub(crate) async fn page_element_count(
    state: &AppState,
    session_key: &str,
    timeout: Duration,
) -> Result<usize> {
    let current_url = session_url(state, session_key).await;
    {
        let cache = state.page_scale.lock().await;
        if let Some((count, at, url)) = cache.get(session_key) {
            if at.elapsed() < Duration::from_secs(120) && Some(url) == current_url.as_ref() {
                return Ok(*count);
            }
        }
    }
    let response = execute_raw_js_with_session(
        state,
        // 让页面自己回报采样时的 `location.href`：只靠 daemon 侧事后读
        // `session.info.url`，导航与 tabs_update 的时序会让「新 URL 配旧计数」
        // 进入缓存，最长 120s 内 scan 的降级判断都是错的。
        "return { count: document.getElementsByTagName('*').length, url: location.href };",
        timeout.min(Duration::from_secs(10)),
        Some(session_key),
    )
    .await?;
    let sampled = response
        .result
        .data
        .or(response.result.result)
        .unwrap_or(Value::Null);
    let count = sampled
        .get("count")
        .and_then(Value::as_u64)
        .or_else(|| sampled.as_u64())
        .unwrap_or_default() as usize;
    let sampled_url = sampled
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 只有「页面自报的 URL」与 daemon 记录的当前 URL 一致时才写缓存：
    // 不一致说明采样期间正在导航，宁可不缓存也不要写错。
    let url = session_url(state, session_key).await.unwrap_or_default();
    if !sampled_url.is_empty() && sampled_url == url {
        let mut cache = state.page_scale.lock().await;
        cache.insert(
            session_key.to_string(),
            (count, Instant::now(), sampled_url),
        );
    }
    Ok(count)
}

/// 会话当前 URL（`tabs_update` 会持续更新 `session.info.url`）；会话不存在时 None。
pub(crate) async fn session_url(state: &AppState, session_key: &str) -> Option<String> {
    let driver = state.driver.read().await;
    driver
        .sessions
        .get(session_key)
        .map(|session| session.info.url.clone())
        .filter(|url| !url.is_empty())
}

/// 在 daemon 侧轮询等待页面条件成立。
///
/// 不在页面里 `setTimeout` 循环：Chrome 会把隐藏（后台）标签页的定时器节流到
/// 1 秒甚至 1 分钟一次，页面内 sleep 会让后台任务几乎必然等到超时。
/// 每次轮询是一次独立脚本注入，不受标签页可见性影响。
pub(crate) async fn wait_for_js_condition(
    state: &AppState,
    session_key: Option<&str>,
    wait_js: &str,
    timeout: f64,
    interval: f64,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    wait_for_js_condition_with_target(
        state,
        session_key,
        wait_js,
        timeout,
        interval,
        cancel,
        &ExecTargetOptions::default(),
    )
    .await
}

pub(crate) const WAIT_JS_EVAL_BLOCKED: &str = "wait_js_eval_blocked";

/// 页面通道是否明确回报「页面 CSP 禁止运行时编译字符串谓词」。
///
/// 只认契约码 `wait_js_eval_blocked`（CONTRACT §3.2）：`page_wait.js` 把
/// **CSP 拦截**归到这个码，把**谓词语法错/空谓词**留在 `js_predicate_compile_failed`。
/// 后者换 CDP 也救不回来（同一个 SyntaxError），走回退只会多绕一次往返并给出
/// 更奇怪的 `cdp-eval` 报错，所以不触发回退。
fn is_wait_js_eval_blocked(payload: &Value) -> bool {
    payload.get("error_code").and_then(Value::as_str) == Some(WAIT_JS_EVAL_BLOCKED)
}

pub(crate) async fn wait_for_js_condition_with_target(
    state: &AppState,
    session_key: Option<&str>,
    wait_js: &str,
    timeout: f64,
    interval: f64,
    cancel: Option<&AtomicBool>,
    target: &ExecTargetOptions,
) -> Result<Value> {
    // 这是 `wait` / `--wait-js` 共用的轮询循环，入口统一 clamp：
    // - timeout 不得超过 `ATB_WAIT_MAX_TOTAL_SEC`（否则 Duration 会饱和成天量）；
    // - interval 不得低于 `ATB_WAIT_MIN_INTERVAL_MS`（否则 interval=0 忙轮询）；
    // - NaN / inf 一律回落到安全值。
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
    let mut injections: u64 = 0;
    let mut checks_total: u64 = 0;
    let mut last_value = Value::Null;
    let mut last_error: Option<String> = None;
    let mut repeated_error: u32 = 0;
    // 页面侧「几乎立刻返回但没命中」的连续次数（见下方 slice 逻辑）。
    let mut quick_returns: u32 = 0;

    // 谓词只在页面里编译一次、反复求值：DOM 变化由 MutationObserver 立即触发，
    // 非 DOM 条件由定时器兜底。daemon 这边正常只有一次注入（一次往返），
    // 只有换页/上下文销毁这类「注入本身失败」才按剩余时间重试。
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(json!({
                "ok": false, "matched": false, "cancelled": true,
                "value": last_value, "error": "cancelled", "polls": checks_total.max(injections),
                "mode": "js-observer", "injections": injections
            }));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // 可取消时按 ≤1s 切片：每次注入都会在页面里阻塞到该片的超时为止，
        // 若直接把剩余时间整段注入（20s 的 wait 就是一次 20s 的往返），
        // 顶部的 cancel 检查要等整段返回才轮到 —— 取消看起来「没反应」。
        // 同步调用（cancel 为 None）保持整段注入，避免多付往返。
        let slice_ms = if cancel.is_some() {
            (remaining.as_millis() as u64).clamp(1, 1_000)
        } else {
            (remaining.as_millis() as u64).max(1)
        };
        injections += 1;
        let opts = json!({
            "js": wait_js,
            "timeoutMs": slice_ms,
            // 键名必须是 minIntervalMs：page_wait.js 只读这个字段，历史上这里传的是
            // intervalMs，于是 `wait --js --interval` 被静默忽略、永远按 50ms 巡检。
            // selector/text 分支（本文件另一处）一直传对了，就 js 这条漏了。
            "minIntervalMs": interval_ms,
        });
        let script = format!(
            "{}\nreturn await __abcWait({});",
            html::js_page_wait(),
            opts
        );
        // 注入超时绝不能越过外层 deadline。旧实现固定 `slice + 3s`，
        // 最后一片会多等 3 秒，整条命令的总耗时突破 `--wait-timeout`。
        // 每轮按**当下的**剩余预算重算一次（循环里可能已经花掉若干往返）。
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
                // 页面 CSP 禁 unsafe-eval 时字符串谓词编译不了。页面通道已经
                // 没有别的办法，这里改用 CDP `Runtime.evaluate` 轮询到 deadline ——
                // 走的是**同一条**已建立的调试器通道（cdp_call → 扩展 cdp 命令），
                // 不额外 attach 连接。返回形状与页面通道一致，调用方无需分支。
                if is_wait_js_eval_blocked(&payload) {
                    return cdp_wait_for_js_condition(
                        state,
                        session_key,
                        wait_js,
                        deadline,
                        Duration::from_millis(interval_ms),
                        cancel,
                        target,
                        injections,
                        checks_total,
                    )
                    .await;
                }
                if payload
                    .get("matched")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return Ok(json!({
                        "ok": true,
                        "matched": true,
                        "value": payload.get("value").cloned().unwrap_or(Value::Null),
                        "error": Value::Null,
                        "polls": checks_total,
                        "checks": checks_total,
                        "mode": payload.get("mode").cloned().unwrap_or(json!("js-observer")),
                        "injections": injections,
                        "elapsed_ms": payload.get("elapsed_ms").cloned().unwrap_or(Value::Null),
                    }));
                }
                // 页面侧已经等满本片的 timeoutMs 仍未命中。
                // 可取消时每片只有 1s，所以「本片超时」不等于「整体超时」：
                // 只要整体 deadline 还没到就继续下一片，让取消能在片间生效。
                let page_elapsed_ms = payload
                    .get("elapsed_ms")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if let Some(value) = payload.get("value") {
                    last_value = value.clone();
                }
                if let Some(err) = payload.get("error").and_then(Value::as_str) {
                    last_error = Some(err.to_string());
                }
                if Instant::now() >= deadline {
                    break;
                }
                // 同步调用是「整段注入」，本片就等于整体，未命中即可判超时（保持旧行为）。
                if cancel.is_none() {
                    break;
                }
                // 可取消路径：本片只等了 1s，继续下一片，让取消在片间生效。
                // 但页面几乎没等就返回（观察器没装起来 / 谓词编译失败之类）时
                // 继续循环会热转，连续 3 次就放弃，避免空烧 CPU 到 deadline。
                let quick = slice_ms >= 200 && page_elapsed_ms < (slice_ms as f64) * 0.5;
                quick_returns = if quick { quick_returns + 1 } else { 0 };
                if quick_returns >= 3 {
                    break;
                }
            }
            Err(err) => {
                // 换页 / 执行上下文销毁 / 注入被拒：换页后条件可能已经成立，
                // 不能直接判失败，按剩余时间重试。
                let message = err.to_string();
                // 注入通道把「CSP 拒绝编译」升级成整条命令的错误时（而不是
                // 回一个带 error_code 的载荷），同样切到 CDP，否则 CSP 页面无解。
                // 只认契约码：语法错（`js_predicate_compile_failed`）换通道也救不回来。
                if message.contains(WAIT_JS_EVAL_BLOCKED) {
                    return cdp_wait_for_js_condition(
                        state,
                        session_key,
                        wait_js,
                        deadline,
                        Duration::from_millis(interval_ms),
                        cancel,
                        target,
                        injections,
                        checks_total,
                    )
                    .await;
                }
                repeated_error = if last_error.as_deref() == Some(message.as_str()) {
                    repeated_error + 1
                } else {
                    1
                };
                last_error = Some(message);
                // 同一错误连续出现，多半是稳定失败（CSP 双通道都不可用等），
                // 没必要把超时时间全花在重试上。
                if repeated_error >= 3 {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(Duration::from_millis(150))).await;
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
        "injections": injections
    }))
}

/// 把 `--wait-js` 谓词包成 CDP `Runtime.evaluate` 的表达式形态。
///
/// 页面通道编译谓词时试三种写法（裸表达式 / `return x` / 多语句），CDP 的
/// `expression` 只有表达式上下文，所以这里准备两份候选：先按裸表达式求值，
/// 编译失败（SyntaxError）再退到「函数体」形态。两份候选都与页面侧等价：
/// 顶层 `return` 只有语句形态能编译，多语句同理。
fn cdp_expression_form(code: &str) -> String {
    let trimmed = code.trim().trim_end_matches(';').trim_end();
    format!("(async () => ({trimmed}\n))()")
}

fn cdp_statement_form(code: &str) -> String {
    let trimmed = code.trim();
    format!("(async () => {{\n{trimmed}\n}})()")
}

/// JS 真值语义（`Runtime.evaluate` 回的是 JSON 值，Rust 侧没有 `if (x)`）。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .map(|f| f != 0.0 && !f.is_nan())
            .unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 页面通道因 CSP 编译不了字符串谓词时，改用 CDP `Runtime.evaluate` 轮询。
///
/// 为什么 `Runtime.evaluate` 不受页面 CSP 约束：它由调试器协议执行，不经过页面的
/// `unsafe-eval` 检查。这里**复用** `cdp_call` 的既有通道（扩展 cdp 命令 + 同一条
/// 已建立的调试器会话），不新开连接、不重复 attach。
///
/// 返回形状与页面通道对齐：成功 `{ok:true, matched:true, value, polls, mode:"cdp-eval"}`，
/// 失败 `{ok:false, matched:false, value, error, error_code:"wait_js_failed"}`。
/// 子 frame（`target.frame_id`）在这一通道上无法定向，回包用
/// `frame_fallback:"main"` 明示这次求值发生在主框架。
#[allow(clippy::too_many_arguments)]
async fn cdp_wait_for_js_condition(
    state: &AppState,
    session_key: Option<&str>,
    wait_js: &str,
    deadline: Instant,
    interval: Duration,
    cancel: Option<&AtomicBool>,
    target: &ExecTargetOptions,
    injections: u64,
    checks_base: u64,
) -> Result<Value> {
    let Some(session_key) = session_key else {
        // 没有确定会话时无从 attach：如实回报「换通道也失败了」，别假装还在等。
        return Ok(json!({
            "ok": false,
            "matched": false,
            "value": Value::Null,
            "error": "wait_js_eval_blocked：页面禁止运行时编译字符串谓词，且当前没有可定位的会话，无法改用 CDP 求值",
            "error_code": WAIT_JS_EVAL_BLOCKED,
            "polls": checks_base.max(injections),
            "checks": checks_base,
            "mode": "cdp-eval",
            "injections": injections,
        }));
    };
    const METHOD: &str = "Runtime.evaluate";
    let expr_form = cdp_expression_form(wait_js);
    let stmt_form = cdp_statement_form(wait_js);
    let mut use_statement_form = false;
    let mut polls = checks_base;
    let mut last_value = Value::Null;
    let mut last_error: Option<String> = None;
    let mut repeated_error: u32 = 0;
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(json!({
                "ok": false,
                "matched": false,
                "cancelled": true,
                "value": last_value,
                "error": "cancelled",
                "polls": polls.max(injections),
                "checks": polls,
                "mode": "cdp-eval",
                "injections": injections,
            }));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        polls += 1;
        let expression = if use_statement_form {
            stmt_form.clone()
        } else {
            expr_form.clone()
        };
        // 单次求值自身只该花毫秒级：给 5s 上限，剩下的预算留给下一轮。
        let call_timeout = remaining.min(Duration::from_secs(5));
        let params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        });
        match cdp_call(state, session_key, METHOD, params, call_timeout).await {
            Ok(result) if result.ok => {
                let data = result.data.unwrap_or(Value::Null);
                if let Some(details) = data.get("exceptionDetails") {
                    let text = details
                        .get("exception")
                        .and_then(|exception| exception.get("description"))
                        .or_else(|| details.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("Runtime.evaluate 抛出异常")
                        .to_string();
                    // 第一候选是「裸表达式」：编译失败说明谓词是语句形态（顶层 return /
                    // 多语句），换第二候选再试一次；这不是执行错误，不能算失败。
                    if !use_statement_form
                        && details
                            .get("exception")
                            .and_then(|exception| exception.get("className"))
                            .and_then(Value::as_str)
                            == Some("SyntaxError")
                    {
                        use_statement_form = true;
                        continue;
                    }
                    last_error = Some(text);
                    break;
                }
                let value = data
                    .get("result")
                    .and_then(|object| object.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if js_truthy(&value) {
                    let mut ok = json!({
                        "ok": true,
                        "matched": true,
                        "value": value,
                        "error": Value::Null,
                        "polls": polls,
                        "checks": polls,
                        "mode": "cdp-eval",
                        "injections": injections,
                    });
                    // 子 frame 的谓词在 CDP 这条通道上只能落到主框架，明示而不是静默。
                    if target.frame_id.is_some() {
                        ok["frame_fallback"] = json!("main");
                    }
                    return Ok(ok);
                }
                last_value = value;
                repeated_error = 0;
            }
            Ok(result) => {
                last_error = result
                    .error
                    .or_else(|| Some("Runtime.evaluate 回包为失败".to_string()));
                repeated_error += 1;
                if repeated_error >= 2 {
                    break;
                }
            }
            Err(err) => {
                let err = translate_page_protected_error(err);
                last_error = Some(err.to_string());
                // 受限站点 / 禁用调试器：这条通道本就不可能成功，立即收手。
                if err.downcast_ref::<PageProtectedError>().is_some()
                    || err.to_string().contains("ATB_NO_DEBUGGER")
                {
                    break;
                }
                repeated_error += 1;
                if repeated_error >= 3 {
                    break;
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(interval.min(remaining)).await;
    }
    let mut failed = json!({
        "ok": false,
        "matched": false,
        "value": last_value,
        "error": last_error,
        "error_code": "wait_js_failed",
        "polls": polls.max(injections),
        "checks": polls,
        "mode": "cdp-eval",
        "injections": injections,
    });
    if target.frame_id.is_some() {
        failed["frame_fallback"] = json!("main");
    }
    Ok(failed)
}
