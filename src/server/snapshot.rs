use super::*;

/// 分页窗口右端（`offset + limit`）。
///
/// `offset` 来自用户 JSON 且没有上限：debug 构建下 `usize::MAX + 1` 会 panic，
/// release 会回绕，导致 `has_more` / `next_offset` 全错。
/// 这里用 `checked_add`，并保留一个宽松上限把明显越界的请求直接拒掉。
pub(crate) const MAX_SNAPSHOT_OFFSET: usize = 1_000_000;

/// 页面脚本单次最多回传的条目数（`page_snapshot.js` 里的硬上限）。
///
/// 这个常量不是「offset 上限」：Rust 侧会多要一条（`offset + limit + 1`）来确认
/// 「是否真的还有下一批」。页面脚本支持 `offset` 时，offset 可以超过这个值；
/// 只有旧版脚本（不支持 offset、总是从文档开头取）才受它限制 —— 那种情况下
/// `offset >= 1000` 无法取到数据，Rust 侧会显式报错而不是返回空页。
pub(crate) const PAGE_SNAPSHOT_MAX_ITEMS: usize = 1000;

pub(crate) fn page_window(offset: usize, limit: usize) -> Result<usize> {
    if offset > MAX_SNAPSHOT_OFFSET {
        return Err(anyhow!(
            "offset 过大（{offset}），上限 {MAX_SNAPSHOT_OFFSET}"
        ));
    }
    offset
        .checked_add(limit)
        .ok_or_else(|| anyhow!("offset 与 limit 之和过大（{offset} + {limit}）"))
}

/// 从 `batch` 的单步结果里取 CDP 原始回包。
///
/// 扩展把 `data` 作为规范位置，同时把 CDP 顶层字段并进条目（兼容旧读法）；
/// 两个位置都要认，否则换个扩展版本就取不到 nodes。
fn batch_step_payload(step: &Value) -> Option<&Value> {
    step.get("data").or_else(|| step.get("result"))
}

/// 一次往返取回 AX 树 + 实时 `location.href`。
///
/// 为什么必须同一次往返：分别调用时页面可以在两次之间导航，拿到的树与 URL 属于
/// 不同文档，`@e` 的 URL 基准就错了。batch 把两条 CDP 命令放进同一次
/// `chrome.debugger` 往返，页面侧不存在「中间再导航」的窗口。
///
/// 失败降级：老扩展不支持 batch、或 `Runtime.evaluate` 被页面策略拒绝时，退回
/// 单条 `getFullAXTree`（与修复前一致），URL 由调用方回落到 `tab.url`。
async fn cdp_ax_tree_with_url(
    state: &AppState,
    session_key: &str,
    timeout: Duration,
) -> (Result<CdpCallResult>, Option<String>) {
    let commands = vec![
        json!({ "cmd": "cdp", "method": "Accessibility.getFullAXTree", "params": {} }),
        json!({
            "cmd": "cdp",
            "method": "Runtime.evaluate",
            "params": { "expression": "location.href", "returnByValue": true }
        }),
    ];
    if let Ok(value) = batch_cdp_call(state, session_key, commands, timeout).await {
        if let Some(results) = value.get("results").and_then(Value::as_array) {
            let ax_data = results.first().and_then(batch_step_payload).cloned();
            let live_url = results
                .get(1)
                .and_then(batch_step_payload)
                .and_then(|data| data.get("result"))
                .and_then(|result| result.get("value"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_string);
            let ax = CdpCallResult {
                ok: ax_data.is_some(),
                data: ax_data,
                error: None,
            };
            return (Ok(ax), live_url);
        }
    }
    (
        cdp_call(
            state,
            session_key,
            "Accessibility.getFullAXTree",
            json!({}),
            timeout,
        )
        .await,
        None,
    )
}

pub(crate) async fn snapshot_page(state: &AppState, req: SnapshotRequest) -> Result<Value> {
    let limit = req.limit.clamp(1, 1000);
    // 先算窗口右端：两条路径（CDP / JS）都用它，越界或溢出在这里就失败。
    let end = page_window(req.offset, limit)?;
    if req.js {
        return snapshot_via_js(state, &req).await;
    }
    let tab_id = select_tab(
        state,
        SessionSelector::new(
            req.switch_tab_id.clone(),
            req.browser.clone(),
            req.profile.clone(),
        ),
    )
    .await?;
    let timeout = secs(req.timeout.max(0.1));
    // TA：不再 `select_tab → active_tabs（全表收集）→ find`。目标已经在上面
    // 选定，这里直接按 key 取，少一次全量快照、也少一个「选中后列表又变了」的窗口。
    let tab = active_tab_by_key(state, &tab_id).await?;
    // TA：AX 树与 `location.href` 在**同一次**往返里取回。旧实现用
    // `active_tabs` 里的 `session.info.url`（driver 侧快照，可能是上一次 tabs 推送的
    // 旧值）作为 `SnapshotCache.url`，于是同一份 AX 树配了一个滞后 URL：
    // 页面导航后 `@e` 校验会认为 URL 没变（或变了），误判成 ref_expired / 不该重定位。
    let (ax, live_url) = cdp_ax_tree_with_url(state, &tab_id, timeout).await;
    let ax = match ax {
        Ok(value) if value.ok => value,
        other => {
            let reason = match other {
                Ok(value) => value
                    .error
                    .unwrap_or_else(|| "Accessibility.getFullAXTree failed".to_string()),
                Err(err) => err.to_string(),
            };
            return snapshot_fallback_to_js(state, &req, reason).await;
        }
    };
    let nodes_value = match ax.data.and_then(|v| v.get("nodes").cloned()) {
        Some(value) => value,
        None => {
            return snapshot_fallback_to_js(state, &req, "CDP response missing nodes".to_string())
                .await
        }
    };
    let nodes: Vec<AxNode> = match serde_json::from_value(nodes_value) {
        Ok(nodes) => nodes,
        Err(err) => {
            return snapshot_fallback_to_js(state, &req, format!("parse AX nodes: {err}")).await
        }
    };
    let mut builder = SnapshotBuilder::new(req.offset, limit, req.details);
    let tree = builder.build(nodes);
    let mut all_refs = builder.refs;
    // 详情收集只在显式请求 `--details` 时执行。旧实现不看 `req.details`、
    // 对窗口内全部元素无条件发起最多 25 批 `DOM.resolveNode + Runtime.callFunctionOn`
    // CDP 往返，结果只写进快照缓存的 `entry.dom` —— 而该字段唯一的消费点是
    // `attach_details_to_tree`（同样只在 `--details` 时调用），不带 `--details`
    // 时这批往返纯属热路径浪费。输出形状兼容：不带 `--details` 时树本来就不挂
    // 详情，`pagination.details_truncated` 只在「确实收集过且被截断」时出现。
    let (detail_map, details_truncated) = if req.details {
        let refs_for_details: Vec<_> = all_refs
            .values()
            .filter(|entry| entry.index >= req.offset && entry.index < end)
            .cloned()
            .collect();
        collect_dom_details(state, &tab_id, &refs_for_details, req.details, timeout).await
    } else {
        (HashMap::new(), false)
    };
    for (ref_id, detail) in detail_map {
        if let Some(entry) = all_refs.get_mut(&ref_id) {
            entry.dom = Some(detail);
        }
    }
    let tree = if req.details {
        attach_details_to_tree(tree, &all_refs)
    } else {
        tree
    };
    let tree = if req.interactive_only {
        prune_tree_keep_refs(tree)
    } else {
        tree
    };
    // 本页窗口内的引用总数（fold 之前）。`max_nodes` 会把树裁短，必须和
    // 实际返回量分开记，否则「树被裁剪」在输出里完全看不出来。
    let window_refs = all_refs
        .values()
        .filter(|entry| entry.index >= req.offset && entry.index < end)
        .count();
    let tree = fold_tree(tree, req.max_nodes);
    let total = all_refs.len();
    // returned 与 JS 路径同口径：树里实际带 ref 的节点数（fold_tree 之后）。
    let returned = count_tree_refs(&tree);
    let tree_truncated = returned < window_refs;
    // TA：优先用同一次往返里读到的实时 `location.href`；只有它拿不到
    // （旧扩展 / Runtime.evaluate 被页面策略拒绝）才回落到 driver 会话里的 URL，
    // 并用 `url_source` 如实标注取的是哪一种，下游 `@e` 校验可按来源决定是否重定位。
    let live_url_present = live_url.is_some();
    let (snapshot_url, url_source) = match live_url {
        Some(url) => (url, "live"),
        None => (tab.url.clone(), "driver"),
    };
    {
        let mut driver = state.driver.write().await;
        // TA × 联动：把同一次往返读到的**实时** URL 回写会话。
        //
        // `write_back_relocated_selector`（cdp.rs）在用 `session.info.url` 与快照缓存的
        // `expected_url` 做实时比对：只把 live URL 写进 `SnapshotCache` 而不回写
        // `Session.info.url`，消费侧仍会拿滞后值比对，保护等于没做。
        // `Session.info` 是会话侧唯一副本（`active_tabs` 每次都从它 clone），
        // 因此这里更新它就是全局生效；只有确实拿到 live URL 时才写，避免用
        // 回落值把已有值覆盖成同一个滞后值。
        if live_url_present {
            if let Some(session) = driver.sessions.get_mut(&tab_id) {
                session.info.url = snapshot_url.clone();
            }
        }
        let generation = driver
            .snapshots
            .get(&tab_id)
            .map(|cache| cache.generation + 1)
            .unwrap_or(1);
        driver.snapshots.insert(
            tab_id.clone(),
            SnapshotCache {
                generation,
                url: snapshot_url.clone(),
                refs: all_refs,
            },
        );
    }
    // has_more 要同时反映两件事：还有下一页（end < total）、以及这一页的树被
    // `max_nodes` 裁短（tree_truncated）。只看前者时「树被静默裁剪」不会出现在输出里。
    let has_more = end < total || tree_truncated;
    let mut pagination = json!({
        "total_operable": total,
        "offset": req.offset,
        "limit": limit,
        // returned：树里实际带 @e 的节点数（fold_tree 之后，与 JS 路径同口径）。
        "returned": returned,
        // window_refs：本页窗口内应有的引用数；比 returned 大说明树被 max_nodes 裁剪。
        "window_refs": window_refs,
        "tree_truncated": tree_truncated,
        "has_more": has_more,
        // next_offset 只跟「还有下一页」走：树被裁剪不是分页问题，
        // 否则调用方会顺着 next_offset 翻到一页空数据。
        "next_offset": if end < total { Some(end) } else { None::<usize> }
    });
    if details_truncated {
        // 详情按整体 deadline 收集，超时后不再下钻；必须显式标记，不能静默缺字段。
        pagination["details_truncated"] = json!(true);
    }
    Ok(json!({
        "status": "success",
        "mode": "cdp",
        // `@e` 的编号口径随快照引擎不同。CDP 只给可操作角色发引用
        // （`ref_scheme=operable-only`），JS 回退给所有有名字的节点发引用
        // （`ref_scheme=all-named`）。两个引擎的 `@e5` 不保证指向同一元素，
        // 跨引擎复用引用前必须重新 snapshot。显式回传口径，调用方才有可能判断。
        "ref_scheme": "operable-only",
        // TA：`url` 与 `SnapshotCache.url` 同源（优先实时 location.href），
        // `url_source` 标明来源（live = 本次页面实时读取；driver = 回落到会话快照）。
        "url": snapshot_url,
        "url_source": url_source,
        "title": tab.title,
        "tab_id": tab.tab_id,
        "session_key": tab_id,
        "tree": tree,
        "pagination": pagination
    }))
}

/// CDP 不可用时的回退：仍然产出 `@e` 引用，但引用带 CSS 选择器，
/// 后续动作全部走页面内执行，不再依赖 debugger。
pub(crate) async fn snapshot_fallback_to_js(
    state: &AppState,
    req: &SnapshotRequest,
    reason: String,
) -> Result<Value> {
    let reason = humanize_error(&reason);
    let mut value = match snapshot_via_js(state, req).await {
        Ok(value) => value,
        Err(err) => {
            return Err(anyhow!(
                "snapshot 失败：CDP 不可用（{reason}）；DOM 回退也失败（{}）。该页面同时禁用了 debugger 与 eval，请关闭在页面里注入 iframe 的其他扩展后再试",
                humanize_error(&err.to_string())
            ))
        }
    };
    value["fallback_reason"] = json!(reason);
    value["hint"] = json!("CDP 不可用（常见原因：页面内嵌其他扩展的 iframe），已回退到 DOM 快照；@e 可直接用于 click/fill/hover/select/check/scroll");
    Ok(value)
}

/// 扩展返回的错误经常是嵌套 JSON 字符串（`{"error":{"message":...}}`）。
/// 剥出人类可读的 message，避免把整段 JSON 塞进提示里。
pub(crate) fn humanize_error(input: &str) -> String {
    let mut text = input.trim().to_string();
    for _ in 0..3 {
        if !text.starts_with('{') {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            break;
        };
        let next = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
            .or_else(|| value.get("error").and_then(Value::as_str))
            .map(str::to_string);
        match next {
            Some(inner) => text = inner,
            None => break,
        }
    }
    text
}

/// 页面 CSP 禁止 eval 时扩展返回的错误特征。
///
/// 只认 CSP 相关的**精确短语**：裸子串 `csp` 会命中 `#csp-banner not found`、
/// `/csp-report`、`csp.example.com` 这类普通失败。一旦误判成 CSP 限制，
/// `page_action_with_fallback` / `run_target_operation` / `*_page_call` 会换通道
/// **再执行一次**，提交类按钮就被点了两次。
///
/// 同理**不能**只凭 `EvalError`（`evalerror`）判定：页面自己的脚本抛 `EvalError`
/// （如 `EvalError: 余额不足`）也会命中，点击类动作因此不可重入、按钮被点两次。
/// 只保留「EvalError + CSP 语境」的组合短语。
pub(crate) fn is_csp_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    const NEEDLES: [&str; 7] = [
        "unsafe-eval",
        "content security policy",
        "refused to evaluate",
        "refused to compile",
        "eval is disabled",
        // Chrome 关掉 eval 时抛的名字是 `EvalError`，但必须带上 CSP 的 `Refused`：
        // 页面自身抛的裸 `EvalError` 不是 CSP，不能触发「换通道再执行一次」。
        "evalerror: refused",
        // 部分浏览器的措辞（"call to Function() blocked by CSP"）：整句是 CSP 专属，
        // 不会像裸 "csp" 那样命中普通内容。
        "blocked by csp",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// 无 eval 通道：让扩展注入固定的页面脚本文件并调用其全局函数。
/// 页面 CSP 禁止 eval 时，read / find / snapshot(js) / action 仍可用（DOM 不受 CSP 限制）。
pub(crate) async fn page_op_via_extension(
    state: &AppState,
    session_key: &str,
    op: &str,
    args: Value,
    frame_id: Option<i64>,
    timeout: Duration,
) -> Result<Value> {
    let mut body = json!({ "op": op, "args": args });
    // 扩展侧 buildScriptTarget 认 frameId：不传就会在整个页面的主框架里执行。
    if let Some(frame_id) = frame_id {
        body["frameId"] = json!(frame_id);
    }
    let value = extension_control(state, session_key, "pageOp", body, timeout).await?;
    let data = value.get("data").cloned().unwrap_or(value);
    let Some(payload) = data.get("value").cloned() else {
        // 扩展没带 `value` 说明这一路根本没执行到页面脚本（SW 刚被回收、debugger 掉线、
        // frameId 失效……）。以前这里解成 `Ok(Null)`，而 `Null` 上 `get("ok")` 是 `None`，
        // 于是所有宽容判据都放行（机制 A）。
        return Err(anyhow!("pageOp({op}): 扩展没有返回 value，页面脚本未执行"));
    };
    // wait 的「未命中」是正常结果（ok:false + matched:false），不能当成命令失败。
    // 判据必须是「带 matched 字段」而不是「op == wait」：challenge 的
    // `action=wait`也走这条
    // 通道，只有无 matched 的 ok:false 才代表页面脚本/通道真的坏了。别收窄成按 op 判断，
    // 否则 challenge 超时会被误报成通道错误，daemon 侧就攒不出 first_blocking_at。
    let has_match_field = payload.get("matched").is_some();
    if !has_match_field && payload.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(anyhow!(
            "pageOp({op}): {}",
            payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("page script failed")
        ));
    }
    Ok(payload)
}

/// 把会话的 `tab_id` 转成扩展协议里的数字 `tabId`。
///
/// **空串返回 `Err(session_gone)`**（历史上曾返回 `Ok(0)`，与本函数自己的
/// 「必须 > 0」校验自相矛盾）。真正不需要 tabId 的路径（`exec` 等）在更上层
/// 就不调用本函数；只要调用到这里，空 tab_id 一定是会话记录坏了。
/// 非空但解析不出正整数的**不能**静默降级成 0——扩展会回 "tabId is required"，
/// 用户看到的归因变成"扩展要求 tabId"，而不是"本地会话的 tab_id 坏了"。
pub(crate) fn parse_tab_id(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // 空串 = 会话记录已失效（扩展重载 / 标签关闭后 tab_id 丢了）。
        // 报「会话的 tab_id 为空」并由上游映射成 session_gone，而不是让扩展报
        // 「tabId 必填」——后者会把本地状态问题归因到扩展协议上。
        return Err(anyhow!(
            "会话的 tab_id 为空：会话可能已断开，请 refresh 或重新 open"
        ));
    }
    match trimmed.parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(anyhow!(
            "会话的 tab_id 非法（{raw}），请 refresh 或重新 open"
        )),
    }
}

pub(crate) fn tab_id_of_session(session_key: &str) -> Value {
    json!(session_key.rsplit(':').next().unwrap_or_default())
}

/// `@eN` → 全页 0 基序号（`@e1` 是 0）。
fn snapshot_ref_index(ref_id: &str) -> Option<usize> {
    ref_id
        .strip_prefix("@e")?
        .parse::<usize>()
        .ok()
        .filter(|number| *number > 0)
        .map(|number| number - 1)
}

/// 判断这一批条目在全页序号里的起点（0 = 旧版脚本，总是从文档开头取）。
///
/// 新版页面脚本会认 `offset`，Rust 需要它才能把「批内位置」换算成全页序号；
/// 旧版脚本不回 offset、ref 也总是从 `@e1` 开始，起点就是 0。
fn snapshot_items_base_offset(payload: &Value, items: &[Value], offset: usize) -> usize {
    if offset == 0 {
        // 第一页两种脚本行为一致，起点必然是 0。
        return 0;
    }
    // 1) 页面显式回传 offset / has_more / next_offset：说明它已经支持分页窗口。
    if let Some(echoed) = payload.get("offset").and_then(Value::as_u64) {
        return usize::try_from(echoed).unwrap_or(0);
    }
    if payload.get("has_more").is_some() || payload.get("next_offset").is_some() {
        return offset;
    }
    // 2) 首个 ref 的编号：支持 offset 的脚本保持全页编号，第 offset 条就是 `@e{offset+1}`。
    let first = items
        .first()
        .and_then(|item| item.get("ref"))
        .and_then(Value::as_str)
        .and_then(snapshot_ref_index);
    if first == Some(offset) {
        offset
    } else {
        0
    }
}

pub(crate) async fn snapshot_via_js(state: &AppState, req: &SnapshotRequest) -> Result<Value> {
    let limit = req.limit.clamp(1, 1000);
    let offset = req.offset;
    let session_key = select_tab(
        state,
        SessionSelector::new(
            req.switch_tab_id.clone(),
            req.browser.clone(),
            req.profile.clone(),
        ),
    )
    .await?;
    let timeout = secs(req.timeout.max(0.1));
    let end = page_window(offset, limit)?;
    // 多要一条：页面脚本的 `truncated` 是 `items.length >= maxItems`，恰好取满时也是
    // true，只看它会让 `has_more` 永远为真、每页都空。多取一条才能区分
    // 「刚好取完」与「确实还有下一批」。
    let probe = end.saturating_add(1).min(PAGE_SNAPSHOT_MAX_ITEMS);
    let opts = json!({
        // offset / limit 一起下发：新版页面脚本按 offset 取窗口；旧版脚本忽略 offset、
        // 从文档开头取 limit 条，Rust 侧再按 `index >= offset` 本地切片兜底。
        // 两条路径都靠「多要一条」判断 has_more。
        "offset": offset,
        "limit": probe,
        "interactiveOnly": req.interactive_only,
        // 接通页面侧 skipGeneric 开关。None 序列化成 null，页面脚本
        // `opts.skipGeneric !== false` 判定为 true（保持默认跳过 generic），
        // 请求体显式带 skipGeneric:false 时保留 generic 角色节点。
        "skipGeneric": req.skip_generic
    });
    let script = format!(
        "{}\nreturn __abcSnapshot({});",
        html::js_page_snapshot(),
        opts
    );
    let executed = execute_page_js(
        state,
        &script,
        SessionSelector::new(Some(session_key.clone()), None, None),
        true,
        timeout,
    )
    .await;
    let (payload, ref_tab_id) = match executed {
        Ok(result) => (
            result.get("js_return").cloned().unwrap_or(Value::Null),
            result.get("tab_id").cloned().unwrap_or(Value::Null),
        ),
        Err(err) if is_csp_error(&err.to_string()) => {
            let payload =
                page_op_via_extension(state, &session_key, "snapshot", opts.clone(), None, timeout)
                    .await?;
            (payload, tab_id_of_session(&session_key))
        }
        Err(err) => return Err(err),
    };
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(anyhow!(
            "snapshot(js): {}",
            humanize_error(
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("page script failed")
            )
        ));
    }
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = items.len();
    // 这一批条目在**全页序号**里的起点。新版页面脚本认 offset，回包的首个 ref 就是
    // `@e{offset+1}`（或显式回传 offset）；旧版脚本永远从文档开头取，起点是 0。
    // 以前不看起点、一律按 `index >= offset` 过滤，于是 offset≥1000 时页面只给
    // 前 1000 条、过滤后一条不剩。
    let base_offset = snapshot_items_base_offset(&payload, &items, offset);
    if base_offset == 0 && offset >= PAGE_SNAPSHOT_MAX_ITEMS {
        // 旧版页面脚本只回前 1000 条，再大的 offset 无论怎么切片都取不到数据。
        // 明确报错，而不是返回空树 + `has_more: true` 让调用方无限翻页。
        return Err(anyhow!(
            "snapshot --js: 当前页面脚本不支持 offset 分页（只回传前 {PAGE_SNAPSHOT_MAX_ITEMS} 条），offset={offset} 无法取到数据；请去掉 --js 改走 CDP 快照，或降低 offset"
        ));
    }
    let mut refs: HashMap<String, ElementRef> = HashMap::new();
    let mut tree: Vec<Value> = Vec::new();
    for (position, item) in items.iter().enumerate() {
        // 全页序号 = 批次起点 + 批内位置；窗口过滤、详情补全都按全页序号判断，
        // 不再依赖「Rust 拿到的这一批一定从 0 开始」这个旧假设。
        let index = base_offset.saturating_add(position);
        let ref_id = item
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if ref_id.is_empty() {
            continue;
        }
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("generic")
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let selector = item
            .get("selector")
            .and_then(Value::as_str)
            .map(str::to_string);
        // 同一批里 ref 可能重复（页面脚本 renumber 时跨批重号）：保留先出现的那个，
        // 避免后一条覆盖前一条的 index，导致窗口过滤错位。
        refs.entry(ref_id.clone()).or_insert(ElementRef {
            ref_id: ref_id.clone(),
            backend_dom_node_id: 0,
            selector: selector.clone(),
            index,
            role: role.clone(),
            name: name.clone(),
            value: item
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: None,
            dom: None,
        });
        if index < offset || index >= end {
            continue;
        }
        let mut node = serde_json::Map::new();
        node.insert("ref".to_string(), json!(ref_id));
        node.insert("role".to_string(), json!(role));
        if !name.is_empty() {
            node.insert("name".to_string(), json!(name));
        }
        for key in [
            "value", "href", "checked", "disabled", "level", "tag", "rect",
        ] {
            if let Some(existing) = item.get(key) {
                if !existing.is_null() {
                    node.insert(key.to_string(), existing.clone());
                }
            }
        }
        node.insert("selector".to_string(), json!(selector));
        tree.push(Value::Object(node));
    }
    // 本页窗口内应有的引用数（fold 之前）；被 `max_nodes` 裁掉多少靠它和 returned 对比。
    let window_refs = refs
        .values()
        .filter(|entry| entry.index >= offset && entry.index < end)
        .count();
    let mut tree = fold_tree(tree, req.max_nodes);
    let url = payload
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `--js --details`：JS 快照本身没有 DOM 详情。按 ref 上的 CSS 选择器在页面内
    // 批量补一次（一次注入描述全部元素），输出形状与 CDP 模式一致：详情直接挂到
    // 树节点的同名字段上（tag/text/placeholder/rect/visible...）。
    let mut details_state = Value::Null;
    if req.details {
        let targets: Vec<ElementRef> = refs
            .values()
            .filter(|entry| entry.index >= offset && entry.index < end)
            .cloned()
            .collect();
        match collect_dom_details_via_js(state, &session_key, &targets, timeout).await {
            Ok(map) => {
                for (ref_id, detail) in map {
                    if let Some(entry) = refs.get_mut(&ref_id) {
                        entry.dom = Some(detail);
                    }
                }
                tree = attach_details_to_tree(tree, &refs);
            }
            // 补详情失败不该让整条快照失败：标注清楚，让调用方知道「没有详情」。
            Err(err) => {
                details_state = json!({
                    "details_supported": false,
                    "hint": format!(
                        "--js 模式下补详情失败（{}）；可去掉 --js 改走 CDP 快照",
                        humanize_error(&err.to_string())
                    )
                });
            }
        }
    }
    // returned 与 CDP 路径同口径：树里实际带 @e 的节点数（fold_tree 之后）。
    let returned = count_tree_refs(&tree);
    let tree_truncated = returned < window_refs;
    {
        let mut driver = state.driver.write().await;
        let generation = driver
            .snapshots
            .get(&session_key)
            .map(|cache| cache.generation + 1)
            .unwrap_or(1);
        driver.snapshots.insert(
            session_key.clone(),
            SnapshotCache {
                generation,
                url: url.clone(),
                refs,
            },
        );
    }
    // 页面脚本只回传「抓到多少条」（受 limit 截断），并没有全页真实总数；
    // 但「本批覆盖到哪里」是确定的：批次起点 + 本批条数，多要的那一条若存在，
    // 覆盖范围就会越过 `end`，据此判断「是否真的还有下一批」。
    let truncated = payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let covered_end = base_offset.saturating_add(total);
    // 本批已经顶到页面侧单次上限：多要的那一条可能被页面钳掉，只能退回信 truncated。
    let capped = total >= PAGE_SNAPSHOT_MAX_ITEMS;
    let more_batches = if covered_end > end {
        true
    } else if capped {
        truncated
    } else {
        false
    };
    // has_more 同时反映「还有下一批」与「这一页的树被 max_nodes 裁剪」；
    // next_offset 只跟前者走，避免调用方按 next_offset 翻到一页空数据。
    let has_more = more_batches || tree_truncated;
    // 旧版页面脚本只能回到前 1000 条：`end >= 1000` 时下一页必然取不到数据，
    // 不给 next_offset（否则调用方会一路翻到报错）。新版脚本认 offset，不受此限。
    // 边界保护：第一页（offset==0）时 ref 编号无法区分「支持 offset 的新脚本」
    // 与「永远从文档开头取的旧脚本」——两者首个 ref 都是 @e1。旧实现无条件豁免
    // offset==0（`offset_supported` 恒真），于是旧脚本页面上 total 恰好被 1000 封顶时
    // 也给出 `next_offset`，调用方跟进后必然在 `snapshot_items_base_offset` 处报
    // 「不支持 offset 分页」。这里要求页面回包显式带 offset / has_more / next_offset
    // 字段才算声明支持分页窗口（与 `snapshot_items_base_offset` 的判据一致）；
    // `end < PAGE_SNAPSHOT_MAX_ITEMS` 的旧豁免一并收紧——旧脚本不管窗口多小，
    // 跟进任何 next_offset 都取不到数据，宁可少给一个 next_offset（调用方仍可
    // 手动传 --offset）也不要给一个必然报错的。
    let offset_support_evidenced = payload.get("offset").is_some()
        || payload.get("has_more").is_some()
        || payload.get("next_offset").is_some();
    let offset_supported = if offset == 0 {
        offset_support_evidenced
    } else {
        base_offset == offset
    };
    let can_advance = offset_supported;
    let mut pagination = json!({
        "scanned": total,
        "total_known": false,
        "offset": offset,
        "limit": limit,
        // returned：树里实际带 @e 的节点数（fold_tree 之后）；window_refs：本页应有的数量。
        "returned": returned,
        "window_refs": window_refs,
        "tree_truncated": tree_truncated,
        "truncated": truncated,
        "has_more": has_more,
        "next_offset": if more_batches && can_advance { Some(end) } else { None::<usize> }
    });
    if more_batches && !can_advance && pagination.get("hint").is_none() {
        pagination["hint"] = json!(concat!(
            "还有更多条目，但当前页面脚本只支持前 1000 条的窗口，无法继续翻页；",
            "去掉 --js 改走 CDP 快照可完整分页"
        ));
    }
    if tree_truncated {
        // 树被裁剪不是分页问题：调高 --max-nodes 或减小 --limit 才能拿到剩下的引用。
        pagination["hint"] = json!(format!(
            "本页窗口内有 {window_refs} 个引用，但树被 --max-nodes={} 裁到 {returned} 个；需要全部引用时请调大 --max-nodes",
            req.max_nodes
        ));
    }
    if !details_state.is_null() {
        pagination["details"] = details_state;
    }
    let mut out = json!({
        "status": "success",
        "mode": "js",
        // 把页面脚本声明的引用口径透出来。JS 回退给所有有名字的节点发 `@e`，
        // 与 CDP 的 `operable-only` 不是同一套编号；调用方跨引擎复用引用前必须重拍。
        "ref_scheme": payload
            .get("ref_scheme")
            .cloned()
            .unwrap_or_else(|| json!("all-named")),
        "url": url,
        "title": payload.get("title").cloned().unwrap_or(Value::Null),
        "tab_id": ref_tab_id,
        "session_key": session_key,
        "tree": tree,
        "pagination": pagination
    });
    // 页面脚本现在会声明 shadow DOM / iframe 的覆盖范围。这些字段必须
    // 原样透传，否则 daemon 侧看起来仍是「已扫描全部」，调用方无法知道
    // shadow 内元素到底可不可达。只在脚本确实给出时补，保持旧回包形状不变。
    if let Some(object) = out.as_object_mut() {
        for key in [
            "scope",
            "frame_count",
            "shadow_mode",
            "shadow_roots",
            "shadow_skipped",
            "shadow_closed_unreachable",
        ] {
            if let Some(value) = payload.get(key) {
                object.insert(key.to_string(), value.clone());
            }
        }
    }
    // 只回字段不够——调用方要能直接看出「哪些内容没覆盖到、下一步怎么办」。
    // 这些提示与 pagination.hint 分开（分页提示讲的是条目窗口，这里是覆盖范围）。
    let frame_count = out.get("frame_count").and_then(Value::as_u64).unwrap_or(0);
    let shadow_skipped = out
        .get("shadow_skipped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let closed_shadow = out
        .get("shadow_closed_unreachable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let declared_scope = out.get("scope").is_some();
    let mut coverage: Vec<String> = Vec::new();
    if frame_count > 0 {
        coverage.push(format!(
            "有 {frame_count} 个 iframe/frame 未被扫描（JS 快照只覆盖主框架）：改用 CDP 快照，或按 frameId 逐 frame 注入"
        ));
    }
    if shadow_skipped > 0 {
        coverage.push(format!(
            "有 {shadow_skipped} 个 open shadow root 被跳过（shadow_mode=off 或超过 roots/depth 上限）：调大上限后重拍，或改用 CDP（pierce）"
        ));
    }
    if closed_shadow {
        // ext-page 已把页面脚本的判据收窄成 `shadow_mode=auto && shadow_roots>0`：
        // 它表示「页面确实用了 open shadow root，因此可能还有枚举不到的 closed root」，
        // 不再是「任何 auto 页面都 true」。文案据此区分两种情形，仍保留「可能」——
        // 页面 JS 无法枚举 closed root，只能提示改用 CDP pierce。
        let shadow_roots = out.get("shadow_roots").and_then(Value::as_u64).unwrap_or(0);
        if shadow_roots > 0 {
            coverage.push(format!(
                "页面有 {shadow_roots} 个 open shadow root，且可能还有 closed root（页面 JS 不可达）：涉及 closed 内容时走 CDP pierce"
            ));
        } else {
            coverage.push(
                "疑似存在 closed shadow root（页面 JS 不可达）：需要时走 CDP pierce".to_string(),
            );
        }
    }
    if let Some(object) = out.as_object_mut() {
        if coverage.is_empty() {
            // 脚本明确声明过覆盖范围且没有任何缺口：回 true，调用方不必靠「字段缺失」
            // 去推断（旧版脚本不声明 scope，此时保持不回该字段，形状不变）。
            if declared_scope {
                object.insert("coverage_complete".to_string(), json!(true));
            }
        } else {
            object.insert("coverage_complete".to_string(), json!(false));
            object.insert("coverage_hint".to_string(), json!(coverage.join("；")));
        }
    }
    Ok(out)
}

pub(crate) struct SnapshotBuilder {
    pub(crate) offset: usize,
    pub(crate) limit: usize,
    pub(crate) operable_seen: usize,
    pub(crate) refs: HashMap<String, ElementRef>,
    pub(crate) nodes: HashMap<String, AxNode>,
}

impl SnapshotBuilder {
    pub(crate) fn new(offset: usize, limit: usize, _details: bool) -> Self {
        Self {
            offset,
            limit,
            operable_seen: 0,
            refs: HashMap::new(),
            nodes: HashMap::new(),
        }
    }

    pub(crate) fn build(&mut self, nodes: Vec<AxNode>) -> Vec<Value> {
        let mut child_ids = std::collections::HashSet::new();
        for node in &nodes {
            for child_id in &node.child_ids {
                child_ids.insert(child_id.clone());
            }
        }
        let mut roots: Vec<String> = nodes
            .iter()
            .filter(|node| !child_ids.contains(&node.node_id))
            .map(|node| node.node_id.clone())
            .collect();
        if roots.is_empty() {
            if let Some(first) = nodes.first() {
                roots.push(first.node_id.clone());
            }
        }
        self.nodes = nodes
            .into_iter()
            .map(|node| (node.node_id.clone(), node))
            .collect();
        roots
            .iter()
            .filter_map(|id| self.format_node(id, 0))
            .flat_map(|value| match value {
                Value::Array(items) => items,
                other => vec![other],
            })
            .collect()
    }

    pub(crate) fn format_node(&mut self, id: &str, depth: usize) -> Option<Value> {
        let (role, name, value, description, backend_dom_node_id, child_ids) = {
            let node = self.nodes.get(id)?;
            (
                ax_string(&node.role),
                ax_string(&node.name),
                ax_string(&node.value),
                ax_string(&node.description),
                node.backend_dom_node_id,
                node.child_ids.clone(),
            )
        };
        let mut children = Vec::new();
        // 深度上限：极端嵌套（或异常 AX 树里 child_ids 成环）时不继续下钻，
        // 只保留当前节点，避免无限递归把栈打爆。
        if depth < MAX_TREE_DEPTH {
            for child_id in child_ids {
                if let Some(child) = self.format_node(&child_id, depth + 1) {
                    match child {
                        Value::Array(items) => children.extend(items),
                        other => children.push(other),
                    }
                }
            }
        }
        let role_str = role.unwrap_or_default();
        let skip_role = role_str.is_empty() || role_str == "none" || role_str == "generic";
        let operable = is_operable_role(&role_str) && backend_dom_node_id.is_some();
        let mut obj = serde_json::Map::new();
        obj.insert("role".to_string(), json!(role_str));
        if let Some(name) = non_empty(name) {
            obj.insert("name".to_string(), json!(truncate_text(&name, 160)));
        }
        if let Some(value) = non_empty(value) {
            obj.insert("value".to_string(), json!(truncate_text(&value, 160)));
        }
        if let Some(description) = non_empty(description.clone()) {
            obj.insert(
                "description".to_string(),
                json!(truncate_text(&description, 160)),
            );
        }
        let mut include_self = false;
        if operable {
            self.operable_seen += 1;
            let index = self.operable_seen - 1;
            let ref_id = format!("@e{}", self.operable_seen);
            self.refs.insert(
                ref_id.clone(),
                ElementRef {
                    ref_id: ref_id.clone(),
                    backend_dom_node_id: backend_dom_node_id.unwrap_or_default(),
                    selector: None,
                    index,
                    role: role_str.clone(),
                    name: obj
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    value: obj.get("value").and_then(Value::as_str).map(str::to_string),
                    description,
                    dom: None,
                },
            );
            if index >= self.offset && index < self.offset.saturating_add(self.limit) {
                include_self = true;
                obj.insert("ref".to_string(), json!(ref_id));
            }
        }
        if skip_role {
            if children.is_empty() {
                None
            } else if children.len() == 1 {
                children.into_iter().next()
            } else {
                Some(Value::Array(children))
            }
        } else {
            if !children.is_empty() {
                obj.insert("children".to_string(), Value::Array(children));
            }
            if include_self || obj.contains_key("children") || role_is_context(&role_str) {
                Some(Value::Object(obj))
            } else {
                None
            }
        }
    }
}

pub(crate) fn attach_details_to_tree(
    mut tree: Vec<Value>,
    refs: &HashMap<String, ElementRef>,
) -> Vec<Value> {
    for item in &mut tree {
        attach_details_to_node(item, refs, 0);
    }
    tree
}

pub(crate) fn attach_details_to_node(
    node: &mut Value,
    refs: &HashMap<String, ElementRef>,
    depth: usize,
) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    if let Some(ref_id) = obj.get("ref").and_then(Value::as_str) {
        if let Some(entry) = refs.get(ref_id).and_then(|entry| entry.dom.as_ref()) {
            if let Ok(Value::Object(map)) = serde_json::to_value(entry) {
                for (key, value) in map {
                    if !value.is_null() {
                        obj.insert(key, value);
                    }
                }
            }
        }
    }
    // 深度上限：超过阈值不再下钻（详情挂在更深的节点上就放弃，不影响树本身）。
    if depth >= MAX_TREE_DEPTH {
        return;
    }
    if let Some(children) = obj.get_mut("children").and_then(Value::as_array_mut) {
        for child in children {
            attach_details_to_node(child, refs, depth + 1);
        }
    }
}

/// 一次 batch 里最多合并多少个元素详情请求（每个元素 2 条 CDP 命令）。
pub(crate) const DETAIL_BATCH_SIZE: usize = 40;

/// 单次 batch 的耗时上限；整体预算由 `collect_dom_details` 的 deadline 控制。
pub(crate) const DETAIL_BATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// 收集元素详情。返回 `(详情表, details_truncated)`。
///
/// 以前把调用方的 `timeout` 原样传给每个 batch：`--details --limit 1000` 要串行往返
/// 25 次，每次单独算超时，整体耗时可以是 `timeout` 的好几倍。
/// 现在按整体 deadline 分配：每次 batch 只拿 `min(单次上限, 剩余预算)`，预算耗尽就停下，
/// 并用 `details_truncated` 明确告诉调用方「详情不全」。
pub(crate) async fn collect_dom_details(
    state: &AppState,
    tab_id: &str,
    refs: &[ElementRef],
    include_errors: bool,
    timeout: Duration,
) -> (HashMap<String, ElementDomInfo>, bool) {
    let mut map = HashMap::with_capacity(refs.len());
    let deadline = Instant::now() + timeout;
    let mut details_truncated = false;
    for chunk in refs.chunks(DETAIL_BATCH_SIZE) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            details_truncated = true;
            break;
        }
        let batch_timeout = remaining.min(DETAIL_BATCH_TIMEOUT);
        match batch_dom_details(state, tab_id, chunk, batch_timeout).await {
            Ok(details) => {
                for (entry, detail) in chunk.iter().zip(details) {
                    map.insert(entry.ref_id.clone(), detail);
                }
            }
            Err(err) => {
                let message = include_errors.then(|| err.to_string());
                for entry in chunk {
                    map.insert(entry.ref_id.clone(), empty_dom_info(message.clone()));
                }
                // 本批失败且整体预算已经用完：后面的批只会继续超时，直接停下并标记。
                if Instant::now() >= deadline {
                    details_truncated = true;
                    break;
                }
            }
        }
    }
    (map, details_truncated)
}

/// 把 N 个元素的 `resolveNode + callFunctionOn` 合成一条 batch：
/// 原来每个元素一次 WS 往返，`--details --limit 200` 就是 200 次串行往返。
pub(crate) async fn batch_dom_details(
    state: &AppState,
    tab_id: &str,
    refs: &[ElementRef],
    timeout: Duration,
) -> Result<Vec<ElementDomInfo>> {
    let mut commands: Vec<Value> = Vec::with_capacity(refs.len() * 2);
    for entry in refs {
        let resolve_index = commands.len();
        commands.push(json!({
            "cmd": "cdp",
            "method": "DOM.resolveNode",
            "params": { "backendNodeId": entry.backend_dom_node_id }
        }));
        commands.push(json!({
            "cmd": "cdp",
            "method": "Runtime.callFunctionOn",
            "params": {
                "objectId": format!("${resolve_index}.object.objectId"),
                "functionDeclaration": ELEMENT_DOM_SCRIPT,
                "returnByValue": true
            }
        }));
    }
    let value = batch_cdp_call(state, tab_id, commands, timeout).await?;
    // 本函数按 `results[index * 2 + 1]` 取结果，条数/顺序一错就会把
    // resolveNode 的回包当成 callFunctionOn 的回包（详情字段全空，却报成功）。
    // 与 batch_cdp_call 共用同一份校验，不各写一套判据。
    validate_batch_response(&value, refs.len() * 2)?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("snapshot: batch detail response missing results"))?;
    let mut details = Vec::with_capacity(refs.len());
    for index in 0..refs.len() {
        let step = results.get(index * 2 + 1);
        // 与 call_function_resolved 同判据：页面脚本抛异常时没有 result，
        // 真异常在 exceptionDetails 里。不读它就只能报 "missing value"（同族）。
        if let Some(message) = step.and_then(cdp_exception_text) {
            details.push(empty_dom_info(Some(message)));
            continue;
        }
        let payload = step
            .and_then(|item| item.get("result"))
            .and_then(|result| result.get("value"));
        match payload {
            Some(value) => details.push(parse_dom_info(value)),
            None => details.push(empty_dom_info(Some(
                "snapshot: DOM detail missing value".to_string(),
            ))),
        }
    }
    Ok(details)
}

/// `snapshot --js --details` 的详情来源。
///
/// JS 快照的 ref 只有 CSS 选择器、没有 `backendNodeId`（`DOM.resolveNode` 用不了），
/// 所以在页面内按选择器一次注入描述全部元素：一次往返，不依赖 CDP。
pub(crate) async fn collect_dom_details_via_js(
    state: &AppState,
    session_key: &str,
    refs: &[ElementRef],
    timeout: Duration,
) -> Result<HashMap<String, ElementDomInfo>> {
    let targets: Vec<Value> = refs
        .iter()
        .filter_map(|entry| {
            entry
                .selector
                .clone()
                .map(|selector| json!([entry.ref_id, selector]))
        })
        .collect();
    if targets.is_empty() {
        return Ok(HashMap::new());
    }
    let script = format!(
        r#"const __abcDescribe = {describe};
const __abcTargets = {targets};
const __abcOut = {{}};
for (const __abcPair of __abcTargets) {{
  let __abcEl = null;
  try {{ __abcEl = document.querySelector(__abcPair[1]); }} catch (__abcErr) {{ __abcEl = null; }}
  if (!__abcEl) {{ __abcOut[__abcPair[0]] = {{ dom_error: 'element not found' }}; continue; }}
  try {{ __abcOut[__abcPair[0]] = __abcDescribe.call(__abcEl); }}
  catch (__abcErr) {{ __abcOut[__abcPair[0]] = {{ dom_error: String((__abcErr && __abcErr.message) || __abcErr) }}; }}
}}
return {{ ok: true, details: __abcOut }};"#,
        describe = ELEMENT_DOM_SCRIPT,
        targets = serde_json::to_string(&targets)?,
    );
    let executed = execute_page_js(
        state,
        &script,
        SessionSelector::new(Some(session_key.to_string()), None, None),
        true,
        timeout,
    )
    .await?;
    let payload = executed.get("js_return").cloned().unwrap_or(Value::Null);
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(anyhow!(
            "{}",
            humanize_error(
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("page script failed")
            )
        ));
    }
    let mut map = HashMap::new();
    if let Some(details) = payload.get("details").and_then(Value::as_object) {
        for (ref_id, value) in details {
            let info = match value.get("dom_error").and_then(Value::as_str) {
                Some(err) => empty_dom_info(Some(err.to_string())),
                None => parse_dom_info(value),
            };
            map.insert(ref_id.clone(), info);
        }
    }
    Ok(map)
}

pub(crate) fn empty_dom_info(dom_error: Option<String>) -> ElementDomInfo {
    ElementDomInfo {
        tag: None,
        text: None,
        placeholder: None,
        input_type: None,
        href: None,
        disabled: None,
        readonly: None,
        checked: None,
        selected: None,
        rect: None,
        selector: None,
        visible: None,
        dom_error,
    }
}

pub(crate) fn parse_dom_info(value: &Value) -> ElementDomInfo {
    let rect = value
        .get("rect")
        .and_then(|rect| serde_json::from_value::<RectInfo>(rect.clone()).ok());
    ElementDomInfo {
        tag: value.get("tag").and_then(Value::as_str).map(str::to_string),
        text: value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        placeholder: value
            .get("placeholder")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_type: value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        href: value
            .get("href")
            .and_then(Value::as_str)
            .map(str::to_string),
        disabled: value.get("disabled").and_then(Value::as_bool),
        readonly: value.get("readonly").and_then(Value::as_bool),
        checked: value.get("checked").and_then(Value::as_bool),
        selected: value.get("selected").and_then(Value::as_bool),
        rect,
        selector: value
            .get("selector")
            .and_then(Value::as_str)
            .map(str::to_string),
        visible: value.get("visible").and_then(Value::as_bool),
        dom_error: None,
    }
}

pub(crate) const ELEMENT_DOM_SCRIPT: &str = r#"function() {
      const el = this;
      const rect = el.getBoundingClientRect ? el.getBoundingClientRect() : null;
      const style = el.ownerDocument.defaultView.getComputedStyle(el);
      const visible = !!rect && rect.width > 0 && rect.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
      function cssPath(node) {
        if (!node || node.nodeType !== 1) return '';
        if (node.id) return '#' + CSS.escape(node.id);
        const parts = [];
        let cur = node;
        while (cur && cur.nodeType === 1 && parts.length < 5) {
          let part = cur.localName;
          if (cur.classList && cur.classList.length) part += '.' + [...cur.classList].slice(0, 2).map(c => CSS.escape(c)).join('.');
          const parent = cur.parentElement;
          if (parent) {
            const same = [...parent.children].filter(c => c.localName === cur.localName);
            if (same.length > 1) part += `:nth-of-type(${same.indexOf(cur) + 1})`;
          }
          parts.unshift(part);
          cur = parent;
        }
        return parts.join(' > ');
      }
      const text = (el.innerText || el.textContent || '').trim().slice(0, 160);
      return {
        tag: el.tagName || '', text,
        placeholder: el.getAttribute && el.getAttribute('placeholder') || undefined,
        type: el.getAttribute && (el.getAttribute('type') || undefined),
        href: el.href ? String(el.href).slice(0, 300) : undefined,
        disabled: !!el.disabled,
        readonly: !!el.readOnly,
        checked: typeof el.checked === 'boolean' ? el.checked : undefined,
        selected: typeof el.selected === 'boolean' ? el.selected : undefined,
        rect: rect ? { x: rect.x, y: rect.y, width: rect.width, height: rect.height } : undefined,
        selector: cssPath(el), visible
      };
    }"#;

#[cfg(test)]
mod regression_tests {
    use super::*;

    /// 裸 `EvalError` 不能当成 CSP，否则点击类动作会被「换通道」再执行一次。
    #[test]
    fn csp_detection_requires_a_csp_specific_phrase() {
        assert!(is_csp_error(
            "EvalError: Refused to evaluate a string as JavaScript because 'unsafe-eval' is not an allowed source"
        ));
        assert!(is_csp_error("Refused to compile WebAssembly module"));
        assert!(is_csp_error("call to Function() blocked by CSP"));
        assert!(!is_csp_error("EvalError: 余额不足"));
        assert!(!is_csp_error("evalerror"));
        assert!(!is_csp_error("#csp-banner not found"));
        assert!(!is_csp_error("/csp-report 404"));
    }

    /// 旧版页面脚本从文档开头取，起点是 0；新版认 offset 时起点必须跟着走。
    #[test]
    fn snapshot_batch_base_offset_follows_page_support() {
        let legacy = vec![json!({ "ref": "@e1" }), json!({ "ref": "@e2" })];
        let payload = json!({ "ok": true });
        assert_eq!(snapshot_items_base_offset(&payload, &legacy, 0), 0);
        assert_eq!(snapshot_items_base_offset(&payload, &legacy, 10), 0);

        let windowed = vec![json!({ "ref": "@e11" }), json!({ "ref": "@e12" })];
        assert_eq!(snapshot_items_base_offset(&payload, &windowed, 10), 10);
        let echoed = json!({ "offset": 10 });
        assert_eq!(snapshot_items_base_offset(&echoed, &[], 10), 10);
    }
}
