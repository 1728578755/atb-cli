use super::*;

/// 受限站点（页面内嵌其他扩展的 iframe / 扩展隔离）时 Chrome debugger 的报错原文。
/// 用**精确子串**匹配：这是 chrome.debugger 对「attach 到含其他扩展
/// iframe 的页面」的固定文案，不会出现在普通页面错误里，几乎不可能误判。
const PAGE_PROTECTED_NEEDLE: &str =
    "cannot access a chrome-extension:// url of different extension";

/// 判断原始 CDP / 扩展错误是否为受限站点错误。
pub(crate) fn is_page_protected_error(message: &str) -> bool {
    message.to_lowercase().contains(PAGE_PROTECTED_NEEDLE)
}

/// 受限站点错误的结构化形态。
///
/// 旧实现把 Chrome 原文（"Cannot access a chrome-extension:// URL of different
/// extension"）原样抛出：调用方既不知道这是页面隔离问题，也不知道 CDP 之外还有
/// 无 eval 通道可用。这里统一翻译：`Display` 保持人话结论（任何只读错误文本的
/// 消费方都能看懂），HTTP 出口（`action.rs` 的 `json_result`）会 downcast 本类型，
/// 把稳定 `error_code:"page_protected"` 与 `hint` 提到响应顶层。
/// Debug 必须派生：anyhow 的 downcast_ref 要求 E: Debug（action.rs / exec.rs 都要 downcast）。
#[derive(Debug)]
pub(crate) struct PageProtectedError {
    pub(crate) raw: String,
}

impl std::fmt::Display for PageProtectedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "该页面禁止调试器注入（页面内嵌了其他扩展的 iframe 或站点 CSP/扩展隔离）"
        )
    }
}

impl std::error::Error for PageProtectedError {}

/// 命中受限站点错误时替换为结构化错误，否则原样返回。
pub(crate) fn translate_page_protected_error(err: anyhow::Error) -> anyhow::Error {
    if is_page_protected_error(&err.to_string()) {
        return anyhow::Error::new(PageProtectedError {
            raw: err.to_string(),
        });
    }
    err
}

pub(crate) async fn session_tab_id(state: &AppState, session_key: &str) -> Result<String> {
    let driver = state.driver.read().await;
    driver
        .sessions
        .get(session_key)
        .filter(|s| s.is_active())
        .map(|s| s.tab_id.clone())
        .ok_or_else(|| anyhow!("会话ID {session_key} 未连接"))
}

pub(crate) async fn cdp_call(
    state: &AppState,
    session_key: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<CdpCallResult> {
    // 「不接触」模式：明确禁止附加调试器。这样浏览器不会出现
    // 「已开始调试此浏览器」横幅，也不会因为 attach 改变页面可见状态。
    if crate::settings::no_debugger() {
        return Err(anyhow!(
            "已启用 ATB_NO_DEBUGGER（旧名 AGENT_BROWSER_CLI_NO_DEBUGGER）：{method} 需要 CDP 调试器，已拒绝执行。\n\
             可用替代：截图去掉 --full-page/--selector（走 captureVisibleTab）；\
             读页面用 read/scan/snapshot --js；网络用 net hook（页面侧）；\
             确认要调试器时，取消该环境变量后重试"
        ));
    }
    let tab_id = session_tab_id(state, session_key).await?;
    let script = json!({ "cmd": "cdp", "tabId": parse_tab_id(&tab_id)?, "method": method, "params": params }).to_string();
    // 注入 / 传输层错误出口做受限站点翻译（原样错误是 Chrome debugger 的
    // 英文原文，智能体无法分支判断）。
    let response = match execute_raw_js_on_tab(state, &script, session_key, timeout).await {
        Ok(response) => response,
        Err(err) => return Err(translate_page_protected_error(err)),
    };
    let value = response.data.or(response.result).unwrap_or(Value::Null);
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(CdpCallResult {
            ok: true,
            data: value.get("data").cloned(),
            error: None,
        }),
        Some(false) => {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
            // CDP 协议层报「跨扩展 iframe 禁止访问」时，这条 CDP 调用在该页面上
            // 不可能成功。转成 Err（而不是 Ok(ok:false)）让上层统一走各自的降级链：
            // snapshot → JS 快照回退、action → 无 eval 通道，且 HTTP 出口能带出
            // 结构化 error_code。
            if error
                .as_deref()
                .map(is_page_protected_error)
                .unwrap_or(false)
            {
                return Err(anyhow::Error::new(PageProtectedError {
                    raw: error.unwrap_or_default(),
                }));
            }
            Ok(CdpCallResult {
                ok: false,
                data: value.get("data").cloned(),
                error,
            })
        }
        None => {
            // 缺 `ok` 的回包以前一律当成功。对象载荷（旧扩展就是这样回包的）
            // 照旧兼容；标量 / 数组 / null 说明这条回包根本没法判成败
            // （典型：只剩一句 "response missing" 文本），把它当 data 继续解析
            // 只会退化成难以归因的空结果 —— 这里直接报 invalid_response。
            if !value.is_object() {
                return Err(anyhow!(
                    "cdp {method}: invalid_response（扩展回包缺少 ok 字段，载荷不是对象）：{}",
                    truncate_text(&value.to_string(), 200)
                ));
            }
            Ok(CdpCallResult {
                ok: true,
                data: Some(value),
                error: None,
            })
        }
    }
}

pub(crate) async fn execute_raw_js_on_tab(
    state: &AppState,
    code: &str,
    session_key: &str,
    timeout: Duration,
) -> Result<ExecResult> {
    // 显式携带 session_key，不再「临时改写全局 default_session_key → 执行 → 还原」。
    // 旧写法在 job 并发时会互相覆盖路由，导致命令打到别的标签页。
    Ok(
        execute_raw_js_with_session(state, code, timeout, Some(session_key))
            .await?
            .result,
    )
}

pub(crate) async fn batch_cdp_call(
    state: &AppState,
    session_key: &str,
    commands: Vec<Value>,
    timeout: Duration,
) -> Result<Value> {
    let expected = commands.len();
    let tab_id = session_tab_id(state, session_key).await?;
    let script = json!({
        "cmd": "batch",
        "tabId": parse_tab_id(&tab_id)?,
        "commands": commands,
    })
    .to_string();
    // 与 cdp_call 同判据，batch 的三个错误出口（传输 / 顶层 ok:false /
    // 单步失败汇总）都做受限站点翻译。
    let response = match execute_raw_js_on_tab(state, &script, session_key, timeout).await {
        Ok(response) => response,
        Err(err) => return Err(translate_page_protected_error(err)),
    };
    let raw = response.data.or(response.result).unwrap_or(Value::Null);
    // 兼容「裸数组」回包：部分旧扩展 / 页面注入通道直接返回 `results` 数组本身，
    // 没有 `{ok, results, failed}` 外壳。先补壳再走同一套校验，既不放松
    // 的条数/index 检查，也不会把正常回包误判成协议错误。
    let value = if let Some(array) = raw.as_array() {
        let failed = array
            .iter()
            .filter(|item| item.get("ok").and_then(Value::as_bool) == Some(false))
            .count();
        json!({ "ok": true, "results": array, "failed": failed })
    } else {
        raw
    };
    let ok_flag = value.get("ok").and_then(Value::as_bool);
    if ok_flag == Some(false) {
        return Err(translate_page_protected_error(anyhow!(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("batch cdp failed")
            .to_string())));
    }
    // 缺 `ok` 的 batch 回包同样不能默认成功（只有 `Some(false)` 才判失败的
    // 写法会把标量垃圾载荷当成成功）。对象载荷（带 results）兼容旧扩展，
    // 非对象一律报 invalid_response 并带截断原文。
    if ok_flag != Some(true) && !value.is_object() {
        return Err(anyhow!(
            "batch cdp: invalid_response（回包缺少 ok 字段，载荷不是对象）：{}",
            truncate_text(&value.to_string(), 200)
        ));
    }
    // 扩展的 `batch` 回包恒为 `{ok:true, results:[...], failed:N}`：单条命令失败只记在
    // `results[i].ok=false`，顶层照样 ok。以前这里只查顶层，于是「文件没挂上去」
    // 「鼠标事件没发出去」全都报成功。
    // 先校验结构自洽：条数 / index / failed 计数任一对不上，下面所有
    // 「按位置取第 N 步结果」的读法都会静默取到别的步骤的结果，比直接失败更危险。
    validate_batch_response(&value, expected)?;
    let failures = batch_step_failures(&value);
    if !failures.is_empty() {
        return Err(translate_page_protected_error(anyhow!(
            "batch cdp failed: {}",
            failures.join("; ")
        )));
    }
    Ok(value)
}

/// 校验 `batch` 回包的结构自洽性。
///
/// 扩展约定：`{ok:true, results:[{index, ok, ...}], count, failed}`，`results` 与
/// `commands` **一一对应且同序**，`index` 就是下发时的位置。下游全部按位置取值
/// （`results[i*2+1]`、`$3.result.objectId`），所以只要条数 / 顺序 / 计数有一项
/// 对不上，拿到的就是**别的步骤**的结果：例如把 `DOM.resolveNode` 的回包当成
/// `Runtime.callFunctionOn` 的回包，然后把「解析成功」报成「点击成功」。
///
/// 因此空 / 短 / 多 / index 重复 / 乱序 / failed 计数不符一律 `protocol_error`，
/// 并在错误里带「期望多少条、实际多少条」，让调用方一眼看出是回包残缺而不是页面问题。
///
/// 两处调用点共用本函数：`batch_cdp_call`（通用）与 `batch_dom_details`
/// （snapshot --details 的 2N 条 resolveNode+callFunctionOn）。
pub(crate) fn validate_batch_response(value: &Value, expected: usize) -> Result<()> {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "protocol_error: batch 回包缺少 results 数组（期望 {expected} 条，实际 0 条）：{}",
                truncate_text(&value.to_string(), 200)
            )
        })?;
    if results.len() != expected {
        return Err(anyhow!(
            "protocol_error: batch 回包条数与命令数不符（期望 {expected} 条，实际 {} 条）",
            results.len()
        ));
    }
    let mut seen = vec![false; expected];
    for (position, item) in results.iter().enumerate() {
        // 旧扩展的 batch 回包不带 `index`（顺序即位置）。有就按 index 校验，
        // 没有就按出现顺序当 index：不能因为「缺一个装饰字段」把正常结果全判死。
        let index = match item.get("index").and_then(Value::as_u64) {
            Some(raw_index) => raw_index as usize,
            None => position,
        };
        if index >= expected {
            return Err(anyhow!(
                "protocol_error: batch 回包的 index 超范围（第 {} 条自带 index={index}，期望 0..{}）",
                position + 1,
                expected.saturating_sub(1)
            ));
        }
        if seen[index] {
            return Err(anyhow!(
                "protocol_error: batch 回包的 index 重复（index={index} 出现多次；期望 {expected} 条、实际 {} 条）",
                results.len()
            ));
        }
        seen[index] = true;
        if index != position {
            return Err(anyhow!(
                "protocol_error: batch 回包乱序（第 {} 条自带 index={index}，期望 {position}）",
                position + 1
            ));
        }
    }
    let failed_in_results = results
        .iter()
        .filter(|item| item.get("ok").and_then(Value::as_bool) == Some(false))
        .count();
    match value.get("failed").and_then(Value::as_u64) {
        Some(failed) if failed as usize == failed_in_results => Ok(()),
        Some(failed) => Err(anyhow!(
            "protocol_error: batch 回包的 failed 计数与逐条结果不符（failed={failed}，逐条统计失败 {failed_in_results} 条）"
        )),
        // 老扩展不带 failed 时按「0 失败」解释；此时逐条结果里也不能有失败项，
        // 否则两边的结论互相矛盾，宁可报协议错误也不猜。
        None if failed_in_results == 0 => Ok(()),
        None => Err(anyhow!(
            "protocol_error: batch 回包缺少 failed 计数（逐条统计失败 {failed_in_results} 条，期望 {failed_in_results}）"
        )),
    }
}

/// 逐条读出 `batch` 回包里失败的步骤，供上层拼错误信息。
///
/// 判据有三条，缺一不可：顶层 `failed` 计数、`results[*].ok == false`、
/// 以及 `results[*].data.skipped == true`（`Page.bringToFront` 被 `allowFocus`
/// 跳过时扩展不会报错，但后续鼠标事件其实没落在前台标签上）。
pub(crate) fn batch_step_failures(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(results) = value.get("results").and_then(Value::as_array) {
        for (index, item) in results.iter().enumerate() {
            let label = item
                .get("method")
                .or_else(|| item.get("op"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("step[{index}]"));
            if item.get("ok").and_then(Value::as_bool) == Some(false) {
                let error = item
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("step failed");
                out.push(format!("{label}: {error}"));
                continue;
            }
            if item
                .get("data")
                .and_then(|data| data.get("skipped"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                out.push(format!("{label}: skipped（页面未获得前台焦点）"));
            }
        }
    }
    if out.is_empty() {
        if let Some(failed) = value.get("failed").and_then(Value::as_u64) {
            if failed > 0 {
                out.push(format!("{failed} 条子命令失败"));
            }
        }
    }
    out
}

pub(crate) async fn resolve_target_object(
    state: &AppState,
    tab_id: &str,
    target: &str,
    timeout: Duration,
) -> Result<String> {
    match resolve_ref_lookup(state, tab_id, target, timeout).await? {
        Some(lookup) => {
            if let Some(selector) = lookup.selector {
                Ok(format!("__selector:{selector}"))
            } else {
                Ok(format!("__backend:{}", lookup.backend_dom_node_id))
            }
        }
        None => Ok(format!("__selector:{target}")),
    }
}

/// `@e` 引用的解析结果：有 selector 时走页面内动作，否则退回 CDP backendNodeId。
pub(crate) struct RefLookup {
    pub(crate) selector: Option<String>,
    pub(crate) backend_dom_node_id: i64,
    /// true 表示这个引用是「按 role/name 在新页面上重新找到的」，不是快照原始节点。
    pub(crate) relocated: bool,
}

/// 解析 `@e` 引用，必要时自动重定位。
///
/// 三条路径：
/// 1. URL 未变 + 有 selector → 直接返回（零额外往返）
/// 2. URL 未变 + CDP 引用 → 返回 backendNodeId
/// 3. URL 变了（导航 / SPA 换页）→ 旧节点已失效，用 role+name 在当前页面重新定位
///
// 缓存写回受 generation/URL 校验保护（第 3 点，见
/// [`write_back_relocated_selector`]）：重定位要跨一次进程往返，期间可能有人重新
/// snapshot 过，写回旧快照的 refs 会把新编号覆盖掉。
pub(crate) async fn resolve_ref_lookup(
    state: &AppState,
    session_key: &str,
    target: &str,
    timeout: Duration,
) -> Result<Option<RefLookup>> {
    if !is_ref_target(target) {
        return Ok(None);
    }
    let (entry, snapshot_url, snapshot_generation, current_url) = {
        let driver = state.driver.read().await;
        let cache = driver.snapshots.get(session_key).ok_or_else(|| {
            anyhow!("unknown_ref: {target} 不在 tab {session_key} 的快照里，先执行 snapshot --tab {session_key}")
        })?;
        let entry = cache.refs.get(target).cloned().ok_or_else(|| {
            anyhow!(
                "unknown_ref: {target} 不在 tab {session_key} 的快照里（快照共 {} 个引用），先重新 snapshot",
                cache.refs.len()
            )
        })?;
        (
            entry,
            cache.url.clone(),
            cache.generation,
            driver
                .sessions
                .get(session_key)
                .map(|session| session.info.url.clone())
                .unwrap_or_default(),
        )
    };

    let url_changed =
        !snapshot_url.is_empty() && !current_url.is_empty() && current_url != snapshot_url;
    if !url_changed {
        return Ok(Some(RefLookup {
            selector: entry.selector.clone(),
            backend_dom_node_id: entry.backend_dom_node_id,
            relocated: false,
        }));
    }

    // 页面已导航：backendNodeId 和旧 selector 都可能失效，按语义重新找。
    match relocate_ref_selector(state, session_key, &entry, timeout).await {
        Ok(selector) => {
            // 只有「还是同一份快照」时才写回；期间有人 snapshot 过就放弃（下次再定位）。
            {
                let mut driver = state.driver.write().await;
                write_back_relocated_selector(
                    &mut driver,
                    session_key,
                    target,
                    snapshot_generation,
                    &snapshot_url,
                    &selector,
                );
            }
            Ok(Some(RefLookup {
                selector: Some(selector),
                backend_dom_node_id: 0,
                relocated: true,
            }))
        }
        // `ref_ambiguous` 是「新页面上有多个同名元素」——重定位本身不安全，
        // 原样上抛（带 error_code + 重新 snapshot 的 hint），不要伪装成 ref_expired。
        Err(err) if is_ref_ambiguous(&err) => Err(err),
        Err(_) => Err(anyhow!(
            "ref_expired: {target}（{}「{}」）属于旧页面 {snapshot_url}，当前页面是 {current_url}，且按 role/name 未能在新页面找到同名元素；请重新执行 snapshot",
            entry.role,
            entry.name
        )),
    }
}

/// 语义重定位的错误码判定：`ref_ambiguous` 必须单独上抛，不能被上层吞成「没找到」。
pub(crate) fn is_ref_ambiguous(err: &anyhow::Error) -> bool {
    err.downcast_ref::<SessionSelectError>()
        .is_some_and(|typed| typed.code == "ref_ambiguous")
}

// 把语义重定位的结果写回快照缓存 —— **只有还是同一份快照时**才写。
///
/// 重定位是一次跨进程往返（页面脚本最长等 timeout）。这期间用户可能又 snapshot 了
/// 一次（generation +1，@e 重新编号），或者页面已经导航（url 变了）。此时写回会把
/// 新快照里的 refs 改成旧节点的选择器，之后每条命令都指向错误的元素 —— 宁可不写回
/// （下一次动作会重新定位一次），也不要写脏。返回是否真的写回，便于调用方记诊断。
pub(crate) fn write_back_relocated_selector(
    driver: &mut DriverState,
    session_key: &str,
    target: &str,
    expected_generation: u64,
    expected_url: &str,
    selector: &str,
) -> bool {
    // 快照缓存里的 url 是「拍快照那一刻」的值，重定位是一次跨进程往返，
    // 期间页面完全可能已经导航。只比缓存自身的字段等于「自己跟自己比」，
    // 挡不住写回过期 selector。这里先读实时 URL（clone 出来，避免和下面的可变借用冲突）。
    let live_url = driver
        .sessions
        .get(session_key)
        .map(|session| session.info.url.clone());
    if let Some(live_url) = live_url {
        if !live_url.is_empty() && !expected_url.is_empty() && live_url != expected_url {
            return false;
        }
    }
    let Some(cache) = driver.snapshots.get_mut(session_key) else {
        return false;
    };
    if cache.generation != expected_generation || cache.url != expected_url {
        return false;
    }
    let Some(slot) = cache.refs.get_mut(target) else {
        return false;
    };
    slot.selector = Some(selector.to_string());
    slot.backend_dom_node_id = 0;
    true
}

/// 在当前页面按 role + name 重新定位引用，返回**唯一**的 CSS 选择器。
///
/// 重定位必须唯一。
/// 旧实现返回第一个 role+name 匹配的候选：页面上有两个同名元素（列表里一排
/// 「删除」、页头页脚各一个「提交」）时会把 @e 指到第一个，而且结果会被写回
/// 快照缓存 —— 错一次就永久粘住，后续每个动作都打在错误的元素上。
/// 现在的判定：候选 == 1 才返回；> 1 → `ref_ambiguous`（让调用方重新 snapshot
/// 拿精确引用）；== 0 → `ref_not_found`。两者都带稳定 error_code。
pub(crate) async fn relocate_ref_selector(
    state: &AppState,
    session_key: &str,
    entry: &ElementRef,
    timeout: Duration,
) -> Result<String> {
    let want_name = normalize_ref_name(&entry.name);
    if want_name.is_empty() {
        return Err(ref_not_found_error(
            entry,
            "该引用没有可用的 name，无法按 role/name 语义重定位",
        ));
    }
    let opts = json!({ "limit": 600, "interactiveOnly": false });
    let script = format!(
        "{}\nreturn __abcSnapshot({});",
        html::js_page_snapshot(),
        opts
    );
    let result = execute_page_js(
        state,
        &script,
        SessionSelector::new(Some(session_key.to_string()), None, None),
        true,
        timeout,
    )
    .await
    .map_err(|err| ref_not_found_error(entry, &format!("重定位时读取页面失败：{err}")))?;
    let payload = result
        .get("js_return")
        .cloned()
        .ok_or_else(|| ref_not_found_error(entry, "重定位脚本没有返回结果"))?;
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(ref_not_found_error(entry, "重定位脚本报告页面快照失败"));
    }
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ref_not_found_error(entry, "重定位脚本没有返回 items 列表"))?;
    let want_role = entry.role.to_lowercase();
    let mut hits: Vec<String> = Vec::new();
    for item in items {
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        // role 必须也相等：只看 name 会在「列表里多个『删除』按钮」时把 ref 重定位到
        // 列表首个元素（role 不同也照认）。两个条件都满足才算候选。
        if role != want_role {
            continue;
        }
        let name = normalize_ref_name(item.get("name").and_then(Value::as_str).unwrap_or(""));
        if name != want_name {
            continue;
        }
        let selector = item
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if selector.is_empty() {
            continue;
        }
        // 同一元素可能在列表里出现两次（例如被 shadow root 与常规 DOM 各遍历一遍），
        // 选择器相同的算同一个候选。
        if !hits.iter().any(|hit| hit == selector) {
            hits.push(selector.to_string());
        }
    }
    match hits.len() {
        0 => Err(ref_not_found_error(
            entry,
            "当前页面没有 role 与 name 都相同的元素",
        )),
        1 => Ok(hits.remove(0)),
        count => Err(ref_ambiguous_error(entry, count, &hits)),
    }
}

/// 「多个候选」的结构化错误：绝不挑一个用，交给调用方重新 snapshot。
pub(crate) fn ref_ambiguous_error(
    entry: &ElementRef,
    count: usize,
    hits: &[String],
) -> anyhow::Error {
    let sample = hits.iter().take(3).cloned().collect::<Vec<_>>().join(" | ");
    SessionSelectError {
        code: "ref_ambiguous",
        message: format!(
            "ref_ambiguous: 语义重定位命中 {count} 个 role={} name=\"{}\" 的元素（{}），无法唯一确定 @{}",
            entry.role,
            entry.name,
            truncate_text(&sample, 200),
            entry.index
        ),
        hint: "不要猜：重新执行 snapshot 拿到新的 @e 引用，或直接用唯一的 CSS 选择器指定目标".to_string(),
        retryable: false,
        retry_after_ms: None,
    }
    .into_anyhow()
}

/// 「找不到」的结构化错误。
pub(crate) fn ref_not_found_error(entry: &ElementRef, reason: &str) -> anyhow::Error {
    SessionSelectError {
        code: "ref_not_found",
        message: format!(
            "ref_not_found: @e{}（role={} name=\"{}\"）无法在当前页面重新定位：{reason}",
            entry.index, entry.role, entry.name
        ),
        hint: "页面可能已经换页或被重新渲染：重新执行 snapshot 后再操作".to_string(),
        retryable: false,
        retry_after_ms: None,
    }
    .into_anyhow()
}

pub(crate) fn normalize_ref_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(crate) fn is_ref_target(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("@e") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

pub(crate) async fn call_function_resolved(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    function_declaration: &str,
    timeout: Duration,
) -> Result<Value> {
    let (value, steps) = if let Some(raw) = object_id.strip_prefix("__backend:") {
        let backend: i64 = raw.parse()?;
        (
            batch_cdp_call(
                state,
                tab_id,
                vec![
                    json!({ "cmd": "cdp", "method": "DOM.resolveNode", "params": { "backendNodeId": backend } }),
                    json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$0.object.objectId", "functionDeclaration": function_declaration, "returnByValue": true } }),
                ],
                timeout,
            )
            .await?,
            "DOM.resolveNode → Runtime.callFunctionOn",
        )
    } else if let Some(selector) = object_id.strip_prefix("__selector:") {
        let expression = format!(
            "document.querySelector({})",
            serde_json::to_string(selector)?
        );
        (
            batch_cdp_call(
                state,
                tab_id,
                vec![
                    json!({ "cmd": "cdp", "method": "Runtime.evaluate", "params": { "expression": expression, "returnByValue": false } }),
                    json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$0.result.objectId", "functionDeclaration": function_declaration, "returnByValue": true } }),
                ],
                timeout,
            )
            .await
            .map_err(|err| selector_missing_target(err, selector))?,
            "Runtime.evaluate → Runtime.callFunctionOn",
        )
    } else {
        return call_function_value(state, tab_id, object_id, function_declaration, timeout).await;
    };
    let step = value
        .get("results")
        .and_then(Value::as_array)
        .and_then(|items| items.get(1))
        .or_else(|| value.as_array().and_then(|items| items.get(1)))
        .ok_or_else(|| {
            // 报告「哪条链路、第几步」缺结果，而不是笼统一句 missing value。
            anyhow!(
                "{steps}: batch 回包缺少第 2 步（Runtime.callFunctionOn）结果：{}",
                truncate_text(&value.to_string(), 300)
            )
        })?;
    // 页面脚本抛异常时 CDP 仍然回 ok，只是没有 result、异常文本在 exceptionDetails 里。
    // 旧实现不查它，所有页面异常统一退化成 "response missing value"。
    if let Some(message) = cdp_exception_text(step) {
        return Err(anyhow!("{steps}: {message}"));
    }
    step.get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "{steps}: Runtime.callFunctionOn 没有返回 result.value（第 2 步回包：{}）",
                truncate_text(&step.to_string(), 300)
            )
        })
}

/// 从 batch 单步（或 `cdp_call` 的 data）里提取 CDP `exceptionDetails` 的可读文本。
pub(crate) fn cdp_exception_text(step: &Value) -> Option<String> {
    let details = step.get("exceptionDetails").or_else(|| {
        step.get("data")
            .and_then(|data| data.get("exceptionDetails"))
    })?;
    Some(
        details
            .pointer("/exception/description")
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| truncate_text(&details.to_string(), 300)),
    )
}

/// DOM 级点击：`this.click()`，不产生真实鼠标事件。
///
/// 代价是它**无视遮挡**：元素被 cookie 弹窗 / 登录浮层 / 全屏 click-catcher 盖住时，
/// `this.click()` 照样派发到元素本身，而真实用户点下去只会点到遮罩上。以前这里
/// 直接返回 success，调用方完全无从得知（daemon 侧另一半）。
/// 现在顺带做一次命中测试，被遮住时把 blocked / covered_by 带回去，
/// 由 finish_operation 提升成顶层 warnings。
pub(crate) async fn dom_click(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let script = r#"function() {
      this.scrollIntoView({ block: 'center', inline: 'center' });
      const rect = this.getBoundingClientRect();
      const cx = Math.round(rect.x + rect.width / 2);
      const cy = Math.round(rect.y + rect.height / 2);
      let covered = null;
      try {
        const top = document.elementFromPoint(cx, cy);
        if (top && top !== this && !this.contains(top) && !top.contains(this)) {
          covered = (top.tagName || '').toLowerCase() + (top.id ? '#' + top.id : '');
        }
      } catch (_) {}
      this.click();
      const out = { success: true, tag: this.tagName, text: (this.textContent || '').slice(0, 100) };
      if (covered) {
        out.blocked = true;
        out.covered_by = covered;
        out.hint = '目标中心点被 ' + covered + ' 遮住：真实鼠标会点到遮罩上（这里是 DOM 级 click，仍已派发到目标）';
      }
      return out;
    }"#;
    call_function_resolved(state, tab_id, object_id, script, timeout).await
}

pub(crate) async fn dom_fill(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value> {
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let append = payload
        .get("append")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let script = format!(
        r#"function() {{
      const __value = {value_json};
      const __append = {append};
      const el = this;
      el.focus();
      const role = el.getAttribute && (el.getAttribute('role') || '').toLowerCase();
      const tag = (el.tagName || '').toLowerCase();
      const editable = el.isContentEditable || tag === 'input' || tag === 'textarea' || role === 'textbox' || role === 'searchbox';
      if (!editable) return {{ error: 'fill target is not editable' }};
      if (el.isContentEditable) {{
        if (!__append) el.textContent = '';
        const sel = window.getSelection();
        if (sel) {{ const range = document.createRange(); range.selectNodeContents(el); range.collapse(false); sel.removeAllRanges(); sel.addRange(range); }}
        let inserted = false;
        try {{ inserted = document.execCommand('insertText', false, __value); }} catch (_) {{ inserted = false; }}
        if (!inserted) {{ el.textContent = (__append ? el.textContent : '') + __value; el.dispatchEvent(new InputEvent('input', {{ inputType: 'insertText', data: __value, bubbles: true }})); }}
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return {{ success: true, tag: el.tagName, mode: 'contenteditable', value_length: __value.length }};
      }}
      if (tag === 'input' || tag === 'textarea') {{
        const setter = Object.getOwnPropertyDescriptor(tag === 'textarea' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value')?.set;
        const next = __append ? (el.value || '') + __value : __value;
        if (setter) setter.call(el, next); else el.value = next;
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return {{ success: true, tag: el.tagName, mode: 'value', value_length: __value.length }};
      }}
      // role=textbox / role=searchbox 的自定义组件（div + 自管 caret）：只能靠 insertText。
      // 旧实现无论成功与否都 return success:true，内容没写进去调用方也无从得知。
      let inserted = false;
      let insert_error = null;
      try {{ inserted = document.execCommand('insertText', false, __value); }}
      catch (err) {{ insert_error = String((err && err.message) || err); }}
      el.dispatchEvent(new Event('input', {{ bubbles: true }}));
      el.dispatchEvent(new Event('change', {{ bubbles: true }}));
      // 读回校验：组件可能只接受部分字符，也可能把文本渲染进子节点而不是 value。
      const actual = el.value !== undefined ? String(el.value) : (el.textContent || '');
      // append 模式是在已有内容尾部追加，用 endsWith 判定；否则必须完全相等。
      const matched = __append ? actual.endsWith(__value) : actual === __value;
      if (!matched) {{
        // execCommand 的返回值 / 异常一并带回：读回不一致时这两项是唯一线索。
        return {{
          success: false,
          error: 'role_textbox_write_failed',
          mode: 'role-textbox',
          expected_length: __value.length,
          actual_length: actual.length,
          inserted: inserted,
          insert_error: insert_error,
        }};
      }}
      return {{ success: true, tag: el.tagName, mode: 'role-textbox', value_length: __value.length, inserted: inserted }};
    }}"#,
        value_json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()),
        append = append,
    );
    let value = call_function_resolved(state, tab_id, object_id, &script, timeout).await?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        // 读回不一致时把长度差一起带出来：「写失败」和「只写进去一半」是两种
        // 完全不同的排障方向，只回一个 error 码看不出区别。
        let lengths = (
            value.get("expected_length").and_then(Value::as_u64),
            value.get("actual_length").and_then(Value::as_u64),
        );
        if let (Some(expected), Some(actual)) = lengths {
            return Err(anyhow!(
                "{error}: 写入后读回不一致（期望 {expected} 字符，实际 {actual} 字符）"
            ));
        }
        return Err(anyhow!(error.to_string()));
    }
    Ok(value)
}

pub(crate) async fn dom_mouse_click(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    timeout: Duration,
) -> Result<Value> {
    match dom_mouse_click_cdp(state, tab_id, object_id, timeout).await {
        Ok(value) => Ok(value),
        Err(err) => {
            // 页面内嵌了别的扩展的 iframe 时 Chrome 拒绝 attach，Input.* 一条都发不出去。
            // 唯一还能用的通道是页面脚本 —— 合成事件不被站点信任（isTrusted=false），
            // 但比「命令直接失败、调用方无从下手」要好，前提是**如实标注**。
            let text = err.to_string();
            if !(is_page_protected_error(&text) || text.contains("禁止调试器注入")) {
                return Err(err);
            }
            synthetic_mouse_click(state, tab_id, object_id, timeout)
                .await
                .map_err(|fallback| anyhow!("{err}；页面内合成事件回落也失败：{fallback}"))
        }
    }
}

/// CDP 通道的真实鼠标点击（`Input.dispatchMouseEvent`，投递的事件 `isTrusted=true`）。
async fn dom_mouse_click_cdp(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let box_data = if let Some(raw) = object_id.strip_prefix("__backend:") {
        let backend: i64 = raw.parse()?;
        batch_cdp_call(
            state,
            tab_id,
            vec![
                // 真实鼠标事件必须落在前台标签上：以前不发 bringToFront，扩展侧
                // `allowFocus=false` 会静默跳过，鼠标事件打到后台标签仍报成功。
                json!({ "cmd": "cdp", "method": "Page.bringToFront", "allowFocus": true }),
                json!({ "cmd": "cdp", "method": "DOM.resolveNode", "params": { "backendNodeId": backend } }),
                json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$1.object.objectId", "functionDeclaration": "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }", "returnByValue": true } }),
                json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$1.object.objectId", "functionDeclaration": "function() { const r = this.getBoundingClientRect(); const cx = Math.round(r.x + r.width / 2); const cy = Math.round(r.y + r.height / 2); let covered = null; try { const top = document.elementFromPoint(cx, cy); if (top && top !== this && !this.contains(top) && !top.contains(this)) covered = (top.tagName || '').toLowerCase() + (top.id ? '#' + top.id : ''); } catch (_) {} return { x: r.x, y: r.y, width: r.width, height: r.height, covered_by: covered }; }", "returnByValue": true } }),
            ],
            timeout,
        )
        .await?
    } else if let Some(selector) = object_id.strip_prefix("__selector:") {
        let expression = format!(
            "document.querySelector({})",
            serde_json::to_string(selector)?
        );
        batch_cdp_call(
            state,
            tab_id,
            vec![
                json!({ "cmd": "cdp", "method": "Page.bringToFront", "allowFocus": true }),
                json!({ "cmd": "cdp", "method": "Runtime.evaluate", "params": { "expression": expression, "returnByValue": false } }),
                json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$1.result.objectId", "functionDeclaration": "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }", "returnByValue": true } }),
                json!({ "cmd": "cdp", "method": "Runtime.callFunctionOn", "params": { "objectId": "$1.result.objectId", "functionDeclaration": "function() { const r = this.getBoundingClientRect(); const cx = Math.round(r.x + r.width / 2); const cy = Math.round(r.y + r.height / 2); let covered = null; try { const top = document.elementFromPoint(cx, cy); if (top && top !== this && !this.contains(top) && !top.contains(this)) covered = (top.tagName || '').toLowerCase() + (top.id ? '#' + top.id : ''); } catch (_) {} return { x: r.x, y: r.y, width: r.width, height: r.height, covered_by: covered }; }", "returnByValue": true } }),
            ],
            timeout,
        )
        .await?
    } else {
        // （残留）：旧实现是 focus_scroll（一次 WS 往返 + 一次页面执行）
        // 再 DOM.getBoxModel（第二次往返）。两次之间页面可能重排（惰性图片撑开、
        // 吸顶条出现），量到的坐标与滚动落定后的真实位置对不上，真实鼠标就点到
        // 旁边去了。这里把 scrollIntoView + getBoundingClientRect 合并成**单个**
        // Runtime.callFunctionOn：滚动与测量在同一次页面执行里完成，布局漂移窗口消失。
        // 之后仍然只有一次输入派发 —— Input.dispatchMouseEvent 必须走 debugger 通道，
        // 页面脚本里合成的鼠标事件站点不认，所以「一次测量 + 一次派发」已是下限。
        const MEASURE_RECT: &str = "function() { \
            this.scrollIntoView({ block: 'center', inline: 'center' }); \
            const r = this.getBoundingClientRect(); \
            return { x: r.x, y: r.y, width: r.width, height: r.height }; \
        }";
        let measured =
            match call_function_resolved(state, tab_id, object_id, MEASURE_RECT, timeout).await {
                Ok(value) => value,
                Err(err) => return Err(anyhow!("mouse-click: 无法测量元素位置（{err}）")),
            };
        // 与其它两条路径保持同一回包形状（results[3].result.value 就是 rect；
        // 下面的取坐标 / 异常诊断逻辑不需要分叉。
        json!({
            "results": [Value::Null, Value::Null, Value::Null, json!({ "result": { "value": measured } })],
        })
    };
    let rect = box_data
        .get("results")
        .and_then(Value::as_array)
        .and_then(|items| items.get(3))
        .and_then(|data| data.get("result").and_then(|r| r.get("value")).cloned())
        .or_else(|| {
            box_data
                .as_array()
                .and_then(|items| items.get(3))
                .and_then(|data| data.get("result").and_then(|r| r.get("value")).cloned())
        });
    if rect.is_none() {
        // 页面脚本在量位置时抛异常（元素被 React 卸载、getBoundingClientRect 被改写等）：
        // 旧实现一律报 "no layout box"，把「页面报错」误诊成「元素不可见」（同族）。
        if let Some(message) = box_data
            .get("results")
            .and_then(Value::as_array)
            .and_then(|items| items.get(3))
            .and_then(cdp_exception_text)
        {
            return Err(anyhow!("mouse-click: 页面计算元素位置时抛异常：{message}"));
        }
    }
    let (x, y, covered_by) = if let Some(rect) = rect {
        let width = rect
            .get("width")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let height = rect
            .get("height")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        if width <= 0.0 || height <= 0.0 {
            return Err(anyhow!("mouse-click: element has no layout box (display:none / detached / zero-size). Use click for DOM-level fallback."));
        }
        (
            rect.get("x").and_then(Value::as_f64).unwrap_or_default() + width / 2.0,
            rect.get("y").and_then(Value::as_f64).unwrap_or_default() + height / 2.0,
            rect.get("covered_by")
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    } else {
        let content = box_data
            .get("results")
            .and_then(Value::as_array)
            .and_then(|items| items.get(3))
            .and_then(|data| data.get("model").and_then(|m| m.get("content")).cloned())
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| anyhow!("mouse-click: element has no layout box (display:none / detached / zero-size). Use click for DOM-level fallback."))?;
        if content.len() < 8 {
            return Err(anyhow!("mouse-click: element has no layout box (display:none / detached / zero-size). Use click for DOM-level fallback."));
        }
        let n = |idx: usize| content.get(idx).and_then(Value::as_f64).unwrap_or_default();
        (
            (n(0) + n(2) + n(4) + n(6)) / 4.0,
            (n(1) + n(3) + n(5) + n(7)) / 4.0,
            None,
        )
    };
    // 之后的往返结构（三条路径一致）：
    //   第 1 批 = 定位 + scrollIntoView + 量 rect（一条 batch，页面侧一次执行完成）
    //   第 2 批 = Input.dispatchMouseEvent × 3（移动 / 按下 / 抬起）
    // 为什么不是一次往返：扩展的 batch 协议只支持把上一步的 objectId / nodeId
    // 注入后续命令的 `$N` 占位，还不能把页面测出的坐标填进下一步 params；而
    // Input.* 只能走 debugger 通道，页面脚本里合成的事件站点不认。
    // 残留风险：页面 CSS 声明了 `scroll-behavior: smooth` 时 scrollIntoView 是
    // 异步滚动，紧接着量的 rect 可能是滚动中途值。这里用的是默认
    // `behavior:'auto'`（同步），只有页面显式声明 smooth 才会命中；
    // 真遇到该场景应在扩展批协议里加「等滚动稳定」的原语，而不是再拆往返。
    // 三次事件合并成一条 batch：原来每次一个 WS 往返（还各带一次 alarm 抖动）。
    // 同时补 pointerType，避免部分站点依赖 pointer 事件判定真实用户操作。
    // 两批之间可能被别的标签抢走前台（用户切换、扩展自身切页、站点
    // window.focus()），第二批必须再抢一次前台 —— 否则 Input.* 会打到后台标签，
    // 扩展侧标成 skipped 而命令仍可能被当成成功。该批只判成败、不按 index 取值，
    // 在最前面插入 bringToFront 不影响后续逻辑。
    let commands = vec![
        json!({ "cmd": "cdp", "method": "Page.bringToFront", "allowFocus": true }),
        json!({ "cmd": "cdp", "method": "Input.dispatchMouseEvent", "params": { "type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0, "pointerType": "mouse" } }),
        json!({ "cmd": "cdp", "method": "Input.dispatchMouseEvent", "params": { "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1, "pointerType": "mouse" } }),
        json!({ "cmd": "cdp", "method": "Input.dispatchMouseEvent", "params": { "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1, "pointerType": "mouse" } }),
    ];
    batch_cdp_call(state, tab_id, commands, timeout).await?;
    let mut out = json!({ "success": true, "x": x.round(), "y": y.round() });
    // 真实鼠标事件会打到最上层元素：被遮罩盖住时点击落在遮罩上，页面自己的 handler
    // 不会触发 —— 命令却仍报 success。把这件事说清楚，别让调用方以为是点到了。
    if let Some(covered) = covered_by {
        out["blocked"] = json!(true);
        out["covered_by"] = json!(covered);
        out["hint"] = json!(format!(
            "目标中心点被 {covered} 遮住：真实鼠标落到了遮罩上，页面可能没有收到这次点击"
        ));
    }
    Ok(out)
}

/// 页面内合成鼠标事件（CDP 被拒时的唯一可用通道）。
///
/// 只支持 CSS 选择器形态的目标：页面脚本拿不到 CDP 的 objectId/backendNodeId。
/// 事件序列与真实点击一致（pointerdown → mousedown → focus → pointerup → mouseup → click），
/// 并带上 `composed:true`，让 shadow DOM 里的监听器也能收到。
async fn synthetic_mouse_click(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let Some(selector) = object_id.strip_prefix("__selector:") else {
        return Err(anyhow!(
            "该页面禁止调试器注入，而目标引用不是 CSS 选择器；请改用 `--selector` 或 `atb click`（DOM 级点击）"
        ));
    };
    let selector_json = serde_json::to_string(selector)?;
    let script = format!(
        r#"return (function() {{
  var el = document.querySelector({selector_json});
  if (!el) return {{ ok: false, error: 'target_not_found' }};
  try {{ el.scrollIntoView({{ block: 'center', inline: 'center' }}); }} catch (_) {{}}
  var r = el.getBoundingClientRect();
  if (!(r.width > 0) || !(r.height > 0)) return {{ ok: false, error: 'no_layout_box' }};
  var x = r.left + r.width / 2, y = r.top + r.height / 2;
  var base = {{ bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, view: window, button: 0 }};
  function fire(name, extra, Ctor) {{
    var init = Object.assign({{}}, base, extra || {{}});
    try {{ el.dispatchEvent(new (Ctor || MouseEvent)(name, init)); }} catch (_) {{}}
  }}
  fire('pointerover', {{ buttons: 0 }}, PointerEvent);
  fire('pointerenter', {{ buttons: 0 }}, PointerEvent);
  fire('pointerdown', {{ buttons: 1, pointerType: 'mouse', isPrimary: true }}, PointerEvent);
  fire('mousedown', {{ buttons: 1, detail: 1 }});
  if (typeof el.focus === 'function') {{ try {{ el.focus({{ preventScroll: true }}); }} catch (_) {{}} }}
  fire('pointerup', {{ buttons: 0, pointerType: 'mouse', isPrimary: true }}, PointerEvent);
  fire('mouseup', {{ buttons: 0, detail: 1 }});
  fire('click', {{ buttons: 0, detail: 1 }});
  return {{ ok: true, tag: el.tagName, x: x, y: y }};
}})()"#
    );
    let response = execute_raw_js_on_tab(state, &script, tab_id, timeout).await?;
    let value = response.data.unwrap_or(Value::Null);
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let reason = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("synthetic_click_failed");
        return Err(anyhow!("{reason}"));
    }
    Ok(json!({
        "success": true,
        "x": value.get("x").cloned().unwrap_or(Value::Null),
        "y": value.get("y").cloned().unwrap_or(Value::Null),
        "synthetic": true,
        "trusted": false,
        "tag": value.get("tag").cloned().unwrap_or(Value::Null),
        "hint": "该页面禁止调试器注入，本次为页面内合成事件（isTrusted=false）；需要真实输入事件的站点可能不响应，可临时关闭注入 iframe 的扩展后重试",
    }))
}

pub(crate) async fn focus_object(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    timeout: Duration,
) -> Result<()> {
    // 老脚本恒 `return true`，返回值被直接丢弃：对不可聚焦元素 `focus()` 是 no-op，
    // 调用方却以为焦点已经进去了，随后 `dispatch_keys` 把按键打进 body。
    // 这里把落点算出来带回去，由调用方决定失败。
    // `getRootNode()` 兼顾 shadow DOM：document.activeElement 在那种情况下是宿主元素。
    let script = "function() { \
        if (this.focus) { try { this.focus({ preventScroll: true }); } catch (_) { this.focus(); } } \
        const root = (this.getRootNode && this.getRootNode()) || document; \
        const active = root.activeElement === this || document.activeElement === this; \
        return { ok: true, active }; \
    }";
    let value = call_function_resolved(state, tab_id, object_id, script, timeout).await?;
    if value.get("active").and_then(Value::as_bool) == Some(false) {
        return Err(anyhow!(
            "目标元素无法获得焦点（CDP 路径）：它是不可聚焦的容器元素，或页面拦截了 focus。\
             按键没有发送——否则会打进当前实际聚焦的元素"
        ));
    }
    Ok(())
}

pub(crate) async fn call_function_value(
    state: &AppState,
    tab_id: &str,
    object_id: &str,
    function_declaration: &str,
    timeout: Duration,
) -> Result<Value> {
    let result = cdp_call(
        state,
        tab_id,
        "Runtime.callFunctionOn",
        json!({ "objectId": object_id, "functionDeclaration": function_declaration, "returnByValue": true }),
        timeout,
    )
    .await?;
    if !result.ok {
        return Err(anyhow!(result
            .error
            .unwrap_or_else(|| "Runtime.callFunctionOn failed".to_string())));
    }
    let data = result.data.unwrap_or(Value::Null);
    // 与 call_function_resolved 同一判据：异常在 exceptionDetails 里，不在协议错误里。
    if let Some(message) = cdp_exception_text(&data) {
        return Err(anyhow!(message));
    }
    Ok(data
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

pub(crate) async fn dispatch_keys(
    state: &AppState,
    tab_id: &str,
    keys: &str,
    timeout: Duration,
) -> Result<Value> {
    let platform = if cfg!(target_os = "macos") {
        "mac"
    } else {
        "other"
    };
    let segments: Vec<&str> = keys.split_whitespace().collect();
    if segments.is_empty() {
        return Err(anyhow!("send-keys: keys is required"));
    }
    // 先**整体**解析、再逐个发送。`parse_key_segment` 是纯函数，没有副作用：
    // 旧实现边发边解析，靠后的键名写错时前几个键已经打出去了，既不能回滚、
    // 错误里也不说「已经发了几段」，调用方重试就变成双打。
    let parsed: Vec<KeySegment> = segments
        .iter()
        .map(|segment| parse_key_segment(segment, platform))
        .collect::<Result<Vec<_>>>()?;
    let total = parsed.len();
    let mut dispatched = 0;
    for key in parsed {
        dispatch_key_segment(state, tab_id, key, timeout)
            .await
            .map_err(|err| {
                anyhow!(
                    "send-keys: 已发送 {dispatched}/{total} 段后失败（前面的按键无法回滚，\
                     重试前请确认页面状态）：{err}"
                )
            })?;
        dispatched += 1;
    }
    Ok(json!({ "success": true, "dispatched": dispatched }))
}

#[derive(Debug, Clone)]
pub(crate) struct KeySegment {
    pub(crate) modifiers: Vec<KeySpec>,
    pub(crate) modifier_bits: i64,
    pub(crate) key: KeySpec,
}

#[derive(Debug, Clone)]
pub(crate) struct KeySpec {
    pub(crate) bit: i64,
    pub(crate) key: String,
    pub(crate) code: String,
    pub(crate) vkc: i64,
    pub(crate) text: Option<String>,
}

pub(crate) fn parse_key_segment(segment: &str, platform: &str) -> Result<KeySegment> {
    let parts: Vec<&str> = segment
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(anyhow!("send-keys: empty segment"));
    }
    let mut modifier_bits = 0;
    let mut modifiers = Vec::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let spec = modifier_spec(part, platform)
            .ok_or_else(|| anyhow!("send-keys: {part} is not a modifier"))?;
        modifier_bits |= spec.bit;
        modifiers.push(spec);
    }
    let mut key = key_spec(parts[parts.len() - 1])?;
    if modifier_bits & 8 != 0
        && key.key.len() == 1
        && key.key.chars().all(|c| c.is_ascii_lowercase())
    {
        key.key = key.key.to_uppercase();
        key.text = key.text.as_ref().map(|_| key.key.clone());
    }
    Ok(KeySegment {
        modifiers,
        modifier_bits,
        key,
    })
}

pub(crate) fn modifier_spec(input: &str, platform: &str) -> Option<KeySpec> {
    let name = input.to_ascii_lowercase();
    let (bit, key, code, vkc) = match name.as_str() {
        "alt" => (1, "Alt", "AltLeft", 18),
        "ctrl" | "control" => (2, "Control", "ControlLeft", 17),
        "cmd" | "meta" => (4, "Meta", "MetaLeft", 91),
        "shift" => (8, "Shift", "ShiftLeft", 16),
        "mod" if platform == "mac" => (4, "Meta", "MetaLeft", 91),
        "mod" => (2, "Control", "ControlLeft", 17),
        _ => return None,
    };
    Some(KeySpec {
        bit,
        key: key.to_string(),
        code: code.to_string(),
        vkc,
        text: None,
    })
}

pub(crate) fn key_spec(input: &str) -> Result<KeySpec> {
    let name = input.to_ascii_lowercase();
    let (key, code, vkc, text) = match name.as_str() {
        "enter" | "return" => (
            "Enter".to_string(),
            "Enter".to_string(),
            13,
            Some("\r".to_string()),
        ),
        "escape" | "esc" => ("Escape".to_string(), "Escape".to_string(), 27, None),
        "tab" => ("Tab".to_string(), "Tab".to_string(), 9, None),
        "backspace" => ("Backspace".to_string(), "Backspace".to_string(), 8, None),
        "delete" => ("Delete".to_string(), "Delete".to_string(), 46, None),
        "space" => (
            " ".to_string(),
            "Space".to_string(),
            32,
            Some(" ".to_string()),
        ),
        "arrowup" | "up" => ("ArrowUp".to_string(), "ArrowUp".to_string(), 38, None),
        "arrowdown" | "down" => ("ArrowDown".to_string(), "ArrowDown".to_string(), 40, None),
        "arrowleft" | "left" => ("ArrowLeft".to_string(), "ArrowLeft".to_string(), 37, None),
        "arrowright" | "right" => ("ArrowRight".to_string(), "ArrowRight".to_string(), 39, None),
        "home" => ("Home".to_string(), "Home".to_string(), 36, None),
        "end" => ("End".to_string(), "End".to_string(), 35, None),
        _ if input.len() == 1 && input.chars().all(|c| c.is_ascii_alphabetic()) => {
            // 单字符字母必须按用户给的大小写原样发送：旧实现 key/text 都取 lower，
            // 只有 code 用 upper，于是 `send-keys "A"` 实际输入的是小写 `a`。
            // code 仍然保持 `KeyA` 形式（KeyboardEvent.code 不区分大小写）。
            let upper = input.to_ascii_uppercase();
            (
                input.to_string(),
                format!("Key{upper}"),
                upper.as_bytes()[0] as i64,
                Some(input.to_string()),
            )
        }
        _ if input.len() == 1 && input.chars().all(|c| c.is_ascii_digit()) => (
            input.to_string(),
            format!("Digit{input}"),
            input.as_bytes()[0] as i64,
            Some(input.to_string()),
        ),
        _ => return Err(anyhow!("send-keys: unknown key {input}")),
    };
    Ok(KeySpec {
        bit: 0,
        key,
        code,
        vkc,
        text,
    })
}

pub(crate) async fn dispatch_key_segment(
    state: &AppState,
    tab_id: &str,
    segment: KeySegment,
    timeout: Duration,
) -> Result<()> {
    let KeySegment {
        modifiers,
        modifier_bits,
        key,
    } = segment;
    // 修饰键的 keyUp 必须无条件执行：CDP 的按键状态是浏览器级的，漏抬一次
    // Ctrl/Shift/Meta，此后该标签的所有操作都等同按住修饰键。
    // 所以「按下」逐条记录，「抬起」统一收尾，中间任何一步失败都不跳过释放。
    let mut active_bits = 0;
    let mut pressed: Vec<KeySpec> = Vec::with_capacity(modifiers.len());
    let mut failure: Option<anyhow::Error> = None;

    for modifier in &modifiers {
        active_bits |= modifier.bit;
        match dispatch_key_event(
            state,
            tab_id,
            "keyDown",
            active_bits,
            modifier,
            None,
            timeout,
        )
        .await
        {
            Ok(()) => pressed.push(modifier.clone()),
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }

    if failure.is_none() {
        let text = if modifier_bits & !8 == 0 {
            key.text.as_deref()
        } else {
            None
        };
        match dispatch_key_event(state, tab_id, "keyDown", modifier_bits, &key, text, timeout).await
        {
            Ok(()) => {
                if let Err(err) =
                    dispatch_key_event(state, tab_id, "keyUp", modifier_bits, &key, None, timeout)
                        .await
                {
                    failure = Some(err);
                }
            }
            Err(err) => failure = Some(err),
        }
    }

    // 收尾：按相反顺序抬起所有已经按下的修饰键，best-effort，不覆盖首个错误。
    let mut release_failed = false;
    for modifier in pressed.iter().rev() {
        active_bits &= !modifier.bit;
        if dispatch_key_event(state, tab_id, "keyUp", active_bits, modifier, None, timeout)
            .await
            .is_err()
        {
            release_failed = true;
        }
    }

    match failure {
        None => Ok(()),
        Some(err) if release_failed => Err(anyhow!(
            "{err}（已尝试释放修饰键，但释放失败，修饰键状态可能残留）"
        )),
        Some(err) => Err(anyhow!("{err}（已释放已按下的修饰键）")),
    }
}

pub(crate) async fn dispatch_key_event(
    state: &AppState,
    tab_id: &str,
    event_type: &str,
    modifiers: i64,
    spec: &KeySpec,
    text: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let mut params = json!({ "type": event_type, "modifiers": modifiers, "key": spec.key, "code": spec.code, "windowsVirtualKeyCode": spec.vkc });
    if let Some(text) = text {
        params["text"] = json!(text);
    }
    // 与挑战点击同一条判据 —— 扩展回的 `ok:false` 是失败，不是「已派发」。
    let result = cdp_call(state, tab_id, "Input.dispatchKeyEvent", params, timeout).await?;
    result.into_ok("Input.dispatchKeyEvent")?;
    Ok(())
}

/// 只剥掉**成对**的外层引号（长度 ≥ 2 且首尾同类）。
///
/// `trim_matches('"')` 会把内容两端的引号全部吃掉：`'admin' === x` 这类合法内容
/// 会变成 `admin' === x`，下游 eval 直接 SyntaxError（旧报告 #3）。sessions.rs /
/// launch.rs 里的同族点位由其他改动负责，这里只收 cdp / page_data 这一侧。
pub(crate) fn strip_outer_quotes(input: &str) -> &str {
    let trimmed = input.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        return &trimmed[1..trimmed.len() - 1];
    }
    trimmed
}

pub(crate) fn ax_string(value: &Option<AxValue>) -> Option<String> {
    value.as_ref().and_then(|v| {
        let value = v.value.as_ref()?;
        let is_string_like = matches!(
            v.value_type.as_deref(),
            Some("string")
                | Some("computedString")
                | Some("token")
                | Some("internalRole")
                | Some("role")
        );
        Some(if is_string_like {
            value.as_str().unwrap_or_default().to_string()
        } else if let Some(s) = value.as_str() {
            s.to_string()
        } else if let Some(n) = value.as_i64() {
            n.to_string()
        } else if let Some(n) = value.as_u64() {
            n.to_string()
        } else if let Some(n) = value.as_f64() {
            n.to_string()
        } else if let Some(b) = value.as_bool() {
            b.to_string()
        } else {
            strip_outer_quotes(&value.to_string()).to_string()
        })
    })
}

pub(crate) fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

pub(crate) fn truncate_text(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!("{}...", value.chars().take(max).collect::<String>())
    }
}

/// CDP 快照（AX 树）侧的「可操作角色」判定集。
///
/// 与 JS 快照（`assets/page_snapshot.js` 的 `OPERABLE` 集合）必须对齐，否则同一
/// 页面两种模式下 `@e` 集合与编号不同。这里取**并集**：JS 侧的 `file` 已并入，
/// 而 AX 树特有的 `listbox` / `menuitem*` / `option` / `spinbutton` / `treeitem`
/// 不能删——原生 `<select>` 的选项、菜单项在 AX 树里就是这些角色，删掉会让 CDP
/// 快照丢引用、`select` / 菜单操作失去目标。JS 侧脚本资产不在本轮修改范围，
/// 其 OPERABLE 缺失的角色只影响 `interactiveOnly` 过滤；默认模式下 JS 快照按
/// 「可见 + 非 generic + 有名字」取元素，本就是 CDP 可操作集的超集。
pub(crate) fn is_operable_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "option"
            | "slider"
            | "spinbutton"
            | "switch"
            | "tab"
            | "treeitem"
            // JS 快照 OPERABLE 里有 file（`<input type=file>`），CDP 侧补齐，
            // 保证同一文件上传控件在两种模式下都能拿到 @e 引用。
            | "file"
    )
}

pub(crate) fn role_is_context(role: &str) -> bool {
    matches!(
        role,
        "form" | "dialog" | "alert" | "heading" | "main" | "navigation" | "region"
    )
}

/// `document.querySelector` 没匹配到元素时，第 1 步拿到的 `objectId` 是 null，
/// CDP 只会回一句 "Either objectId or executionContextId or uniqueContextId must
/// be specified"——用户完全看不出是「选择器没匹配到」。这里翻译成人话。
fn selector_missing_target(err: anyhow::Error, selector: &str) -> anyhow::Error {
    let text = err.to_string();
    if text.contains("objectId") && text.contains("must be specified") {
        return anyhow!(
            "target not found: 选择器 {selector} 没有匹配到元素（页面可能还没加载完、元素已被移除，或在 iframe 里）"
        );
    }
    err
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    /// 旧报告 #3：只剥成对的外层引号，内容里的引号必须原样保留。
    #[test]
    fn strip_outer_quotes_only_removes_a_matching_pair() {
        assert_eq!(strip_outer_quotes("\"abc\""), "abc");
        assert_eq!(strip_outer_quotes("'abc'"), "abc");
        assert_eq!(strip_outer_quotes("'admin' === x"), "'admin' === x");
        assert_eq!(strip_outer_quotes("\""), "\"");
        assert_eq!(strip_outer_quotes("\"a'"), "\"a'");
    }

    /// 单字符字母按原样（大写）发送，`code` 仍是 `KeyX`。
    #[test]
    fn single_letter_keeps_its_case() {
        let upper = key_spec("A").expect("A");
        assert_eq!(upper.key, "A");
        assert_eq!(upper.code, "KeyA");
        assert_eq!(upper.text.as_deref(), Some("A"));

        let lower = key_spec("a").expect("a");
        assert_eq!(lower.key, "a");
        assert_eq!(lower.code, "KeyA");
        assert_eq!(lower.text.as_deref(), Some("a"));
    }
}
