use super::*;

/// 找出页面上所有验证码 iframe 及其屏幕几何。
///
/// 走 DOM 查而不是 `Page.getFrameTree`：跨源 iframe 是独立进程（OOPIF），
/// 主目标的 frame tree 里看不到它们（实测只返回主框架 + about:blank 两个），
/// 而验证码组件恰恰全是跨源的。`DOM.querySelectorAll('iframe')` 拿的是元素本身，
/// 跨不跨源都一样；盒模型还自带 CSS transform 后的四角坐标。
///
// `deadline` 是整条探测链的硬截止。每次 CDP 调用前都按剩余量重算超时，
/// 归零立刻收手 —— 旧实现把同一个 `timeout` 复用到每个 iframe 的所有调用上，
/// 30 个 iframe 就能把 `--timeout 30` 顶成好几分钟。
///
/// 第二个返回值是排障诊断：定位要过 文档 → iframe 元素 → 盒模型 三关，
/// 任何一关失败都表现为「没找到组件」，没有诊断就只能瞎猜。
///
// 第三个返回值 `query_failed` 是 的判据：**查询本身失败**（CDP 附不上 /
/// DOM 域不可用 / 回包缺失）与「页面上真的没有 iframe」必须分开。旧实现把两种
/// 情况都压成空集合，求解决策里「组件消失」被当成「点击被接收」（widget-removed），
/// 于是一次 attach 失败就让整轮求解提前停手等结论。
pub(crate) async fn challenge_frames(
    state: &AppState,
    session_key: &str,
    deadline: Instant,
) -> (Vec<ChallengeFrame>, Vec<Value>, bool) {
    let mut diag: Vec<Value> = Vec::new();
    let Some(left) = deadline_left(deadline) else {
        push_diag_error(
            &mut diag,
            "dom-enable",
            "deadline",
            "挑战几何探测预算耗尽",
            None,
        );
        return (Vec::new(), diag, true);
    };
    // DOM 域没启用时 getAttributes / getBoxModel 会报错；启用本身对页面不可见。
    let enable = cdp_call(state, session_key, "DOM.enable", json!({}), left).await;
    // 页面里挂着**别的扩展**注入的 iframe 时，Chrome 会直接拒绝 chrome.debugger.attach
    // （"Cannot access a chrome-extension:// URL of different extension"）。
    // 这种 iframe 对页面内容没有贡献，却能让整条验证码链路失效（定位、点击、截图全废）。
    // 先摘掉再 attach 一次；扩展会在下次导航时自己重新注入，不做持久化。
    let attach_error = match &enable {
        Err(err) => Some(err.to_string()),
        Ok(result) if !result.ok => Some(result.error.clone().unwrap_or_default()),
        Ok(_) => None,
    };
    if let Some(message) = attach_error {
        // attach 失败本身就是根因，先原样收进 diag，再判断是不是跨扩展 iframe。
        push_diag_error(&mut diag, "dom-enable", "attach_failed", &message, None);
        if is_foreign_extension_frame_error(&message) {
            // ：摘除失败、以及「拿不到自身扩展 id 所以没摘」都要单独记录。
            match strip_foreign_extension_frames(state, session_key).await {
                Ok(StripFramesOutcome::Stripped(stripped)) => {
                    diag.push(json!({
                        "step": "attach",
                        "reason": "foreign_extension_iframe",
                        "stripped": stripped,
                    }));
                }
                Ok(StripFramesOutcome::SkippedUnknownOwnId) => {
                    diag.push(json!({
                        "step": "attach",
                        "reason": "foreign_extension_iframe",
                        "skipped": "unknown_own_extension_id",
                    }));
                }
                Err(err) => push_diag_error(
                    &mut diag,
                    "strip-foreign-frames",
                    "script_failed",
                    &err.to_string(),
                    None,
                ),
            }
            if let Some(left) = deadline_left(deadline) {
                let _ = cdp_call(state, session_key, "DOM.enable", json!({}), left).await;
            }
        }
    }
    let (iframe_ids, query_failed) =
        collect_iframe_ids(state, session_key, deadline, &mut diag).await;
    if iframe_ids.is_empty() {
        if query_failed {
            diag.push(json!({ "step": "query", "reason": "frames_query_failed" }));
            return (Vec::new(), diag, true);
        }
        diag.push(json!({ "step": "query", "reason": "no_iframe_element" }));
        return (Vec::new(), diag, false);
    }

    let mut out = Vec::new();
    let mut seen: Vec<Value> = Vec::new();
    // 探测中途把 deadline 耗光时，空集合不能被读成「组件已消失」：
    // 用这个标记把「没探完」与「真的没有」区分开。
    let mut deadline_exhausted = false;
    // srcdoc / about:blank 的包裹 iframe 的 URL 里没有挑战特征，光靠
    // URL 子串匹配会漏掉。对这类 iframe 追加「面积 > 阈值 + hit-test 命中自身」
    // 两个判据，阈值可配（默认 2000 px²）。
    let srcdoc_min_area = crate::settings::challenge_srcdoc_min_area();
    for node_id in iframe_ids {
        if deadline_left(deadline).is_none() {
            deadline_exhausted = true;
            push_diag_error(
                &mut diag,
                "iframe",
                "deadline",
                "挑战几何探测预算耗尽，剩余 iframe 未探测",
                Some(node_id),
            );
            break;
        }
        let url = iframe_doc_url(state, session_key, node_id, deadline, &mut diag).await;
        if seen.len() < 8 {
            seen.push(json!({ "node": node_id, "url": url.chars().take(120).collect::<String>() }));
        }
        let hint_match = CHALLENGE_FRAME_HINTS.iter().any(|hint| url.contains(hint));
        let opaque = url.trim().is_empty() || url.trim_start().starts_with("about:");
        if !hint_match && !opaque {
            continue;
        }
        let Some(left) = deadline_left(deadline) else {
            deadline_exhausted = true;
            push_diag_error(
                &mut diag,
                "getBoxModel",
                "deadline",
                "挑战几何探测预算耗尽",
                Some(node_id),
            );
            break;
        };
        let model = match cdp_call(
            state,
            session_key,
            "DOM.getBoxModel",
            json!({ "nodeId": node_id }),
            left,
        )
        .await
        {
            Ok(result) if result.ok => result.data.unwrap_or(Value::Null),
            Ok(result) => {
                push_diag_error(
                    &mut diag,
                    "getBoxModel",
                    "cdp_rejected",
                    result.error.as_deref().unwrap_or("扩展未给出原因"),
                    Some(node_id),
                );
                continue;
            }
            Err(err) => {
                push_diag_error(
                    &mut diag,
                    "getBoxModel",
                    "cdp_error",
                    &err.to_string(),
                    Some(node_id),
                );
                continue;
            }
        };
        let content = model
            .pointer("/model/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if content.len() < 8 {
            push_diag_error(
                &mut diag,
                "geometry",
                "no_content_quad",
                "盒模型缺少 content quad",
                Some(node_id),
            );
            continue;
        }
        let n = |idx: usize| content.get(idx).and_then(Value::as_f64).unwrap_or_default();
        let quad = [n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7)];
        let width = ((quad[2] - quad[0]).powi(2) + (quad[3] - quad[1]).powi(2)).sqrt();
        let height = ((quad[6] - quad[0]).powi(2) + (quad[7] - quad[1]).powi(2)).sqrt();
        if width < 4.0 || height < 4.0 {
            diag.push(json!({ "step": "geometry", "node": node_id, "reason": "zero_size", "w": width, "h": height }));
            continue;
        }
        if !hint_match {
            // URL 无特征、只能靠几何判断：面积够大且中心点 hit-test 到自身，
            // 才可能是挑战组件（广告 / 隐藏探针通常过不了这两关）。
            let area = width * height;
            if area < srcdoc_min_area {
                diag.push(json!({
                    "step": "srcdoc-frame",
                    "node": node_id,
                    "reason": "below_area_threshold",
                    "area": area.round(),
                    "threshold": srcdoc_min_area,
                }));
                continue;
            }
            let cx = (quad[0] + quad[4]) / 2.0;
            let cy = (quad[1] + quad[5]) / 2.0;
            if !iframe_hit_test_visible(state, session_key, node_id, cx, cy, deadline, &mut diag)
                .await
            {
                diag.push(json!({
                    "step": "srcdoc-frame",
                    "node": node_id,
                    "reason": "hit_test_missed",
                    "x": cx.round(),
                    "y": cy.round(),
                }));
                continue;
            }
            diag.push(json!({
                "step": "srcdoc-frame",
                "node": node_id,
                "reason": "accepted_opaque_frame",
                "area": area.round(),
            }));
        }
        out.push(ChallengeFrame {
            url,
            node_id,
            quad,
            width,
            height,
        });
    }
    if out.is_empty() {
        diag.push(json!({ "step": "match", "reason": "no_challenge_iframe", "iframes": seen }));
    }
    // 面积大的排前面：页面里常有一个「小尺寸 / 隐形」的挑战 iframe（站点自己的检测框、
    // CF 的 hidden 探针），DOM 顺序上它可能排第一。调用方取 `frames.first()` 时就会
    // 永远点到那个点不动的组件。`width/height` 已经在上面按 >=4.0 校验过。
    out.sort_by(|left, right| {
        (right.width * right.height)
            .partial_cmp(&(left.width * left.height))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // 多 iframe 页最终只返回面积最大的前 K 个
    // （ATB_CHALLENGE_MAX_IFRAMES，默认 8），并把这件「做过截断」写进
    // `diag.iframe_limit`，避免调用方把「没探第 K+1 个」当成「页面上没有」。
    let max_iframes = crate::settings::challenge_max_iframes();
    if out.len() > max_iframes {
        let total = out.len();
        out.truncate(max_iframes);
        diag.push(json!({
            "step": "iframe_limit",
            "total": total,
            "kept": max_iframes,
            "reason": "challenge_max_iframes",
        }));
    }
    let query_failed = deadline_exhausted && out.is_empty();
    (out, diag, query_failed)
}

/// 中心点是否真的命中了这个 iframe（而不是被别的元素盖住）。
///
/// `about:blank` / `srcdoc` 的包裹 iframe 没有 URL 特征，只能靠几何 + hit-test
// 判断它是不是「用户能看见、能点到」的组件。CDP 的
/// `DOM.getNodeForLocation` 返回最顶层节点；命中点属于别的元素就说明它被遮挡。
async fn iframe_hit_test_visible(
    state: &AppState,
    session_key: &str,
    node_id: i64,
    x: f64,
    y: f64,
    deadline: Instant,
    diag: &mut Vec<Value>,
) -> bool {
    let Some(left) = deadline_left(deadline) else {
        push_diag_error(
            diag,
            "hit-test",
            "deadline",
            "挑战几何探测预算耗尽",
            Some(node_id),
        );
        return false;
    };
    let result = match cdp_call(
        state,
        session_key,
        "DOM.getNodeForLocation",
        json!({ "x": x, "y": y, "includeUserAgentShadowDOM": true }),
        left,
    )
    .await
    {
        Ok(result) if result.ok => result.data.unwrap_or(Value::Null),
        Ok(result) => {
            push_diag_error(
                diag,
                "hit-test",
                "cdp_rejected",
                result.error.as_deref().unwrap_or("扩展未给出原因"),
                Some(node_id),
            );
            return false;
        }
        Err(err) => {
            push_diag_error(
                diag,
                "hit-test",
                "cdp_error",
                &err.to_string(),
                Some(node_id),
            );
            return false;
        }
    };
    if let Some(hit) = result.get("nodeId").and_then(Value::as_i64) {
        return hit == node_id;
    }
    let Some(backend) = result.get("backendNodeId").and_then(Value::as_i64) else {
        push_diag_error(
            diag,
            "hit-test",
            "invalid_response",
            "DOM.getNodeForLocation 未返回 nodeId / backendNodeId",
            Some(node_id),
        );
        return false;
    };
    let Some(left) = deadline_left(deadline) else {
        return false;
    };
    match cdp_call(
        state,
        session_key,
        "DOM.describeNode",
        json!({ "backendNodeId": backend, "depth": 0 }),
        left,
    )
    .await
    {
        Ok(result) if result.ok => {
            let data = result.data.unwrap_or(Value::Null);
            if let Some(id) = data.pointer("/node/nodeId").and_then(Value::as_i64) {
                return id == node_id;
            }
            // 旧扩展不回 nodeId 时退一步：命中点就是 iframe 元素本身也算可见。
            data.pointer("/node/nodeName").and_then(Value::as_str) == Some("IFRAME")
        }
        Ok(result) => {
            push_diag_error(
                diag,
                "hit-test",
                "cdp_rejected",
                result.error.as_deref().unwrap_or("扩展未给出原因"),
                Some(node_id),
            );
            false
        }
        Err(err) => {
            push_diag_error(
                diag,
                "hit-test",
                "cdp_error",
                &err.to_string(),
                Some(node_id),
            );
            false
        }
    }
}

/// 坐标点击的超时错误码：整条链的预算不足时返回，CLI 可据此分支。
pub(crate) const CLICK_TIMEOUT_CODE: &str = "challenge_click_timeout";
/// 一次鼠标事件至少需要这么多预算；低于它就没必要再发（必然超时）。
const MIN_EVENT_BUDGET: Duration = Duration::from_millis(25);

/// 用可信鼠标事件点一个页面视口坐标。
///
/// 这是给「智能体自己点」的兜底通道（`challenge click --at x,y`）：组件已经在
/// 页面上、坐标也拿到了，但它需要一次真实鼠标事件。事件之间**不插入 sleep**——
/// 间距由 CDP 往返自然产生；从左侧 24px 滑入只是为了让 Turnstile 看到
/// mouseenter → mousemove → mousedown 的完整序列。
///
// `deadline` 是**整条链**的硬截止。6 次 `Input.dispatchMouseEvent`
/// 每次都在发送前按剩余量截断；不足一次事件就提前返回
/// `challenge_click_timeout`。旧实现让每次调用各用 `per_call`，扩展挂死时最坏
/// 能到 6×15s，`--timeout` 完全失效。
///
/// 每一次 `Input.*` 都走 `into_ok` 检查扩展的 `ok` 字段。旧实现只看
/// 传输层 `?`，`{ok:false}`（未 attach / 参数非法 / 目标已销毁）被当成「已发出」。
pub(crate) async fn challenge_click_at(
    state: &AppState,
    session_key: &str,
    x: f64,
    y: f64,
    deadline: Instant,
) -> Result<()> {
    const MOUSE: &str = "Input.dispatchMouseEvent";
    for step in 1..=4 {
        let t = f64::from(step) / 4.0;
        let px = x - 24.0 * (1.0 - t);
        let py = y + if step % 2 == 0 { 1.0 } else { -1.0 };
        click_event(
            state,
            session_key,
            json!({ "type": "mouseMoved", "x": px, "y": py, "button": "none", "buttons": 0, "pointerType": "mouse" }),
            deadline,
            "mouseMoved",
        )
        .await?;
    }
    click_event(
        state,
        session_key,
        json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0, "pointerType": "mouse" }),
        deadline,
        "mouseMoved",
    )
    .await?;
    // 按下之前先确认还有「按下 + 抬起」两份预算：预算只够按下时宁可整条链
    // 报超时，也不能把页面留在 mousedown 态（后续 click 事件永远不会触发）。
    match deadline_left(deadline) {
        Some(left) if left >= MIN_EVENT_BUDGET * 2 => {}
        _ => {
            return Err(anyhow!(
                "{CLICK_TIMEOUT_CODE}: 剩余预算不足以完成一次按下/抬起，已取消点击"
            ))
        }
    }
    // press 失败也 best-effort 抬一次（状态未知），与
    // `challenge_click_widget` 的 finally 口径一致；原始错误优先，抬起失败不覆盖它。
    let pressed = click_event(
        state,
        session_key,
        json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1, "pointerType": "mouse" }),
        deadline,
        "mousePressed",
    )
    .await;
    match pressed {
        Ok(()) => {
            let release = json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1, "pointerType": "mouse" });
            // 都已经按下去了，抬起必须尽力：哪怕预算只剩 1ms 也要发一次，
            // 否则组件会永远停在「按下」态。
            let release_timeout = deadline_left(deadline).unwrap_or(Duration::from_millis(1));
            cdp_call(state, session_key, MOUSE, release, release_timeout)
                .await?
                .into_ok(MOUSE)?;
            Ok(())
        }
        Err(err) => {
            let release = json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1, "pointerType": "mouse" });
            if let Some(left) = deadline_left(deadline) {
                let _ = cdp_call(state, session_key, MOUSE, release, left)
                    .await
                    .and_then(|result| result.into_ok(MOUSE));
            }
            Err(err)
        }
    }
}

/// 发一次鼠标事件，超时按 `deadline` 截断。
async fn click_event(
    state: &AppState,
    session_key: &str,
    params: Value,
    deadline: Instant,
    step: &str,
) -> Result<()> {
    const MOUSE: &str = "Input.dispatchMouseEvent";
    let left = deadline_left(deadline).ok_or_else(|| {
        anyhow!("{CLICK_TIMEOUT_CODE}: 剩余预算耗尽，未能在超时内完成坐标点击（step={step}）")
    })?;
    if left < MIN_EVENT_BUDGET {
        return Err(anyhow!(
            "{CLICK_TIMEOUT_CODE}: 剩余预算不足一次鼠标事件（step={step}）"
        ));
    }
    match cdp_call(state, session_key, MOUSE, params, left).await {
        Ok(result) => {
            result.into_ok(MOUSE)?;
            Ok(())
        }
        Err(err) => {
            // 调用本身超时/挂死且 deadline 已过：统一归到 click 超时，
            // 调用方才能给出稳定的 `challenge_click_timeout`。
            if deadline_left(deadline).is_none() {
                Err(anyhow!("{CLICK_TIMEOUT_CODE}: {err}"))
            } else {
                Err(err)
            }
        }
    }
}

/// 点击后等结论。
///
/// CF 点完勾选框不会立刻放行，而是切到「Verifying you are human. This may take
/// a few seconds.」再要 3-10 秒。这段窗口里重复点击会把流程打回原点，
/// 所以这里持续轮询到出结论或窗口用尽，并把 verifying 状态回传给调用方。
pub(crate) async fn challenge_wait_result(
    state: &AppState,
    session_key: &str,
    max_wait: Duration,
    budget_end: Instant,
) -> (bool, Value, bool) {
    let deadline = (Instant::now() + max_wait).min(budget_end);
    let mut last = Value::Null;
    let mut verifying = false;
    // 失败诊断：只有发生失败时才会往里放，返回结构在成功路径上不变。
    let mut diag: Vec<Value> = Vec::new();
    // 只在第一次发现「窗口不可见」时恢复一次：反复切标签会把正在跑的验证打断，
    // 而已经跑废的挑战也不会因为窗口恢复就复活（那需要调用方 reload）。
    let mut env_restored = false;
    // 「已经拦了多久」由 daemon 记并下发（页面脚本刻意不写 window.*，见）。
    let mut first_blocking_at: Option<Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(350)).await;
        // （同一预算约束链）：单次 status 查询只能用**本函数剩余**里的量。
        // 固定 8s 时，扩展挂死会让「被动等待」在 deadline 之后再多耗 8s，
        // `--challenge-timeout 15` 就会被顶成 23s。
        let left = deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(50) {
            return (false, attach_wait_diag(last, &diag), verifying);
        }
        // 只有「真的探测过」的 status 才能参与判定：空载荷（Value::Null）上
        // detected / blocking 全是 false，下面的 `!detected` 会立刻判 cleared:true，
        // 这正是 的假通过路径。拿不到结论就按「还没结论」继续等/超时。
        let Some(status) = challenge_page_call(
            state,
            session_key,
            json!({
                "action": "status",
                "stalledMs": first_blocking_at
                    .map(|at| at.elapsed().as_millis() as u64)
                    .unwrap_or(0),
            }),
            None,
            left.min(Duration::from_secs(8)),
        )
        .await
        .ok()
        .filter(challenge_status_usable) else {
            if Instant::now() >= deadline {
                return (false, attach_wait_diag(last, &diag), verifying);
            }
            continue;
        };
        let flag = |key: &str| status.get(key).and_then(Value::as_bool).unwrap_or(false);
        let token_ready = flag("token_ready");
        let detected = flag("detected");
        let blocking = flag("blocking");
        let interactive = flag("interactive");
        verifying = flag("verifying") || verifying;
        if blocking || interactive {
            first_blocking_at.get_or_insert_with(Instant::now);
        } else if !detected {
            first_blocking_at = None;
        }

        // 窗口最小化时 `outerWidth=0`、`visibilityState='hidden'`，CF 的 Turnstile
        // 在这种页面里**不会执行**（实测连 iframe 都不创建）。`--background` 打开时
        // 窗口往往就是最小化的，所以先恢复一次窗口，再继续等。
        if !env_restored {
            let env_bad = status
                .get("env")
                .map(|env| {
                    let hidden = env.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    let outer_w = env
                        .get("outer")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0);
                    hidden || outer_w <= 0.0
                })
                .unwrap_or(false);
            if env_bad {
                env_restored = true;
                // ：切不过去就别静默失败；激活统一走
                // `challenge_activate_tab`（仅在非 active 时切换、同窗口带冷却）。
                challenge_activate_tab(state, session_key, false, &mut diag).await;
            }
        }

        last = status.clone();
        if token_ready || !detected || (!blocking && !interactive) {
            return (true, attach_wait_diag(status, &diag), verifying);
        }
        // 静默挂死：页面侧已确认「Turnstile 没渲染」并超过阈值 —— 继续等只会把
        // `--challenge-timeout` 耗完，早点把结论和原因交出去，比一个 need_human 有用。
        if flag("stalled") {
            return (false, attach_wait_diag(status, &diag), verifying);
        }
        if Instant::now() >= deadline {
            return (false, attach_wait_diag(last, &diag), verifying);
        }
    }
}

/// 把失败诊断并入返回给调用方的 status 载荷。
///
/// 没有诊断时不加任何字段：成功路径的输出结构与旧版完全一致，只在真的
/// 发生过「切标签失败」时才多一个 `diag` 数组。
fn attach_wait_diag(status: Value, diag: &[Value]) -> Value {
    if diag.is_empty() {
        return status;
    }
    match status {
        Value::Object(mut map) => {
            map.insert("diag".to_string(), json!(diag));
            Value::Object(map)
        }
        // 非对象（通常只有 `Value::Null`）时整体换成一个只带诊断的对象。
        _ => json!({ "diag": diag }),
    }
}
