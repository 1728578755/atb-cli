use super::*;

// 使用 `@N`（tabs 列表序号）时的固定提示。
///
/// `@1` 是「tabs 输出顺序的第 1 个」，不是「最近打开的标签」；浏览器里标签一多，
/// 这个序号与用户的直觉几乎必然不一致。只在「目标数 > 1」或「标签总数很大」时
/// 提示，避免每条 `--tab @1` 都刷一遍噪音。
const INDEX_TARGET_WARNING: &str =
    "--tab @N 是 tabs 列表的第 N 个，不是最近打开的标签；需要“刚打开的那个”请用 --tab @latest";

#[derive(Debug, Deserialize)]
pub(crate) struct MultiRequest {
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) args: Value,
    /// `--tab` 的原始规格；为空时按默认标签页处理。
    #[serde(default)]
    pub(crate) target: Option<String>,
    /// 客户端已经解析好的 id 列表（优先级高于 target）。
    #[serde(default)]
    pub(crate) tab_ids: Vec<String>,
    #[serde(default)]
    pub(crate) browser: Option<String>,
    #[serde(default)]
    pub(crate) profile: Option<String>,
    /// 并发上限；默认取目标数，最多 8。
    #[serde(default)]
    pub(crate) concurrency: Option<usize>,
    /// 只解析目标、不执行，用于 --dry-run。
    #[serde(default)]
    pub(crate) dry_run: bool,
    #[serde(default)]
    pub(crate) stop_on_error: bool,
    /// 目标解析范围：`agent`（默认，只看本智能体 open 过的标签）/
    /// `all`（显式跨 agent）。缺省时由 `ATB_MULTI_SCOPE` 决定。
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

/// 多标签并发执行：一次请求完成「解析目标 → 并发执行 → 按输入顺序汇总」。
///
/// 与 `/batch` 的分工：`/batch` 面向「显式列出每一步（可跨 op）」，
/// `/multi` 面向「同一条命令打在多个标签页上」——这是 Agent 最常见的批量形态，
/// 不需要构造 steps 文件。
pub(crate) async fn multi(
    State(state): State<AppState>,
    Json(req): Json<MultiRequest>,
) -> Json<Value> {
    touch(&state).await;
    Json(multi_value(&state, req).await)
}

pub(crate) async fn multi_value(state: &AppState, req: MultiRequest) -> Value {
    if !JOB_OPS.contains(&req.op.as_str()) {
        return json!({
            "ok": false,
            "error": format!("multi: 不支持 op {}", req.op),
            "error_code": "unsupported_op",
            "hint": "只有页面级命令可以一次打多个标签页；用 `atb ops` 查可用命令",
            "supported": JOB_OPS,
        });
    }
    if !req.args.is_null() && !req.args.is_object() {
        return json!({
            "ok": false,
            "error": "multi: args 必须是 JSON 对象",
            "error_code": "invalid_arguments",
            "hint": "args 里放这条命令的参数（不需要自己写 switch_tab_id，daemon 会按目标注入）",
        });
    }

    let spec = if req.tab_ids.is_empty() {
        match TargetSpec::parse(req.target.as_deref().unwrap_or("")) {
            Ok(spec) => spec,
            Err(err) => {
                return json!({
                    "ok": false,
                    "error": err.message,
                    "error_code": err.code,
                    "hint": err.hint,
                    "example": err.example,
                });
            }
        }
    } else {
        TargetSpec::Ids(req.tab_ids.clone())
    };

    // 有些选择器依赖新版扩展上报的字段（window / active / group）。
    // 旧扩展上直接报「找不到」，用户无从判断是版本问题还是真没有——
    // 所以在动手前先做能力检查，并给出可执行的修复步骤。
    if let Some((feature, min_protocol)) = spec_requires_protocol(&spec) {
        if let Err(error) = ensure_extension_protocol(state, min_protocol, feature).await {
            return error;
        }
    }

    // TA：默认只解析本 agent 打开的标签（多智能体共用同一浏览器时不串线）；
    // 只有显式 `scope:"all"`（或 ATB_MULTI_SCOPE=all）才跨 agent。
    let scope_all = match req.scope.as_deref() {
        Some("all") => true,
        Some("agent") => false,
        Some(other) => {
            return json!({
                "ok": false,
                "error": format!("multi: 未知 scope {other:?}"),
                "error_code": "invalid_scope",
                "hint": "scope 只接受 \"agent\"（默认，只操作本智能体 open 的标签）或 \"all\"（显式跨智能体）",
            });
        }
        None => crate::settings::multi_scope_default() == "all",
    };
    let resolved = match resolve_targets(
        state,
        &spec,
        req.browser.as_deref(),
        req.profile.as_deref(),
        !scope_all,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(err) => return err.to_json(),
    };
    let skipped_foreign = resolved.skipped_foreign;
    let targets = resolved.targets;
    let scope_label = if scope_all { "all" } else { "agent" };
    // `@N` 语义提示。目标数 > 1 时序号必然是「一批里的第 N 个」，很容易
    // 选错；标签总数超过阈值时单目标也一样容易选错（`tabs` 列表太长看不完）。
    let index_target_warning = if matches!(&spec, TargetSpec::Index(_)) {
        let total_tabs = state
            .driver
            .read()
            .await
            .sessions
            .values()
            .filter(|session| session.is_active())
            .count();
        (targets.len() > 1 || total_tabs > crate::settings::tabs_large_warning())
            .then_some(INDEX_TARGET_WARNING)
    } else {
        None
    };

    let plan: Vec<Value> = targets
        .iter()
        .map(|target| {
            json!({
                "index": target.index,
                "tab_id": target.tab_id,
                "title": truncate_for_tree(&target.title, 80),
                "url": truncate_for_tree(&target.url, 120),
                "active": target.active,
                "group": target.group_title,
            })
        })
        .collect();

    if req.dry_run {
        let mut payload = json!({
            "ok": true,
            "dry_run": true,
            "op": req.op,
            "target": spec.describe(),
            "scope": scope_label,
            "skipped_foreign": skipped_foreign,
            "count": targets.len(),
            "targets": plan,
        });
        if let Some(warning) = index_target_warning {
            payload["target_warning"] = json!(warning);
        }
        return payload;
    }

    let started = Instant::now();
    let concurrency = req
        .concurrency
        .unwrap_or_else(|| targets.len().min(8))
        .clamp(1, 32)
        .min(targets.len().max(1));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let agent = current_agent();
    let mut set = tokio::task::JoinSet::new();

    for target in targets {
        let state = state.clone();
        let op = req.op.clone();
        let base_args = req.args.clone();
        let permits = semaphore.clone();
        let stop_flag = stop_flag.clone();
        let stop_on_error = req.stop_on_error;
        let agent = agent.clone();
        set.spawn(with_agent(agent, async move {
            if stop_flag.load(Ordering::Relaxed) {
                return (
                    target,
                    Err(anyhow!("已跳过：前面的步骤失败（stop_on_error）")),
                    0u64,
                );
            }
            // 拿不到许可不能「无许可继续执行」：那会让 concurrency 形同虚设，
            // 几十个目标一起压浏览器（同类点，open/run 已修，这里补齐）。
            let _permit = match permits.acquire_owned().await {
                Ok(permit) => permit,
                Err(err) => {
                    eprintln!(
                        "multi: 目标 {} 并发许可获取失败（semaphore closed），已跳过: {err}",
                        target.session_key
                    );
                    return (
                        target,
                        Err(anyhow!(
                            "concurrency_closed: 内部并发控制器已关闭，该目标未执行，可重试"
                        )),
                        0u64,
                    );
                }
            };
            // **拿到许可之后**必须再查一次停止位。
            //
            // 旧实现只在任务开头查一次，而目标是一次性全部 spawn 的：目标多、并发低时
            // 大部分任务会排在 semaphore 队列里，等它们拿到许可时前面早就有失败发生了，
            // 却依然会继续执行——`stop_on_error` 在排队场景下等于没有。
            if stop_on_error && stop_flag.load(Ordering::Relaxed) {
                return (
                    target,
                    Err(anyhow!("已跳过：前面的步骤失败（stop_on_error）")),
                    0u64,
                );
            }
            let mut args = if base_args.is_null() {
                json!({})
            } else {
                base_args
            };
            if let Some(object) = args.as_object_mut() {
                object.insert(
                    "switch_tab_id".to_string(),
                    json!(target.session_key.clone()),
                );
                object.remove("browser");
                object.remove("profile");
                expand_output_placeholders(object, &target);
            }
            let step_started = Instant::now();
            let outcome = run_job(&state, &op, args, Arc::new(AtomicBool::new(false))).await;
            if stop_on_error {
                // `run_job` 的 Err 只是「一种失败」；`wait` 超时、页面脚本 ok:false 都会
                // 以 Ok(ok:false) 回来，旧实现只认 Err，于是这些失败
                // 不会触发停止位，后续目标照常执行。
                let failed = match &outcome {
                    Err(_) => true,
                    Ok(value) => value.get("ok").and_then(Value::as_bool) == Some(false),
                };
                if failed {
                    stop_flag.store(true, Ordering::Relaxed);
                }
            }
            (target, outcome, step_started.elapsed().as_millis() as u64)
        }));
    }

    let mut slots: Vec<Option<Value>> = vec![None; plan.len()];
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    while let Some(joined) = set.join_next().await {
        let (target, outcome, elapsed_ms) = match joined {
            Ok(value) => value,
            Err(err) => {
                failed += 1;
                if let Some(slot) = slots.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(json!({
                        "index": null,
                        "ok": false,
                        "error": format!("任务异常退出：{err}"),
                        "error_code": "task_panicked",
                        // 与 `atb schema` 的 error_codes 声明对齐（cli 域）：复核后可重试。
                        "retryable": true,
                        "hint": hint_for_error("task_panicked"),
                    }));
                }
                continue;
            }
        };
        let entry = match outcome {
            Ok(value) => {
                // 内层 ok:false（wait 超时等）不能计入 succeeded（同族）。
                let inner_ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                if inner_ok {
                    succeeded += 1;
                } else {
                    failed += 1;
                }
                let mut entry = json!({
                    "index": target.index,
                    "tab_id": target.tab_id,
                    "title": truncate_for_tree(&target.title, 80),
                    "url": truncate_for_tree(&target.url, 120),
                    "ok": inner_ok,
                    "elapsed_ms": elapsed_ms,
                    "result": value,
                });
                if !inner_ok {
                    let message = entry
                        .get("result")
                        .and_then(|value| value.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("目标未达成")
                        .to_string();
                    entry["error"] = json!(message);
                    entry["error_code"] = json!(classify_error(&message));
                    if let Some(hint) = hint_for_error(&message) {
                        entry["hint"] = json!(hint);
                    }
                }
                entry
            }
            Err(err) => {
                failed += 1;
                let message = err.to_string();
                let mut entry = json!({
                    "index": target.index,
                    "tab_id": target.tab_id,
                    "title": truncate_for_tree(&target.title, 80),
                    "url": truncate_for_tree(&target.url, 120),
                    "ok": false,
                    "elapsed_ms": elapsed_ms,
                    "error": message,
                    "error_code": classify_error(&message),
                });
                if let Some(hint) = hint_for_error(&message) {
                    entry["hint"] = json!(hint);
                }
                entry
            }
        };
        if let Some(slot) = slots.get_mut(target.index) {
            *slot = Some(entry);
        }
    }
    let results: Vec<Value> = slots.into_iter().flatten().collect();
    let mut payload = json!({
        "ok": failed == 0 && succeeded > 0,
        "op": req.op,
        "target": spec.describe(),
        // TA：回显解析范围与「因归属被跳过」的数量。默认 agent 范围下
        // skipped_foreign>0 表示「还有别的智能体的标签没被操作」，不是失败。
        "scope": scope_label,
        "skipped_foreign": skipped_foreign,
        "count": results.len(),
        "succeeded": succeeded,
        "failed": failed,
        "concurrency": concurrency,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "results": results,
    });
    if let Some(warning) = index_target_warning {
        payload["target_warning"] = json!(warning);
    }
    payload
}

/// 截图 / PDF 的输出路径支持 `{tab}` 与 `{index}` 占位符。
///
/// 不加这个，`--tab all --out shot.png` 会让所有标签页抢同一个文件，
/// 最终只剩一张图——而这正是批量截图最常见的写法。
pub(crate) fn expand_output_placeholders(
    object: &mut serde_json::Map<String, Value>,
    target: &ResolvedTarget,
) {
    let Some(out) = object.get("out").and_then(Value::as_str) else {
        return;
    };
    if !out.contains("{tab}") && !out.contains("{index}") {
        return;
    }
    let expanded = out
        .replace("{tab}", &target.tab_id)
        .replace("{index}", &target.index.to_string());
    object.insert("out".to_string(), json!(expanded));
}

/// 把错误文本归类成稳定的 error_code，脚本可以按码分支，不必匹配中文字符串。
pub(crate) fn classify_error(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    if lower.contains("超时") || lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("未连接") || lower.contains("not connected") {
        "tab_disconnected"
    } else if lower.contains("未找到") || lower.contains("not found") || lower.contains("matched 0")
    {
        "element_not_found"
    } else if lower.contains("csp") || lower.contains("cannot access") {
        "csp_blocked"
    } else if lower.contains("script_error") || lower.contains("script failed") {
        "script_error"
    } else {
        "operation_failed"
    }
}

/// 常见失败的「下一步」提示。没有把握时返回 None，不硬编一条空话。
pub(crate) fn hint_for_error(message: &str) -> Option<&'static str> {
    let lower = message.to_lowercase();
    // panic 归 `task_panicked` 后必须有自己的 hint（§12.2：每个 error_code
    // 都要能告诉调用方下一步）。文案必须与「超时」区分开：panic 不是"再等等就好"。
    if lower.contains("task_panicked") || lower.contains("任务异常退出") {
        Some("这是 atb 的 bug（任务 panic）：附上 daemon 日志反馈；先用 tabs 复核目标状态，不要立刻整批重试")
    } else if lower.contains("未连接") || lower.contains("not connected") {
        Some("标签页可能已关闭或扩展刚断连；用 `tabs` 复核目标，再用 --tab 重新指定")
    } else if lower.contains("超时") || lower.contains("timed out") {
        Some("页面可能还在加载；加 --timeout，或用 `wait --selector` 先等元素出现")
    } else if lower.contains("未找到") || lower.contains("matched 0") {
        Some("用 `find` 先确认选择器能命中，再用返回的 selector 操作")
    } else if lower.contains("csp") || lower.contains("cannot access") {
        Some("页面 CSP 限制了注入；改用封装命令（read/snapshot/click），或加 --world isolated")
    } else {
        None
    }
}
