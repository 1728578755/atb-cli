use super::*;

/// 挑战页标题特征（`open` 的「值不值得探一次」与 read 的
/// `attach_challenge_if_blocked` 共用同一份列表）。
///
/// 两处曾经各写一份，`should_probe_challenge` 里还留着已声明移除的 "access denied"，
// 于是同一个 403 页在 open 与 read 两条路径上得到相反结论。
pub(crate) const CHALLENGE_TITLE_HINTS: [&str; 12] = [
    "just a moment",
    "attention required",
    "checking your browser",
    "verifying you are human",
    "one more step",
    "请稍候",
    "正在检查",
    "正在验证",
    "安全验证",
    "人机验证",
    "确认您是真人",
    "请完成安全验证",
];

/// 诊断里最多保留多少条底层错误。
///
/// `ATB_DIAG_DEPTH` 的 `0` 表示「只保留一句归因」，不是完全静音：排障时
/// 「没有任何错误」和「有错但被截断」必须可区分。
fn diag_error_limit() -> usize {
    crate::settings::diag_depth().max(1)
}

// 把底层 `Err` 的原文限量收进 diag。
///
/// 旧实现把 `Err` 层层吞成 `false` / 空集合，上层只能看到
/// "no challenge iframe" 这类结论，根因（attach 失败 / 回包缺失 / 超时）完全丢失。
pub(crate) fn push_diag_error(
    diag: &mut Vec<Value>,
    step: &str,
    reason: &str,
    detail: &str,
    node: Option<i64>,
) {
    if diag.len() >= diag_error_limit() {
        return;
    }
    let mut entry = json!({
        "step": step,
        "reason": reason,
        "error": truncate_text(detail, 240),
    });
    if let Some(node) = node {
        entry["node"] = json!(node);
    }
    diag.push(entry);
}

/// 距截止时间还剩多少；归零返回 `None`（调用方负责把「预算耗尽」写进 diag）。
///
// 单次 CDP 调用的超时必须在**每次调用前**按 deadline 重算，
/// 不能在循环里复用同一个 `timeout`，否则 N 个 iframe 的最坏耗时是 N×timeout。
pub(crate) fn deadline_left(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some(left)
    }
}

/// 只在「看起来像挑战页」时才多注入一次做精确识别。
///
/// 正常页面不该为验证码检测多付一次往返；而 CF 拦截页的标题/正文特征很稳定，
/// 用几十字节的文本判断就够，命中后再让页面脚本给结构化结果。
pub(crate) async fn attach_challenge_if_blocked(state: &AppState, payload: &mut Value) {
    let page = payload.get("page").cloned().unwrap_or(Value::Null);
    let title = page
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let text = page
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 这里的列表只决定「要不要多花一次往返去问页面脚本」，不用来下结论。
    // "access denied" 已移除：它是无数 403 页的通用标题，页面脚本侧也只当 suspected
    // （见 page_challenge.js 的 STRONG/WEAK 分档），留着只会让每个 403 页白付一次注入。
    let title_hit = CHALLENGE_TITLE_HINTS
        .iter()
        .any(|needle| title.contains(needle));
    let text_hit = text.chars().count() < 600 && {
        let lower = text.to_lowercase();
        [
            "verify you are human",
            "checking if the site connection",
            "enable javascript and cookies",
            "确认您是真人",
            "请完成安全验证",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    };
    if !title_hit && !text_hit {
        return;
    }
    let session_key = payload
        .get("session_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_key.is_empty() {
        return;
    }
    // 探测结果必须真的可判定才算「识别成功」。`challenge_page_call` 在
    // 扩展未注入 / 页面脚本没有返回值时给 `Value::Null`，把它塞进 `challenge` 会让
    // 调用方读成「没有挑战」——把根本没探到说成探到了。
    match challenge_page_call(
        state,
        &session_key,
        json!({ "action": "status" }),
        None,
        Duration::from_secs(8),
    )
    .await
    {
        Ok(challenge) if challenge_status_usable(&challenge) => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("challenge".to_string(), challenge);
            }
        }
        Ok(_) => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("challenge_probe_failed".to_string(), json!(true));
                object.insert(
                    "challenge_probe_error".to_string(),
                    json!("页面 status 返回空载荷（扩展未注入 / 页面脚本未执行）"),
                );
            }
        }
        Err(err) => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("challenge_probe_failed".to_string(), json!(true));
                object.insert(
                    "challenge_probe_error".to_string(),
                    json!(humanize_extension_error(&err.to_string())),
                );
            }
        }
    }
}

/// 值不值得为这个刚打开的标签页花一次往返去问「你是不是挑战页」。
///
/// 标题是最便宜的判据：CF 拦截页标题稳定为 Just a moment / Attention required 等。
/// 标题为空的也探一次——挑战页在拿到响应头之前标题就是空的，而正常页面几毫秒内
/// 就会有标题，这点开销（一次 page op）远小于「漏判导致后续命令全打在拦截页上」。
pub(crate) fn should_probe_challenge(url: &str, title: &str) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    let title = title.trim().to_lowercase();
    if title.is_empty() {
        return true;
    }
    CHALLENGE_TITLE_HINTS
        .iter()
        .any(|hint| title.contains(hint))
}

/// open 之后的自动等待：识别一次，确认被拦才进入等待。
///
/// 复用 `challenge_value` 的 wait 分支，保证 `open --challenge` 与 `challenge wait`
/// 的判定口径、超时语义、need_human 行为完全一致——两条独立实现迟早会分叉。
pub(crate) async fn auto_wait_challenge(
    state: &AppState,
    session_key: &str,
    timeout_secs: f64,
    activate: bool,
) -> Value {
    // 识别要重试：刚 open 完的标签页可能还在导航中，页面脚本注入会撞上
    // 「执行上下文被销毁」，表现为 exec_timeout（ACK 收到、结果丢失）。
    // 这种失败跟「不是挑战页」是两回事，直接报错会让 open 的输出里多一条假故障；
    // 等页面落定后再探一次基本都能拿到结果。
    //
    // 另外文档还在 loading 时必须重探：CF 拦截页刚提交时标题是空的、DOM 还没渲染，
    // 此刻「检测不到挑战」是假阴性，直接返回 cleared:true 会让调用方以为已经进去了。
    let mut last_err = String::new();
    let mut got: Option<Value> = None;
    // `--challenge-timeout` 是**总预算**，不是每个阶段各给一份。
    // 旧实现：solve_budget=0.8T、reload 再等最多 8s、被动等待又拿完整 T，
    // 最坏 ≈1.8T+8s —— `--challenge-timeout 15` 实测能等 35~40s。
    // 从现在起所有阶段都只从 `remaining()` 里取，绝不重新起算。
    let overall_deadline = Instant::now() + secs(timeout_secs.clamp(1.0, 600.0));
    let budget_left = || overall_deadline.saturating_duration_since(Instant::now());
    for attempt in 0..12 {
        // 重试次数不能压过总预算。旧实现只在循环外算一次 deadline，
        // 循环里每次固定 8s，`--challenge-timeout 15` + 扩展挂死时这一小段
        // 最坏能等 12×8s —— 「识别」就把整条链路的预算吃光。
        let left = budget_left();
        if left.is_zero() {
            break;
        }
        // 剩余不足一次往返（50ms）时同样收手：用 0 超时发请求只能得到一次
        // 必然失败的调用，把「预算用完」误报成「扩展错误」。
        if left < Duration::from_millis(50) {
            break;
        }
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250).min(left)).await;
        }
        match challenge_page_call(
            state,
            session_key,
            json!({ "action": "status" }),
            None,
            left.min(STATUS_CALL_TIMEOUT),
        )
        .await
        {
            // `Ok(Value::Null)`（页面脚本没返回值）不能当成「探测过了」：Null 上
            // blocking / interactive 全是 false，下面会直接判 cleared:true。
            Ok(status) if !challenge_status_usable(&status) => {
                last_err = "页面 status 返回空载荷（扩展未注入 / 页面脚本未执行）".to_string();
            }
            Ok(status) => {
                let pending = status
                    .get("pending")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                got = Some(status);
                // 最多为「还在加载」等 1.5s：再久就是慢站点的正常加载，
                // 不该让每次 open 都陪着等。
                if !pending || attempt >= 6 {
                    break;
                }
            }
            Err(err) => last_err = err.to_string(),
        }
    }
    let status = match got {
        Some(status) => status,
        // 识别失败不能算 open 失败：页面已经打开了，把原因带回去让调用方决定。
        None => {
            return json!({
                "detected": Value::Null,
                "waited": false,
                "error": humanize_extension_error(&last_err),
            })
        }
    };
    let blocking = status
        .get("blocking")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let interactive = status
        .get("interactive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let token_ready = status
        .get("token_ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 文档还在 loading 时**不能**下结论。上面的重试最多只等 6×250ms≈1.5s，慢站点
    // （CF 边缘拥塞、大页面）超过这个时间还在 pending 很正常；而 pending 期间的
    // 「检测不到挑战」是假阴性，此时返回 cleared:true 会让 `open --challenge` 的
    // 调用方以为已经进去了（与函数内注释直接矛盾）。
    // 如实回 `cleared:false + reason:"still_loading"`，让调用方自己决定要不要再等。
    if status
        .get("pending")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return json!({
            "detected": status.get("detected").cloned().unwrap_or(json!(false)),
            "waited": false,
            "cleared": false,
            "reason": "still_loading",
            "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
            "kind": status.get("kind").cloned().unwrap_or(Value::Null),
        });
    }
    // 「没被拦」不等于「已经放行」：页面上挂着一个没勾的 Turnstile 组件时，
    // blocking 是 false（能浏览），但提交前必须过它。旧实现只看 blocking，
    // 于是 open 在这种页面上返回 cleared:true —— 调用方以为进去了，
    // 后续命令全打在挑战页上。判定改成「有组件且没 token」也算未完成。
    if !blocking && (!interactive || token_ready) {
        return json!({
            "detected": status.get("detected").cloned().unwrap_or(json!(false)),
            "waited": false,
            "cleared": true,
            "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
            "kind": status.get("kind").cloned().unwrap_or(Value::Null),
        });
    }
    // 硬拦截（CF 的 "Sorry, you have been blocked"、error 1020/1015）：点也点不了、
    // 等也不会放行。直接把结论带回去，别让 open 白耗十几秒再报超时。
    if status.get("kind").and_then(Value::as_str) == Some("blocked") {
        return json!({
            "detected": true,
            "waited": false,
            "cleared": false,
            "blocked": true,
            "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
            "kind": "blocked",
            "hints": status.get("hints").cloned().unwrap_or_else(|| json!([])),
        });
    }
    // 后台标签的定时器会被 Chrome 节流，CF/DataDome 的挑战脚本跑不完 —— 实测同一个
    // 挑战页前台 1~3s 放行、后台 20~30s 仍在转圈。所以**只要判定为拦截，就必须把这个
    // 标签切成 active**，`--background` 也不例外：那个参数的本意是「开页面别打断我」，
    // 而不是「停在挑战页上永远过不去」。allowFocus:false 只切标签、不抢系统焦点。
    // 切不过去时挑战一定过不了（定时器被节流），必须把原因带回给调用方：
    // 否则 open 只会报一句「等了 N 秒没清」，看不出是标签根本没激活。
    let mut diag: Vec<Value> = Vec::new();
    // 只有「确实被拦 + 该 tab 非 active」才切，且同窗口冷却；
    // 不再每个求解器都无条件抢一次 active tab。
    challenge_activate_tab(state, session_key, false, &mut diag).await;
    // `activate` 只是 open 的「打开时是否前台」提示：挑战页无论是否 --background
    // 都必须在前台跑定时器，是否真的需要切换由 challenge_activate_tab 判断。
    let _ = activate;
    // 先主动点一次验证码组件（带勾选框的 Turnstile 等只会等待是过不去的）。
    // 预算取总超时的 80%（上限 25s）：点完还要等它自己验证，Turnstile 常常要 5-20s
    // 才写出 token，旧的 12s 上限会在「马上要过」的时候掐断，随后 open 报 need_human，
    // 用户看到的就是「工具有时候能过、有时候不能」。剩下的时间留给被动等待。
    //
    // 一律走一次主动求解，不再拿「页面脚本说有没有可点组件」当门。
    //
    // 原因：CF 的组件是**异步**渲染进 closed shadow root 的，open 完成的那一刻
    // 它往往还没建出来，页面脚本如实报 interactive=false；等组件出现时，
    // 旧代码已经进了被动等待分支，再也不会去点。求解器本身是元素驱动的
    // （每轮先看 token/是否离开挑战页，再看组件在不在），没有组件时它会很快
    // 返回 no-widget-frame，不会白耗预算；而要点的页面只有它能过。
    let solve_cap = Duration::from_millis((timeout_secs * 800.0) as u64)
        .min(Duration::from_secs(25))
        .max(Duration::from_millis(2000));
    // 从总预算里扣，而不是重新给 0.8T。
    let solve_budget = budget_left().min(solve_cap);
    let mut solved = challenge_auto_solve(state, session_key, solve_budget, None, None).await;
    // 一轮没解掉时整页 reload 再解一次：CF 每次下发的挑战不是同一个 ray，
    // 卡住的那一轮换个 ray 往往就过了。只在「预算还剩得下第二轮」时才做，
    // 并且 reload 后用元素确认页面已经落定（不是睡固定时间）。
    if solved.get("solved").and_then(Value::as_bool) != Some(true) {
        let reason = solved.get("reason").and_then(Value::as_str).unwrap_or("");
        let worth_retry = matches!(reason, "verifying_not_completed" | "click-no-effect");
        let remaining = solve_budget.saturating_sub(Duration::from_millis(
            solved
                .get("elapsed_ms")
                .and_then(Value::as_u64)
                .unwrap_or(default_solve_ms(solve_budget)),
        ));
        // 第二轮只能花「总预算里真正剩下的」：reload 与等待都算在内，至少留 4s。
        if worth_retry
            && remaining > Duration::from_secs(4)
            && budget_left() > Duration::from_secs(4)
        {
            let _ = execute_page_js_with_target(
                state,
                "location.reload(); return 'reload'",
                SessionSelector::new(Some(session_key.to_string()), None, None),
                true,
                Duration::from_secs(5),
                &ExecTargetOptions::default(),
            )
            .await;
            // 等页面重新落定：status 不再是 pending 才算新挑战页就绪。
            // 单次查询的超时同样取「本循环剩余」与「总预算剩余」的较小值：
            // 旧实现固定 6s，reload 窗口只有 8s 时最后那次调用能把总预算顶穿。
            let reload_deadline = Instant::now() + Duration::from_secs(8).min(budget_left());
            while Instant::now() < reload_deadline {
                let left = reload_deadline
                    .saturating_duration_since(Instant::now())
                    .min(budget_left());
                if left < Duration::from_millis(50) {
                    break;
                }
                if let Ok(status) = challenge_page_call(
                    state,
                    session_key,
                    json!({ "action": "status" }),
                    None,
                    left.min(Duration::from_secs(6)),
                )
                .await
                {
                    let pending = status
                        .get("pending")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !pending {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            solved =
                challenge_auto_solve(state, session_key, remaining.min(budget_left()), None, None)
                    .await;
        }
    }
    if solved.get("solved").and_then(Value::as_bool) == Some(true) {
        return json!({
            "detected": true,
            "waited": true,
            "cleared": true,
            "via": "click",
            "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
            "kind": status.get("kind").cloned().unwrap_or(Value::Null),
            "auto_solve": solved,
            // 成功与否都带上：诊断数组为空时字段为空数组，不影响旧消费方。
            "diag": diag,
        });
    }
    if !blocking {
        // 组件类挑战点了也没过：如实说需要人，绝不能再报 cleared:true。
        return json!({
            "detected": true,
            "waited": false,
            "cleared": false,
            "need_human": true,
            "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
            "kind": status.get("kind").cloned().unwrap_or(Value::Null),
            "hints": status.get("hints").cloned().unwrap_or_else(|| json!([])),
            "auto_solve": solved,
            "diag": diag,
        });
    }
    // 直接轮询 `status`（350ms 一次）而不是注入「4 秒一片」的等待脚本。
    //
    // 后者必须等一整片结束才回报，页面导航时还会撞上「执行上下文被销毁」，
    // 恢复得慢；纯 interstitial 页面本来就不需要页面内观察器（没有任何可点/可等的东西，
    // 只是等 CF 脚本跑完并跳转）。实测同一次挑战：分片等待 12~15s 仍未识别为放行，
    // 轮询 350ms 在 1s 内就能看到 `detected:false`。
    let budget = budget_left();
    let deadline = Instant::now() + budget;
    let (cleared, last, _verifying) =
        challenge_wait_result(state, session_key, budget, deadline).await;
    // hints 以**最后一轮**的为准：首轮往往还在「页面没跑完」的阶段，
    // 而「Turnstile 没渲染」「窗口不可见」这些结论只有后面才出得来。
    let hints = last
        .get("hints")
        .cloned()
        .filter(|value| {
            value
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false)
        })
        .or_else(|| status.get("hints").cloned())
        .unwrap_or_else(|| json!([]));
    // 没放行时要给出**可操作的原因**。只回一个 `need_human: true` 等于没说：
    // 用户无从判断是「再等等」「点一下」还是「这台机器根本过不去」。
    let reason = if cleared {
        Value::Null
    } else if last.get("stalled").and_then(Value::as_bool) == Some(true) {
        json!("turnstile_not_rendered")
    } else if last
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
        .unwrap_or(false)
    {
        json!("window_not_visible")
    } else {
        json!("timeout")
    };
    json!({
        "detected": true,
        "waited": true,
        "cleared": cleared,
        "need_human": !cleared,
        "reason": reason,
        "vendor": status.get("vendor").cloned().unwrap_or(Value::Null),
        "kind": status.get("kind").cloned().unwrap_or(Value::Null),
        "hints": hints,
        "env": last.get("env").cloned().unwrap_or(Value::Null),
        "turnstile": last.get("turnstile").cloned().unwrap_or(Value::Null),
        // 求解过程的结论要跟着一起交出去：只回一个 need_human，用户无从知道
        // 是「组件没找到」「点了没反应」还是「点了但 CF 一直不给结果」。
        "auto_solve": solved,
        "stalled_ms": last.get("stalled_ms").cloned().unwrap_or(Value::Null),
        "wait": last,
        "diag": diag,
    })
}

/// 会出现「人机验证组件」的 frame URL 特征。
/// 命中才有必要做几何定位；普通 iframe（广告、编辑器）一律不碰。
pub(crate) const CHALLENGE_FRAME_HINTS: [&str; 9] = [
    "challenges.cloudflare.com",
    "cdn-cgi/challenge-platform",
    "recaptcha/api2",
    "recaptcha/enterprise",
    "hcaptcha.com",
    "captcha-delivery.com",
    "px-captcha",
    "awswaf.com",
    "arkoselabs.com",
];

/// 一个验证码 iframe 在页面视口里的几何。
///
/// `node_id` 只用于**组件签名**（同 URL 重载时 nodeId 会变，能识别出「换了一个
/// 组件实例」），不参与坐标换算：页面一重排，nodeId 对应的位置就失效了。
pub(crate) struct ChallengeFrame {
    pub(crate) url: String,
    /// DOM nodeId（组件签名的一部分，见 `challenge_frame_signature`）。
    pub(crate) node_id: i64,
    /// content box 四角（TL, TR, BR, BL），已包含 CSS transform 的效果。
    pub(crate) quad: [f64; 8],
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl ChallengeFrame {
    /// 局部坐标（iframe 内容坐标，CSS px）→ 页面视口坐标。
    ///
    /// 不能简单相加：有的站点（如 nowsecure.nl 的测试页）给挑战 iframe 加 3D 旋转，
    /// 纯加法点不到勾选框。这里用四角反推仿射基向量，旋转/缩放/斜切都能对上。
    pub(crate) fn to_screen(&self, lx: f64, ly: f64) -> (f64, f64) {
        let [x1, y1, x2, y2, _x3, _y3, x4, y4] = self.quad;
        let ux = x2 - x1;
        let uy = y2 - y1;
        let vx = x4 - x1;
        let vy = y4 - y1;
        let u = if self.width > 0.0 {
            lx / self.width
        } else {
            0.0
        };
        let v = if self.height > 0.0 {
            ly / self.height
        } else {
            0.0
        };
        (x1 + u * ux + v * vx, y1 + u * uy + v * vy)
    }

    /// 同 [`ChallengeFrame::to_screen`]，但把落在视口外的负坐标夹回 0。
    ///
    /// 用户给的点击坐标 / 组件内的落点都不该是负数：仿射变换后的负值只可能来自
    /// 「组件被滚出视口 / 旋转后越界」。夹紧比把一个 -37px 的坐标直接发给 CDP 好 ——
    /// 后者在部分 Chrome 版本上会被静默拒绝。返回 `(坐标, 是否夹紧)`，
    // 调用方把 `true` 写进 diag。
    pub(crate) fn to_screen_clamped(&self, lx: f64, ly: f64) -> ((f64, f64), bool) {
        let (sx, sy) = self.to_screen(lx, ly);
        let cx = sx.max(0.0);
        let cy = sy.max(0.0);
        ((cx, cy), cx != sx || cy != sy)
    }
}

// 组件签名：识别「同一个组件」还是「换了一个实例」。
///
/// 旧实现只用 iframe URL：页面用同一个 URL 重新加载组件时签名不变，
/// 求解器会把新组件当成旧的，验证期继续补点，把流程打回原点。
/// nodeId 在文档重载 / 组件替换时都会变；quad 尺寸哈希覆盖「同一 node 被重排成
/// 另一个醒目组件」的情况。targetId 拿不到（DOM node 与 Target 没有直接映射），
/// nodeId 已覆盖它在本场景里的作用。
pub(crate) fn challenge_frame_signature(frame: &ChallengeFrame) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for value in frame.quad {
        // f64 不能直接 Hash：先量化到 0.1px，避免亚像素抖动被当成新组件。
        ((value * 10.0).round() as i64).hash(&mut hasher);
    }
    frame.node_id.hash(&mut hasher);
    frame.url.hash(&mut hasher);
    format!(
        "{}:{}:{}x{}:{:x}",
        frame.node_id,
        frame.url,
        frame.width.round() as i64,
        frame.height.round() as i64,
        hasher.finish()
    )
}

/// 取一个 iframe 元素「真正加载的文档 URL」。
///
/// 不能只读 src 属性：CF 的挑战 iframe 常常先用 `srcdoc`/属性为空、再用 JS 赋
/// `contentWindow.location` 或 `document.write` 填充，属性值读不到真实地址。
/// describeNode(depth:0) 的 contentDocument.documentURL 是实际导航地址，更可靠。
///
// 每次 CDP 调用前都按 `deadline` 重算剩余量；预算归零立刻返回空串
/// （调用方会把它当成「这一帧没探到」，不再往下花时间）。
pub(crate) async fn iframe_doc_url(
    state: &AppState,
    session_key: &str,
    node_id: i64,
    deadline: Instant,
    diag: &mut Vec<Value>,
) -> String {
    let left = match deadline_left(deadline) {
        Some(left) => left,
        None => {
            push_diag_error(
                diag,
                "describeNode",
                "deadline",
                "iframe 探测预算耗尽",
                Some(node_id),
            );
            return String::new();
        }
    };
    match cdp_call(
        state,
        session_key,
        "DOM.describeNode",
        json!({ "nodeId": node_id, "depth": 0, "pierce": true }),
        left,
    )
    .await
    {
        Ok(result) => {
            if let Some(url) = result
                .data
                .as_ref()
                .and_then(|data| data.pointer("/node/contentDocument/documentURL"))
                .and_then(Value::as_str)
            {
                if !url.is_empty() {
                    return url.to_string();
                }
            }
        }
        Err(err) => push_diag_error(
            diag,
            "describeNode",
            "cdp_error",
            &err.to_string(),
            Some(node_id),
        ),
    }
    // 退一步读属性（src / data-src）。
    let left = match deadline_left(deadline) {
        Some(left) => left,
        None => {
            push_diag_error(
                diag,
                "getAttributes",
                "deadline",
                "iframe 探测预算耗尽",
                Some(node_id),
            );
            return String::new();
        }
    };
    let attrs = match cdp_call(
        state,
        session_key,
        "DOM.getAttributes",
        json!({ "nodeId": node_id }),
        left,
    )
    .await
    {
        Ok(result) if result.ok => result.data,
        Ok(result) => {
            push_diag_error(
                diag,
                "getAttributes",
                "cdp_rejected",
                result.error.as_deref().unwrap_or("扩展未给出原因"),
                Some(node_id),
            );
            None
        }
        Err(err) => {
            push_diag_error(
                diag,
                "getAttributes",
                "cdp_error",
                &err.to_string(),
                Some(node_id),
            );
            None
        }
    }
    .and_then(|data| data.get("attributes").and_then(Value::as_array).cloned())
    .unwrap_or_default();
    let mut src = String::new();
    for pair in attrs.chunks(2) {
        let Some(name) = pair.first().and_then(Value::as_str) else {
            continue;
        };
        let value = pair.get(1).and_then(Value::as_str).unwrap_or_default();
        if name.eq_ignore_ascii_case("src") || name.eq_ignore_ascii_case("data-src") {
            src = value.to_string();
        }
    }
    src
}

/// 收集文档里所有 iframe 的 nodeId（常规 DOM + shadow root 内部，去重）。
///
/// 只在已确认「页面存在挑战」时调用，那时页面很小，全量 pierce 的代价可控；
/// 换来的是不漏掉 shadow root 里的组件。
///
// 第二个返回值是 的成败标记：**查询本身失败**（CDP 附不上 / DOM 域不可用）
/// 与「文档里真的没有 iframe」是两件事。
///
// 旧实现先 `DOM.getDocument{depth:1}` 拿 rootId 做 querySelectorAll，
/// 再 `DOM.getDocument{depth:-1}` 全量遍历，两次回包的 nodeId 属于**不同的前端节点
/// 映射**，混用会把别的节点当 iframe。现在只做一次全量 getDocument，
/// 所有 iframe 都从同一棵树里取。
pub(crate) async fn collect_iframe_ids(
    state: &AppState,
    session_key: &str,
    deadline: Instant,
    diag: &mut Vec<Value>,
) -> (Vec<i64>, bool) {
    let Some(left) = deadline_left(deadline) else {
        push_diag_error(diag, "getDocument", "deadline", "iframe 探测预算耗尽", None);
        return (Vec::new(), true);
    };
    match cdp_call(
        state,
        session_key,
        "DOM.getDocument",
        json!({ "depth": -1, "pierce": true }),
        left,
    )
    .await
    {
        Ok(result) if result.ok => {
            let Some(root) = result.data.as_ref().and_then(|data| data.get("root")) else {
                push_diag_error(
                    diag,
                    "getDocument",
                    "invalid_response",
                    "DOM.getDocument 回包缺少 root",
                    None,
                );
                return (Vec::new(), true);
            };
            let mut ids = Vec::new();
            collect_iframe_nodes(root, &mut ids);
            ids.sort_unstable();
            ids.dedup();
            (ids, false)
        }
        Ok(result) => {
            push_diag_error(
                diag,
                "getDocument",
                "cdp_rejected",
                result.error.as_deref().unwrap_or("扩展未给出原因"),
                None,
            );
            (Vec::new(), true)
        }
        Err(err) => {
            push_diag_error(diag, "getDocument", "cdp_error", &err.to_string(), None);
            (Vec::new(), true)
        }
    }
}

/// 从 pierce 过的 DOM 树里收集 iframe 的 nodeId（含 shadow root 内部）。
///
/// 深度上限与 `html.rs` 的 `MAX_DOM_DEPTH` 同阈值：`DOM.getDocument{depth:-1}`
/// 给出的是整棵文档树，构造一个几千层嵌套的页面就能让这个递归在 tokio worker
/// 的 2MB 栈上爆栈（实测栈溢出是 abort，不是 panic）。正常页面（含深层组件库）
/// 远到不了 512 层，命中即停止下钻——少收集几个深层 iframe 也不会漏掉验证码组件，
/// 因为验证码 iframe 都挂在可见的浅层容器里。
pub(crate) const MAX_IFRAME_DOM_DEPTH: usize = 512;

pub(crate) fn collect_iframe_nodes(node: &Value, out: &mut Vec<i64>) {
    collect_iframe_nodes_at(node, out, 0);
}

fn collect_iframe_nodes_at(node: &Value, out: &mut Vec<i64>, depth: usize) {
    if depth > MAX_IFRAME_DOM_DEPTH {
        return;
    }
    if node.get("nodeName").and_then(Value::as_str) == Some("IFRAME") {
        if let Some(id) = node.get("nodeId").and_then(Value::as_i64) {
            out.push(id);
        }
    }
    for key in ["children", "shadowRoots"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            for child in children {
                collect_iframe_nodes_at(child, out, depth + 1);
            }
        }
    }
}

/// CDP 附加失败是不是「页面里嵌了别的扩展的 iframe」造成的。
///
/// Chrome 的硬限制：一个标签页里只要存在 `chrome-extension://<别的扩展>` 的 iframe，
/// chrome.debugger 就拒绝 attach（"Cannot access a chrome-extension:// URL of
/// different extension"）。这不是本工具的 bug，但会让验证码定位/点击整条链路失效，
/// 所以必须识别出来并给出可操作的处理，而不是报一句 "no iframe element in document"。
pub(crate) fn is_foreign_extension_frame_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("different extension")
        || lower.contains("chrome-extension:// url")
        || message.contains("内嵌了其他扩展")
}

/// 摘除其它扩展 iframe 的结果。
///
/// `SkippedUnknownOwnId` 表示「拿不到本扩展 id，因此一个都没删」：
// 这与「摘了 0 个」必须可区分，调用方要据此换回落通道。
pub(crate) enum StripFramesOutcome {
    Stripped(u64),
    SkippedUnknownOwnId,
}

/// 本扩展的 id：优先用 `ATB_EXTENSION_ID`，否则取当前 WS 连接握手时的扩展 id。
///
/// 自己注入的 iframe 同样以 `chrome-extension://<own-id>` 开头，必须白名单放行；
/// 取不到就不删（见 `strip_foreign_extension_frames`）。
async fn own_extension_id(state: &AppState, session_key: &str) -> Option<String> {
    let from_env = crate::settings::extension_id();
    if !from_env.is_empty() {
        return Some(from_env);
    }
    let conn_id = {
        let driver = state.driver.read().await;
        driver
            .sessions
            .get(session_key)
            .map(|session| session.conn_id)
    }?;
    let connections = state.ws_connections.lock().ok()?;
    connections
        .get(&conn_id)
        .map(|connection| connection.extension_id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// 摘掉页面里别的扩展注入的 iframe，让 chrome.debugger 能 attach。
///
/// 只处理当前文档这一次：这类扩展会在下一次导航时重新注入，做成持久化反而会
/// 影响用户其它页面。返回值是摘掉的个数，写进 diag 让排障能看到「为什么突然能用了」。
///
// 旧实现无差别删除**所有** `chrome-extension://` iframe，包括本工具自己
/// 注入的组件和用户合法扩展。现在只删「扩展 id ≠ 自身 id」的；自身 id 拿不到时
/// **一个都不删**，返回 `SkippedUnknownOwnId` 让调用方换回落通道并如实记录。
///
// 失败必须返回 `Err`：旧实现 `.ok()...unwrap_or(0)` 把「脚本根本没跑」
/// 和「跑完了、一个都没摘掉」压成同一个 0，排障时完全看不出 attach 为什么还是失败。
pub(crate) async fn strip_foreign_extension_frames(
    state: &AppState,
    session_key: &str,
) -> Result<StripFramesOutcome> {
    let Some(own_id) = own_extension_id(state, session_key).await else {
        return Ok(StripFramesOutcome::SkippedUnknownOwnId);
    };
    let own = json!([own_id]);
    let script = format!(
        "const own = {own};\
         const prefix = 'chrome-extension://';\
         const frames = Array.prototype.slice.call(document.querySelectorAll('iframe'))\
         .filter(function (frame) {{\
             const src = String(frame.src || '');\
             if (src.indexOf(prefix) !== 0) return false;\
             const id = src.slice(prefix.length).split('/')[0].split('?')[0].split('#')[0];\
             return own.indexOf(id) < 0;\
         }});\
         frames.forEach(function (frame) {{ try {{ frame.remove(); }} catch (_) {{}} }});\
         return frames.length;"
    );
    let executed = execute_page_js_with_target(
        state,
        &script,
        SessionSelector::new(Some(session_key.to_string()), None, None),
        true,
        Duration::from_secs(5),
        &ExecTargetOptions::default(),
    )
    .await?;
    let stripped = executed
        .get("js_return")
        .cloned()
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|number| number as u64))
        })
        // 没有计数就等于「不知道摘了几个」：不能让调用方以为已经处理干净了。
        .ok_or_else(|| anyhow!("strip-foreign-frames: 页面脚本未返回摘除计数"))?;
    Ok(StripFramesOutcome::Stripped(stripped))
}
