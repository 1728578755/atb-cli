use super::*;

/// 页面内容是否被 `page_data::truncate_output`（`html::smart_truncate`）截断。
///
/// `get_html` 的三条降级路径与主路径现在都统一走 `truncate_output`，截断时会在末尾
/// 追加 `" [TRUNCATED]"`。这里以**标记**为口径，不用字节数去猜：
/// `String::len()` 是字节、预算却是字符数，中文页会提前/滞后判定。
pub(crate) fn content_is_truncated(content: &str) -> bool {
    content.trim_end().ends_with(" [TRUNCATED]")
}

pub(crate) async fn scan_page(
    state: &AppState,
    tabs_only: bool,
    selector: SessionSelector,
    text_only: bool,
    fast: bool,
    max_nodes: Option<usize>,
    timeout: Duration,
) -> Result<Value> {
    let tabs = active_tabs(state, true).await;
    if tabs.is_empty() {
        return Err(anyhow!(
            "no_active_tabs: 没有可用的浏览器标签页，请先在 Chrome 打开一个 http/https 页面"
        ));
    }
    // 显式拿到 session_key 后再执行脚本，避免依赖全局默认路由（job 并发时会串台）。
    let session_key = select_tab(state, selector).await?;
    let mut result = json!({
        "status": "success",
        "metadata": {
            "tabs_count": tabs.len(),
            "tabs": tabs,
            "active_tab": session_key,
        }
    });
    if !tabs_only {
        let timeout_error = || {
            anyhow!(
                "scan_timeout: 页面内容抓取超过 {}s，可增大 --timeout",
                timeout.as_secs()
            )
        };
        if fast {
            // 快速模式：不克隆 DOM、不算 rect/style，只做一次带预算的文本抽取。
            // 十万级节点的页面上这是「秒级」与「几十毫秒」的差别。
            let opts = json!({
                "maxChars": 35000,
                "forms": false,
                "links": false,
                "maxNodes": max_nodes.unwrap_or(80_000),
                "budgetMs": 1500,
            });
            let script = format!("{}\nreturn __abcRead({});", html::js_page_read(), opts);
            let payload = tokio::time::timeout(
                timeout,
                execute_page_js(
                    state,
                    &script,
                    SessionSelector::new(Some(session_key.clone()), None, None),
                    true,
                    timeout,
                ),
            )
            .await
            .map_err(|_| timeout_error())??;
            let page = page_payload(payload, "page");
            let page = page.get("page").cloned().unwrap_or(Value::Null);
            let text = page
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // 快通道由页面侧 `page_read` 按 maxChars 截断并自带 `truncated` 标记；
            // 保险起见再认一次末尾标记，两条口径取并集。
            let truncated = page.get("truncated").and_then(Value::as_bool) == Some(true)
                || content_is_truncated(&text);
            result["content"] = json!(text);
            result["metadata"]["mode"] = json!("fast");
            result["metadata"]["fast"] = page.get("stats").cloned().unwrap_or(Value::Null);
            result["metadata"]["truncated"] = json!(truncated);
        } else {
            let node_budget = max_nodes.unwrap_or(8_000);
            let content = tokio::time::timeout(
                timeout,
                get_html(
                    state,
                    &session_key,
                    true,
                    35000,
                    text_only,
                    timeout,
                    node_budget,
                ),
            )
            .await
            .map_err(|_| timeout_error())??;
            // `get_html` 现在所有返回点都会按 maxchars 截断并追加标记，
            // 这里按标记回报真实值（旧实现固定 false / 干脆没有这个字段）。
            let truncated = content_is_truncated(&content);
            result["content"] = Value::String(content);
            result["metadata"]["mode"] = json!(if text_only { "text" } else { "html" });
            result["metadata"]["node_budget"] = json!(node_budget);
            result["metadata"]["truncated"] = json!(truncated);
        }
    }
    Ok(result)
}

/// 单次观察保留的样本条数上限。
///
/// 计数是无界的（`changed` / `added` …），但**样本数组**必须有上限：旧实现把每条
/// `MutationRecord` 原样 push 进数组，长观察（几十分钟的 SPA）会一直涨到标签页 OOM。
/// 值域 `1..=500`，默认 50；超出的条数只进 `overflow` 计数。
fn monitor_sample_limit() -> usize {
    crate::settings::monitor_samples()
}

/// 把用户脚本包进「MutationObserver 记录 + 执行」的一次注入里。
///
/// 旧的 monitor 做法是执行前后各 `getHtml` 一次再比较字符串——大页面上光是
/// 两次序列化就要几十到上百毫秒。这里改成页面内观察，只回传计数和少量样本。
pub(crate) fn wrap_with_mutation_watch(script: &str) -> String {
    let source = serde_json::to_string(script).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(async () => {{
  const __abcSource = {source};
  // 只保留计数与少量样本：回调里直接累加，绝不把 MutationRecord 原样
  // 存进数组——长观察会把标签页内存吃干净。samples 只留前 N 条用于诊断，
  // 其余条数进 overflow，保证输出口径不丢。
  const __abcSampleLimit = {sample_limit};
  const __abcWatch = {{ changed: 0, added: 0, removed: 0, attributes: 0, text: 0, samples: [], overflow: 0 }};
  // 回调必须自己累积：脚本里的 await 会让微任务先跑，
  // 回调消费掉记录后 takeRecords() 就只剩空数组。
  const __abcConsume = function (list) {{
    for (const __abcRecord of list) {{
      __abcWatch.changed += 1;
      if (__abcRecord.type === 'childList') {{
        __abcWatch.added += __abcRecord.addedNodes.length;
        __abcWatch.removed += __abcRecord.removedNodes.length;
      }} else if (__abcRecord.type === 'attributes') {{
        __abcWatch.attributes += 1;
      }} else if (__abcRecord.type === 'characterData') {{
        __abcWatch.text += 1;
      }}
      if (__abcWatch.samples.length < __abcSampleLimit) {{
        const __abcTarget = __abcRecord.target && __abcRecord.target.nodeType === 1
          ? (__abcRecord.target.tagName.toLowerCase() + (__abcRecord.target.id ? '#' + __abcRecord.target.id : ''))
          : String((__abcRecord.target && __abcRecord.target.nodeName) || '').toLowerCase();
        const __abcAdded = __abcRecord.addedNodes && __abcRecord.addedNodes[0];
        const __abcDetail = __abcAdded
          ? ('+' + (__abcAdded.nodeType === 1 ? __abcAdded.tagName.toLowerCase() : '#text'))
          : (__abcRecord.attributeName || '');
        __abcWatch.samples.push({{ type: __abcRecord.type, target: __abcTarget, detail: __abcDetail }});
      }} else {{
        __abcWatch.overflow += 1;
      }}
    }}
  }};
  let __abcObserver = null;
  try {{
    __abcObserver = new MutationObserver(__abcConsume);
    __abcObserver.observe(document.documentElement || document, {{
      childList: true, subtree: true, attributes: true, characterData: true
    }});
  }} catch (_) {{ __abcObserver = null; }}
  let __abcResult;
  let __abcError = null;
  try {{
    const __abcAsync = Object.getPrototypeOf(async function () {{}}).constructor;
    const trimmed = String(__abcSource || '').trim();
    // 先编译、后执行：通道选择只看编译结果，不看运行期异常。
    // 旧实现「eval 抛 SyntaxError 且消息含 return/await 就 AsyncFunction 重跑」会把整段脚本
    // 执行两次（POST、点击等副作用翻倍），用户自己 throw 的 SyntaxError 也会触发。
    let __abcCompilesAsPlain = !/^return\b/.test(trimmed);
    if (__abcCompilesAsPlain) {{
      try {{ new Function(trimmed); }} catch (__abcCompileError) {{ __abcCompilesAsPlain = false; }}
    }}
    if (!__abcCompilesAsPlain) {{
      __abcResult = await (new __abcAsync(trimmed))();
    }} else {{
      try {{
        // ：用户代码**直接** eval，不加任何前缀。
        // 旧实现 `eval('__abcStarted = true;\n' + trimmed)` 让首行 `"use strict"` 不再是
        // 指令序言（整段退化成非严格模式），行号也整体 +1；monitor 这条路径当时漏改了。
        const __abcValue = eval(trimmed);
        __abcResult = __abcValue instanceof Promise ? await __abcValue : __abcValue;
      }} catch (__abcError) {{
        if (__abcError instanceof SyntaxError) {{
          // 走到这里时 `new Function(trimmed)` 已经通过，正常只可能是用户代码**运行时**
          // 自己抛 SyntaxError（不能再跑第二次）。只有「编译期非法」才允许换
          // AsyncFunction 重跑；判定方式与 page_data.rs 同口径：把同一段代码包进
          // 恒假的 if 块再 eval 一次——只编译、不执行。
          let __abcEvalLegal = true;
          try {{ eval('if (false) {{' + trimmed + '\n}}'); }} catch (_) {{ __abcEvalLegal = false; }}
          if (!__abcEvalLegal) {{
            __abcResult = await (new __abcAsync(trimmed))();
          }} else {{
            throw __abcError;
          }}
        }} else {{
          throw __abcError;
        }}
      }}
    }}
  }} catch (e) {{
    __abcError = (e && e.message) ? e.message : String(e);
  }} finally {{
    if (__abcObserver) {{
      __abcConsume(__abcObserver.takeRecords());
      __abcObserver.disconnect();
    }}
  }}
  if (__abcError) return {{ __abc_watch: __abcWatch, error: __abcError }};
  return {{ __abc_watch: __abcWatch, result: __abcResult }};
}})()"#,
        sample_limit = monitor_sample_limit()
    )
}

/// 拆掉 `wrap_with_mutation_watch` 的外壳：变更计数进 `change`，页面返回值回到 `js_return`。
pub(crate) fn unwrap_mutation_watch(result: &mut Value) {
    let Some(payload) = result.get("js_return").cloned() else {
        return;
    };
    let Some(watch) = payload.get("__abc_watch").cloned() else {
        return;
    };
    result["change"] = watch;
    result["js_return"] = payload.get("result").cloned().unwrap_or(Value::Null);
    if let Some(error) = payload.get("error") {
        result["error"] = error.clone();
        // 显式标记：这是**包装层捕获的脚本异常**，不是用户脚本正常返回里的业务字段。
        // 调用方据此把 `exec --monitor` 判成失败（否则 throw 也会 ok:true + 退出码 0）。
        result["script_error"] = json!(true);
    }
}

/// 两段式变更观察的第一段：只装 observer，立刻返回（不做任何序列化）。
///
/// `click/fill --monitor` 以前是「动作前后各 `get_html(9_999_999)` 一次 → 比字符串」，
/// 大页面上两次整页序列化就是几十到上百毫秒，而且回传的 `top_change` 是前 2000 字
/// 原始 HTML，没有任何消费者。改成动作前装 observer、动作后收计数，
/// 输出与 `js --monitor` 的 `change` 完全同构。
///
/// `monitor_id` 是每次观察的令牌：第二段据此判断「我这次观察还在不在」，
/// 从而把「导航丢了观察器」和「被后来的观察顶掉」区分开。
pub(crate) fn monitor_start_script(monitor_id: &str) -> String {
    format!(
        r#"const KEY = '__abcMonitor__';
const ID = {monitor_id};
const prev = window[KEY];
if (prev && prev.observer) {{ try {{ prev.observer.disconnect(); }} catch (_) {{}} }}
// 计数在回调里累加、样本只留前 N 条：不保留任何 MutationRecord，
// 观察多久都只占 O(1) 内存。
const LIMIT = {sample_limit};
const state = {{ changed: 0, added: 0, removed: 0, attributes: 0, text: 0, samples: [], overflow: 0 }};
const consume = function (list) {{
  for (const record of list) {{
    state.changed += 1;
    if (record.type === 'childList') {{
      state.added += record.addedNodes.length;
      state.removed += record.removedNodes.length;
    }} else if (record.type === 'attributes') {{
      state.attributes += 1;
    }} else if (record.type === 'characterData') {{
      state.text += 1;
    }}
    if (state.samples.length < LIMIT) {{
      const target = record.target && record.target.nodeType === 1
        ? (record.target.tagName.toLowerCase() + (record.target.id ? '#' + record.target.id : ''))
        : String((record.target && record.target.nodeName) || '').toLowerCase();
      const added = record.addedNodes && record.addedNodes[0];
      const detail = added
        ? ('+' + (added.nodeType === 1 ? added.tagName.toLowerCase() : '#text'))
        : (record.attributeName || '');
      state.samples.push({{ type: record.type, target: target, detail: detail }});
    }} else {{
      state.overflow += 1;
    }}
  }}
}};
let observer = null;
try {{
  observer = new MutationObserver(consume);
  observer.observe(document.documentElement || document, {{
    childList: true, subtree: true, attributes: true, characterData: true
  }});
}} catch (_) {{ observer = null; }}
if (!observer) return {{ status: 'unsupported' }};
window[KEY] = {{ id: ID, observer: observer, state: state, consume: consume }};
return {{ status: 'watching' }};"#,
        monitor_id = serde_json::to_string(monitor_id).unwrap_or_else(|_| "\"\"".to_string()),
        sample_limit = monitor_sample_limit()
    )
}

/// 第二段：取走记录、断开 observer，返回与 `wrap_with_mutation_watch` 相同的计数结构。
///
/// - `status: ok` —— 正常收到计数；
/// - `status: navigated` —— 观察器不见了（最常见是动作触发了导航，文档已换）；
/// - `status: superseded` —— 同一个标签上又开了新的观察，本次被顶掉（不动对方的观察器）。
pub(crate) fn monitor_stop_script(monitor_id: &str) -> String {
    format!(
        r#"const KEY = '__abcMonitor__';
const ID = {monitor_id};
const entry = window[KEY];
if (!entry) return {{ status: 'navigated' }};
if (entry.id !== ID) return {{ status: 'superseded' }};
window[KEY] = null;
const state = entry.state || {{ changed: 0, added: 0, removed: 0, attributes: 0, text: 0, samples: [], overflow: 0 }};
if (entry.observer) {{
  // 还没跑回调的尾部记录也要计入。
  if (typeof entry.consume === 'function') {{
    try {{ entry.consume(entry.observer.takeRecords()); }} catch (_) {{}}
  }}
  entry.observer.disconnect();
}}
return {{ status: 'ok', watch: state }};"#,
        monitor_id = serde_json::to_string(monitor_id).unwrap_or_else(|_| "\"\"".to_string())
    )
}

/// 一次 `--monitor` 动作的观察状态，随 `finish_operation` 一路传下去。
pub(crate) enum MonitorState {
    /// 没开 `--monitor`。
    Off,
    /// 观察已装好，带着本次观察的令牌。
    Armed(String),
    /// 开了 `--monitor` 但装不上（CSP / 页面环境不支持）——要显式回给调用方，不能静默。
    Unsupported,
}

/// 装观察器；`None` 表示页面环境装不上（CSP 拦下 eval / MutationObserver 不可用）。
pub(crate) async fn start_mutation_watch(state: &AppState, session_key: &str) -> Option<String> {
    let monitor_id = Uuid::new_v4().simple().to_string();
    let script = monitor_start_script(&monitor_id);
    let execution =
        execute_raw_js_with_session(state, &script, Duration::from_secs(10), Some(session_key))
            .await
            .ok()?;
    let status = execution
        .result
        .data
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str);
    (status == Some("watching")).then_some(monitor_id)
}

pub(crate) enum MonitorOutcome {
    Collected(Value),
    Navigated,
    Superseded,
}

pub(crate) async fn stop_mutation_watch(
    state: &AppState,
    session_key: &str,
    monitor_id: &str,
) -> Result<MonitorOutcome> {
    let script = monitor_stop_script(monitor_id);
    let execution =
        execute_raw_js_with_session(state, &script, Duration::from_secs(10), Some(session_key))
            .await?;
    let data = execution.result.data.unwrap_or(Value::Null);
    match data.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(MonitorOutcome::Collected(
            data.get("watch").cloned().unwrap_or(Value::Null),
        )),
        Some("superseded") => Ok(MonitorOutcome::Superseded),
        // 注入本身成功但 KEY 不见了：文档被换掉（导航）。
        _ => Ok(MonitorOutcome::Navigated),
    }
}
