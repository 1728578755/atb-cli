use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// 参数类失败的标记类型。
///
/// `main` 只能拿到 `anyhow::Error`，无法区分「命令写错了」与「执行失败」。
/// 把参数错误包成这个类型，`main` 用 `downcast_ref` 就能给出正确的退出码 2，
/// 而不是让 `Err` 一路冒到 `main -> Result` 变成 stderr 的纯文本 `Error: ...`。
#[derive(Debug)]
pub(crate) struct UsageFailure {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) hint: String,
    /// 「你是不是想用这条命令」：只在参数写错、且存在明确替代命令时才给
    /// （`atb download list` 这类把子命令名当位置参数的误用）。`None` 表示没有。
    pub(crate) suggestion: Option<String>,
}

impl std::fmt::Display for UsageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for UsageFailure {}

/// 构造一个「命令写错了」的错误：退出码 2 + 结构化 JSON。
pub(crate) fn usage_failure(
    code: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> anyhow::Error {
    anyhow::Error::new(UsageFailure {
        code: code.to_string(),
        message: message.into(),
        hint: hint.into(),
        suggestion: None,
    })
}

/// 同 [`usage_failure`]，额外带上 `did_you_mean`（正确的命令写法）。
///
/// 有些误用是「把子命令名当成了位置参数」（`atb download list`），只给文案和 hint
/// 调用方仍要自己找正确命令；把替代命令结构化地放在 `did_you_mean` 里，
// 预检信息就能直接被复用。
pub(crate) fn usage_failure_with_suggestion(
    code: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
    suggestion: impl Into<String>,
) -> anyhow::Error {
    anyhow::Error::new(UsageFailure {
        code: code.to_string(),
        message: message.into(),
        hint: hint.into(),
        suggestion: Some(suggestion.into()),
    })
}

/// 参数类错误的简写：`usage_err!("--list 不能和 ... 同时使用")`。
///
/// 语义等价于 `usage_failure("invalid_argument", format!(...), "")`，
/// 用于替换散落在各命令里的裸 `anyhow!`（那些会退化成退出码 1 + 纯文本）。
macro_rules! usage_err {
    ($($arg:tt)*) => {
        $crate::cli::usage_failure("invalid_argument", format!($($arg)*), "")
    };
}
pub(crate) use usage_err;

/// `--async` 只在部分命令上实现。以前其余命令**静默忽略**它（照常同步执行、也不返回
/// job_id），同一个 flag 在不同命令上三种行为并存（CLI 一致性）。
/// 现在统一：不支持的命令给出结构化错误 + 退出码 2。
pub(crate) fn reject_async_if_set(command: &str, is_async: bool) -> Result<()> {
    if !is_async {
        return Ok(());
    }
    Err(usage_failure(
        "unsupported_flag",
        format!("{command} 不支持 --async"),
        "去掉 --async；需要后台执行请用支持它的命令（scan/exec/read/find/snapshot/click/fill/mouse-click/send-keys/upload/screenshot/save-pdf/inject/frames/wait）",
    ))
}

/// 把任意输出写到 stdout，忽略「下游已关闭管道」。
///
/// `println!` 在 `atb tabs | head -1` 这种管道里会因为 EPIPE **panic**（退出码 101），
/// 而 101 不在对外契约里。
pub(crate) fn write_stdout(text: &str) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{text}");
    let _ = lock.flush();
}

/// `main` 的统一失败出口：结构化 JSON + 稳定退出码。
pub(crate) fn report_failure(err: &anyhow::Error) -> i32 {
    let (code, message, hint, suggestion) = match err.downcast_ref::<UsageFailure>() {
        Some(usage) => (
            usage.code.clone(),
            usage.message.clone(),
            Some(usage.hint.clone()),
            usage.suggestion.clone(),
        ),
        // HTTP 层失败带结构化 code/hint：直接把稳定码透出去，
        // 否则会被兜底成 unexpected_error，调用方就分不清 token 失配与环境故障。
        None => match err.downcast_ref::<HttpFailure>() {
            Some(failure) => (
                failure.code.to_string(),
                failure.message.clone(),
                Some(failure.hint.clone()),
                None,
            ),
            // 非用法错误以前完全不给 hint，Agent 只能看到一句 `Error: ...`。
            // 这里补一条「先自检、再看日志、再重试」的通用可执行建议；具体命令仍可在
            // 自己的分支里给出更精确的 hint（本函数只在兜底时用）。
            None => (
                "unexpected_error".to_string(),
                err.to_string(),
                Some(format!(
                    "先运行 atb doctor 看环境自检（端口/扩展连接/配置），再到 atb logs --tail 100 查看 daemon 日志；\
                     若与 daemon 通信失败，可 atb status 确认后用 atb restart 重启。原始错误链：{err:#}"
                )),
                None,
            ),
        },
    };
    let mut value = json!({ "ok": false, "error": message, "error_code": code });
    if let Some(hint) = hint {
        if !hint.is_empty() {
            value["hint"] = json!(hint);
        }
    }
    if let Some(suggestion) = suggestion {
        if !suggestion.is_empty() {
            value["did_you_mean"] = json!(suggestion);
        }
    }
    write_stdout(&serialize(&value));
    if err.downcast_ref::<UsageFailure>().is_some() {
        EXIT_USAGE
    } else {
        EXIT_FAILURE
    }
}

fn serialize(value: &Value) -> String {
    if PRETTY_OUTPUT.load(Ordering::Relaxed) {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .unwrap_or_else(|_| "{}".to_string())
}

/// 预检失败的统一出口：`ok:false` + 退出码 2。
///
/// 智能体可以只凭退出码判断「是命令写错了（改命令）还是执行失败了（重试/换目标）」。
pub(crate) fn usage_error(problem: &preflight::Problem) -> Result<()> {
    FAILURE_EXIT_CODE.store(EXIT_USAGE, Ordering::Relaxed);
    write_stdout(&serialize(&problem.to_json()));
    Ok(())
}

pub(crate) fn spec_problem(err: crate::target_spec::SpecError) -> preflight::Problem {
    preflight::Problem {
        code: err.code,
        message: err.message,
        hint: err.hint,
        example: err.example,
        field: Some("tab".to_string()),
    }
}

pub(crate) fn print_json(mut value: Value) {
    // 失败要能被 `$?` / `&&` 捕获：旧实现恒 exit 0，智能体只能靠解析文本来判断。
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        // daemon 的选标签错误是纯文本（会话ID xxx 未连接）。在这里补上稳定的
        // error_code 和下一步提示，智能体不用解析中文就能判断「目标写错了」。
        let text = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 子串兜底只在 JSON 里**没有**结构化 error_code 时才走：daemon 侧正在补
        // typed error_code，一旦有了结构化码就不能再被中文子串覆盖——否则同一次
        // 失败在 daemon 升级前后会被归进不同分支，退出码漂移（契约被破坏）。
        if value.get("error_code").is_none() {
            if text.contains("exec_timeout") {
                // exec/action 超时不代表结果丢失，扩展可能稍后才回包。
                // exec_id 由 daemon 附在 error 文本里；保留时长可配（默认 1 小时）。
                let ttl = crate::settings::exec_result_ttl_sec();
                value["hint"] = json!(format!(
                    "结果不会丢：exec_id 见 error 字段，用 `atb job result <exec_id> --wait 60` 取回（daemon 保留 {ttl} 秒，可用 ATB_EXEC_RESULT_TTL_SEC 调整）"
                ));
            } else if text.trim_start().starts_with("outcome_unknown") {
                // 旧 daemon 只给文案前缀（`server/action.rs` 的 Display）：补结构化码，
                // 让 exec/click 等动作类命令统一走退出码 4，而不是被当成可重试的 1。
                value["error_code"] = json!("outcome_unknown");
                value["outcome_unknown"] = json!(true);
            } else if text.contains("未连接") {
                value["error_code"] = json!("target_not_found");
                value["hint"] = json!("从报错里的「当前可用」挑一个 tab_id，或用 --tab @1 按序号选；tabs --ids 可看完整列表");
            } else if text.contains("没有可用的浏览器标签页")
                || text.contains("当前没有可操作标签页")
            {
                value["error_code"] = json!("no_tabs");
                value["hint"] = json!("先 open 一个页面；如果浏览器开着，用 status 确认扩展已连接");
            }
        }
        let error_code = value.get("error_code").and_then(Value::as_str);
        // 结果未知优先于 error_code：`invalid_response`（响应截断）在 POST 上也要
        // 归到退出码 4，不能用它的字面码去命中别的分支。
        let outcome_unknown = value
            .get("outcome_unknown")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let code = if outcome_unknown {
            EXIT_UNKNOWN
        } else {
            match error_code {
                Some("job_wait_timeout") => EXIT_JOB_TIMEOUT,
                // 4 = 结果未知：POST 已发出但响应丢失，动作可能已执行。
                // 与 1（明确失败）分开，智能体不应盲目重试，先查询或带幂等键重发。
                Some("outcome_unknown") => EXIT_UNKNOWN,
                // 当前 daemon 不再生产该码（统一用 outcome_unknown），
                // 但协议 4 的旧 daemon 仍可能回它；保留映射属兼容分支，不是死代码。
                // 与 outcome_unknown 同级：不要重放，先查状态。
                Some("action_indeterminate") => EXIT_UNKNOWN,
                // 2 = 命令本身写错了（选择器 / JS / 参数 / 目标），改命令就能解决。
                // 和 1（执行失败，可能重试）分开，智能体不用读错误文本就能选分支。
                Some(code) if is_usage_error_code(code) => EXIT_USAGE,
                _ => EXIT_FAILURE,
            }
        };
        FAILURE_EXIT_CODE.store(code, Ordering::Relaxed);
    }
    // 2026-11-13：把「当前操作的是哪个浏览器」提示到 stderr，不污染 stdout JSON。
    maybe_print_browser_banner(&value);
    let text = if PRETTY_OUTPUT.load(Ordering::Relaxed) {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .unwrap_or_else(|_| "{}".to_string());
    write_stdout(&text);
}

// 在 stderr 提示当前浏览器身份，避免「以为是 Chrome，实际在 Edge」这类误判。
///
/// - `ATB_BROWSER_BANNER=off`：不提示；`always`：每条命令都提示；
/// - `once`（默认）：同一 agent + 同一浏览器 10 分钟内只提示一次，状态存
///   `~/.atb/banner.json`（写失败只影响提示，不影响命令）。
///
/// stdout 的 JSON 完全不变，stderr 是给人看的。
fn maybe_print_browser_banner(value: &Value) {
    let mode = crate::settings::browser_banner();
    if mode == "off" {
        return;
    }
    let browser = value
        .get("browser")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/result/browser").and_then(Value::as_str))
        .or_else(|| {
            value
                .pointer("/resolved_target/browser")
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let Some(browser) = browser else {
        return;
    };
    let agent = AGENT_ID.get().map(String::as_str).unwrap_or("default");
    let state_path = config::user_config_dir()
        .ok()
        .map(|dir| dir.join("banner.json"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    if mode == "once" {
        if let Some(path) = state_path.as_ref() {
            if let Ok(raw) = fs::read_to_string(path) {
                if let Ok(state) = serde_json::from_str::<Value>(&raw) {
                    let same_agent = state.get("agent").and_then(Value::as_str) == Some(agent);
                    let same_browser =
                        state.get("browser").and_then(Value::as_str) == Some(browser);
                    let at = state.get("at").and_then(Value::as_u64).unwrap_or(0);
                    if same_agent && same_browser && now.saturating_sub(at) < 600 {
                        return;
                    }
                }
            }
        }
    }
    eprintln!("→ 正在操作：{browser}");
    if mode == "once" {
        if let Some(path) = state_path {
            let body = json!({ "agent": agent, "browser": browser, "at": now });
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(path, body.to_string());
        }
    }
}

/// 这些 error_code 表示「命令写错了」，对应退出码 2。
///
/// 判据是**结构优先、白名单兜底**：先按命名约定认前缀，再列那些不符合约定的历史码。
/// 以前只有白名单，新增一个 `invalid_frame` / `unsupported_world` 而忘了登记，就会
// 静默落到退出码 1（= 可重试），Agent 于是反复重试一条本来就写错的命令。
///
/// 反例：`idempotency_in_flight_timeout` **不要**加进白名单——它表示首次请求仍在执行、
/// 稍后可重试，属于执行失败（退出码 1）。
pub(crate) fn is_usage_error_code(code: &str) -> bool {
    /// 命名约定：参数/目标/语法类错误都用这些前缀。
    ///
    /// `too_many_` 同样属于「调用方要改」：`too_many_rules` 是 net 规则数超过每
    /// profile 上限（默认 64），原样重试永远还是满的，必须先 `net unroute` / `net reset`。
    const USAGE_PREFIXES: &[&str] = &["invalid_", "unsupported_", "not_a_", "empty_", "too_many_"];
    /// 例外：同样是 `invalid_` 开头，但属于「daemon 侧异常」，重试或重启才有意义。
    /// 把它们判成 2 会让 Agent 去改一条本来没错的命令。
    const NOT_USAGE: &[&str] = &["invalid_response", "invalid_api_token"];
    if !NOT_USAGE.contains(&code) && USAGE_PREFIXES.iter().any(|prefix| code.starts_with(prefix)) {
        return true;
    }
    // 不符合命名约定的历史码（对外已经稳定，不能改名）。
    matches!(
        code,
        "file_not_found"
            // 2026-11-13：注入/exec 的本地语法预检失败，改脚本即可。
            | "syntax_error"
            | "output_path_collision"
            | "target_out_of_range"
            | "target_not_found"
            | "no_tabs"
            | "job_not_found"
            // job_evicted：等待中的 job 已被 daemon 淘汰（另一个 agent 正在 daemon
            // 侧新增该码），继续等 / 重试同一条等待命令没有意义，属于调用侧要先
            // 修正用法的问题，归入退出码 2 而不是「可重试失败」。
            | "job_evicted"
            // impl-errors 新增的三个目标选择类错误码：语义都是「调用方选的目标
            // 不对，改命令即可」，漏登记会静默落到退出码 1（可重试），Agent 会
            // 反复重试一条本来就该改参数的命令（同 教训）。
            | "session_mismatch"
            | "no_target_tabs"
            | "target_ambiguous"
            // 解析后目标已变化（`close --tab @N` 的 TOCTOU 复核）。
            // 属于「调用方选的目标不对」，改命令即可，不是可重试的执行失败。
            | "target_mismatch"
            // @e 重定位不唯一 / 找不到：调用方选的引用已不适用，重新 snapshot 即可。
            | "ref_ambiguous"
            | "ref_not_found"
            // 同一个 --idempotency-key 绑定了不同请求（409）。这是调用方
            // 复用了不该复用的 key，改 key / 改请求即可，重试同一条会一直 409。
            | "idempotency_key_reused"
            // 解析出的目标已经失效（标签被关/会话被回收）。改命令重新选目标，
            // 盲目重试同一个目标只会一直失败。
            | "target_lost"
            // 同族：单条 mock/route 的 body 超过 `ATB_NET_RULE_MAX_BYTES`（默认 2MiB）。
            // 和 `too_many_rules` 一样是「改 payload / 调上限」才解决，原样重试永远失败；
            // 这里用**精确码**而不是 `net_rule_` 前缀：daemon 的规则校验未来可能新增
            // 真正可重试的 `net_rule_*`，不能被一刀切归成用法错误。
            | "net_rule_too_large"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `too_many_rules` 属于「改命令/先清理」那一档：原样重试永远是满的。
    /// 同族前缀 `too_many_` 统一进 USAGE_PREFIXES，避免以后新增同类码又落回 1。
    #[test]
    fn too_many_prefix_maps_to_usage_exit() {
        assert!(is_usage_error_code("too_many_rules"));
        assert!(is_usage_error_code("too_many_nodes"));
        // 单条 body 超上限同样是「改 payload 才解决」。
        assert!(is_usage_error_code("net_rule_too_large"));
        // 精确码而不是前缀：`net_rule_` 家族未来的其它码不能被顺手判成 2。
        assert!(!is_usage_error_code("net_rule_pending_flush"));
        // 反例：这些是执行失败/环境故障，不能被前缀或白名单误判成 2。
        assert!(!is_usage_error_code("ws_backpressure"));
        assert!(!is_usage_error_code("ws_disconnected"));
        assert!(!is_usage_error_code("task_panicked"));
        assert!(!is_usage_error_code("invalid_response"));
        assert!(!is_usage_error_code("invalid_api_token"));
        assert!(!is_usage_error_code("idempotency_in_flight_timeout"));
    }
}
