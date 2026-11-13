use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// 可选数值字段为 `None` 时不要下发 `null`。
///
/// `preflight` 把数值字段的 `null` 当成「用户写了 NaN/Inf，`json!` 序列化成了 null」
/// 并报 `invalid_number`（检测手段）；但 `Option<usize>` 的 `None` 同样
/// 序列化成 null —— 那是「没传、用服务端默认」，不是错误。两者在 JSON 里无法区分，
/// 所以在构造 payload 时就把 None 摘掉，让 null 只剩「显式传入非有限值」一种来源。
fn omit_absent_numbers(payload: &mut Value, keys: &[&str]) {
    let Some(map) = payload.as_object_mut() else {
        return;
    };
    for key in keys {
        if map.get(*key).is_some_and(Value::is_null) {
            map.remove(*key);
        }
    }
}

/// CLI 侧统一的 HTTP 超时预算：业务超时 + `ATB_HTTP_SLACK_SEC`（默认 10s）。
///
/// 以前每个命令各自写 `args.timeout` / `+5.0` / `+10.0`：同样是 30s 的业务超时，
/// cookies 只给 30s、snapshot 30s，daemon 还在等待时 CLI 已经断开并报「超时」
// 。所有直接用 `args.timeout` 的客户端超时统一走这里。
pub(crate) fn http_budget(timeout: f64) -> f64 {
    if !timeout.is_finite() {
        return 30.0;
    }
    timeout.max(0.0) + crate::settings::http_slack_sec()
}

/// 写/破坏性命令是否强制要求显式 `--tab`（`ATB_REQUIRE_TAB=1`，默认关闭）。
///
/// 默认行为保持不变：不设环境变量时，缺省 `--tab` 仍作用于 daemon 当前标签页。
pub(crate) fn require_tab_enabled() -> bool {
    crate::settings::require_tab()
}

// 写类 op 缺少 `--tab` 时直接以用法错误拒绝。
///
/// 只在 `ATB_REQUIRE_TAB=1` 时生效：写/破坏性操作误作用到用户正在看的标签页
/// 代价很高，开启后强制调用方明确目标；关闭时完全沿用旧行为。
pub(crate) fn enforce_write_tab(op: &str, tab: Option<&str>) -> Result<()> {
    const WRITE_OPS: &[&str] = &[
        "click",
        "mouse-click",
        "fill",
        "send-keys",
        "select",
        "check",
        "hover",
        "scroll",
        "upload",
        "inject",
    ];
    if !require_tab_enabled() || !WRITE_OPS.contains(&op) {
        return Ok(());
    }
    if tab.map(str::trim).is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }
    Err(usage_failure(
        "target_not_found",
        format!("{op}: 写/破坏性操作必须显式指定 --tab（已开启 ATB_REQUIRE_TAB）"),
        "先 atb tabs --ids 看清目标，再 --tab <tab_id>；确实要操作当前标签时显式写 --tab current",
    ))
}

/// 单个脚本文件的上限（MiB）。超限直接拒绝，避免把整个文件读进内存后
/// 再因为 HTTP 体过大被 daemon / reqwest 拒绝，错误现场反而丢失。
///
/// 上限经 `ATB_SCRIPT_FILE_MAX_MB` 配置（默认 32 MiB，值域 1..=256）。
fn script_file_max_bytes() -> u64 {
    crate::settings::script_file_max_mb() as u64 * 1024 * 1024
}

/// 读取脚本文件（`--file` / `--cleanup-file`），错误统一带**绝对路径 + 大小**。
///
/// 旧实现直接 `read_to_string`，用户只能看到 `No such file or directory`，
// 既不知道 CLI 实际解析到了哪个路径，也不知道文件是不是编码/大小问题。
pub(crate) fn read_script_file(op: &str, path: &Path) -> Result<String> {
    let raw = path.to_string_lossy();
    if raw == "-" {
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer).map_err(|err| {
            usage_failure(
                "invalid_argument",
                format!("{op}: 从 stdin 读取脚本失败: {err}"),
                "确认管道/heredoc 正常；也可以把脚本保存成文件后用 --file <路径>",
            )
        })?;
        return Ok(buffer);
    }
    let absolute = absolutize_path(&raw);
    let absolute_path = Path::new(&absolute);
    let metadata = match std::fs::metadata(absolute_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(usage_failure(
                "file_not_found",
                format!("{op}: 脚本文件不存在: {absolute}"),
                "确认路径存在；相对路径以当前工作目录为基准，可用绝对路径避免歧义",
            ));
        }
        Err(err) => {
            return Err(usage_failure(
                "file_not_found",
                format!("{op}: 无法读取脚本文件: {absolute}（{err}）"),
                "确认文件权限与路径；Windows 下注意反斜杠与盘符，或改用绝对路径",
            ));
        }
    };
    if !metadata.is_file() {
        return Err(usage_failure(
            "file_not_found",
            format!("{op}: 脚本路径不是普通文件: {absolute}"),
            "传入具体的 .js 文件；目录请先拼接出目标文件路径",
        ));
    }
    let size = metadata.len();
    if size > script_file_max_bytes() {
        return Err(usage_failure(
            "script_file_too_large",
            format!(
                "{op}: 脚本文件过大: {absolute}（{:.1} MiB，上限 {} MiB）",
                size as f64 / (1024.0 * 1024.0),
                crate::settings::script_file_max_mb()
            ),
            "拆分脚本，或把公共部分抽成另一个文件；也可以确认路径指向的是脚本而不是数据/日志文件",
        ));
    }
    let bytes = std::fs::read(absolute_path).map_err(|err| {
        usage_failure(
            "file_not_found",
            format!("{op}: 读取脚本文件失败: {absolute}（{err}）"),
            "确认文件权限；文件可能正被其它程序占用，稍后重试",
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        usage_failure(
            "invalid_encoding",
            format!("{op}: 脚本文件不是 UTF-8 编码: {absolute}（{size} 字节）"),
            "用编辑器另存为 UTF-8（无 BOM 更稳）；exec/inject 也可以用 --file - 通过管道传入 UTF-8 文本",
        )
    })
}

/// `exec` 的脚本来源：位置参数或一个/多个 `--file`。
///
/// 多文件按出现顺序用 `"\n;\n"` 拼接：分号保证前一个文件末尾没有分号时
// 也不会把两条语句粘成一条。`-` 只允许出现一次，避免第二次
/// 读到已经耗尽的 stdin 却静默变成空脚本。
fn collect_exec_script(args: &ExecArgs) -> Result<String> {
    if args.file.is_empty() {
        // Windows 命令行总上限约 32K，8000 字符已经接近「再拼几个参数就超」的区间。
        if args.script.chars().count() > 8_000 {
            eprintln!("提示：脚本较长，Windows 命令行上限约 32K；建议 atb exec --file script.js");
        }
        return Ok(args.script.clone());
    }
    let stdin_count = args
        .file
        .iter()
        .filter(|path| path.as_os_str() == std::ffi::OsStr::new("-"))
        .count();
    if stdin_count > 1 {
        return Err(usage_failure(
            "unsupported_combination",
            "exec: --file - 只能出现一次",
            "stdin 只能读一遍；多次需要 stdin 时先保存成文件，或把内容合并后通过一个 --file - 传入",
        ));
    }
    let mut parts: Vec<String> = Vec::with_capacity(args.file.len());
    for file in &args.file {
        parts.push(read_script_file("exec", file)?);
    }
    Ok(parts.join("\n;\n"))
}

/// `ATB_JS_CHECK=warn` 时在 stderr 提示本地检查已跳过（stdout 的 JSON 不变）。
fn warn_if_js_check_skipped(explicit_no_preflight: bool) {
    if !explicit_no_preflight && crate::settings::js_check() == "warn" {
        eprintln!("warning: 本地 JS 检查已跳过（ATB_JS_CHECK=warn）");
    }
}

/// 从 `exec_id=<id>` 里取出 id；daemon 的文本形如
/// `... exec_id=abc123；结果可能已通过 GET ...`。
fn extract_exec_id(text: &str) -> Option<String> {
    let rest = text.split("exec_id=").nth(1)?;
    let id: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect();
    (!id.is_empty()).then_some(id)
}

// 把 `exec` 超时错误里的 hint 改写成可直接执行的 `atb job result` 命令。
///
/// daemon 侧 `exec_timeout` 目前是纯文本错误（可能没有 `error_code`），
/// `print_json` 的兜底 hint 只告诉用户「去查 GET /exec-result」，不是一条能复制
/// 执行的命令。这里在 CLI 出口统一改写，并保留原 hint 到 `original_hint`。
fn rewrite_exec_timeout_hint(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let error_text = object
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let is_timeout = object.get("error_code").and_then(Value::as_str) == Some("exec_timeout")
        || error_text.contains("exec_id=");
    if is_timeout {
        if let Some(exec_id) = extract_exec_id(&error_text) {
            if let Some(original) = object.get("hint").and_then(Value::as_str) {
                object.insert("original_hint".to_string(), json!(original));
            }
            object.insert(
                "hint".to_string(),
                json!(format!(
                    "结果可能已产生：运行 `atb job result {exec_id} --wait 60` 取回（迟到结果在 daemon 内保留一段时间）；\
                     也可增大 --timeout 或改用 --async"
                )),
            );
            object.insert("exec_id".to_string(), json!(exec_id));
            if !object.contains_key("error_code") {
                object.insert("error_code".to_string(), json!("exec_timeout"));
            }
        }
    }
    // `/multi` 的失败条目在 results[] 里；同一套改写递归应用。
    if let Some(results) = object.get_mut("results").and_then(Value::as_array_mut) {
        for item in results {
            rewrite_exec_timeout_hint(item);
        }
    }
}

/// 供 `exec` / `measure` 等复用 `/exec` 的命令调用：把 exec 超时 hint 改成可执行命令。
pub(crate) fn post_process_exec_error(mut value: Value) -> Value {
    rewrite_exec_timeout_hint(&mut value);
    value
}

/// 把 preflight 的 `invalid_js` 转成 inject 的 `syntax_error`，并补上附近 40 字符。
fn js_syntax_problem(label: &str, code: &str, problem: preflight::Problem) -> preflight::Problem {
    let (line, column) = extract_line_col(&problem.message).unwrap_or((1, 1));
    let snippet = snippet_around(code, line, column, 40);
    let position = format!("第 {line} 行第 {column} 列");
    let message = if problem.message.contains("第 ") {
        format!("{label}语法错误：{}；附近：{snippet}", problem.message)
    } else {
        format!(
            "{label}语法错误（{position}）：{}；附近：{snippet}",
            problem.message
        )
    };
    preflight::Problem::new(
        "syntax_error",
        message,
        "修正语法后重试；如果脚本在浏览器里确实能跑（本地检查误报），加 --no-preflight 或设 ATB_JS_CHECK=warn/off",
    )
    .with_field("code")
}

/// 从「第 N 行第 M 列」里解析位置；解析不到时返回 None。
fn extract_line_col(message: &str) -> Option<(usize, usize)> {
    let rest = message.split("第 ").nth(1)?;
    let line: usize = rest.split(" 行").next()?.trim().parse().ok()?;
    let after_line = rest.split(" 行第 ").nth(1)?;
    let column: usize = after_line.split(" 列").next()?.trim().parse().ok()?;
    Some((line, column))
}

/// 取位置附近最多 `limit` 个字符（前后各一半），换行显示成 `\n`。
fn snippet_around(code: &str, line: usize, column: usize, limit: usize) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut offset = None;
    let (mut current_line, mut current_column) = (1usize, 1usize);
    for (index, ch) in chars.iter().enumerate() {
        if current_line == line && current_column == column {
            offset = Some(index);
            break;
        }
        if *ch == '\n' {
            current_line += 1;
            current_column = 1;
        } else {
            current_column += 1;
        }
    }
    let offset = offset.unwrap_or(0);
    let half = limit / 2;
    let start = offset.saturating_sub(half);
    let end = (start + limit).min(chars.len());
    chars[start..end]
        .iter()
        .collect::<String>()
        .replace('\n', "\\n")
}

/// 页面级 payload 的统一收尾：路径绝对化 → 参数预检 → 目标语法解析。
///
/// 同步路径（[`dispatch_page`]）与异步路径（`cmd_pipeline::submit_job`）**必须**走同一份
/// 逻辑：以前异步分支直接把原始 payload 交给 `/job/submit`，于是
/// `screenshot --async --out shots/a.png` 的路径被 daemon 按 `current_dir(home)` 解析成
/// `%USERPROFILE%\shots\a.png`，`upload --async` 则直接报文件不存在。
pub(crate) fn finalize_page_payload(
    op: &str,
    payload: &mut Value,
) -> std::result::Result<(), preflight::Problem> {
    // 相对路径以当前 shell 的 cwd 为基准绝对化后再下发。
    absolutize_payload_paths(
        payload,
        &["out", "files", "dir", "save_path", "output", "file"],
    );
    preflight::preflight_payload(op, payload)?;
    // `no_preflight` 是 CLI 本地预检的提示位：`InjectRequest` 是
    // `deny_unknown_fields` 且没有这个字段，原样下发会被 daemon 以 422 拒绝。
    // exec 的 `ExecRequest` 有该字段（用于记录），只对 inject 摘掉。
    if op == "inject" {
        if let Some(object) = payload.as_object_mut() {
            object.remove("no_preflight");
        }
    }
    // 目标语法（`@x` / 空段 / 非法列表）在本地就能判定，异步提交也必须拦，
    // 否则错误会推迟到 job 结果里，退出码从 2 变成 0。
    if let Some(tab) = payload.get("switch_tab_id").and_then(Value::as_str) {
        if let Err(err) = TargetSpec::parse(tab) {
            return Err(spec_problem(err));
        }
    }
    Ok(())
}

/// 页面级命令的统一分发。
///
/// - 单目标：走原来的端点，行为与旧版本完全一致；
/// - 多目标（`--tab all` / `1,2,3` / `@1-3` / `url:` / `title:` / `group:`）：
///   一次请求打到 `/multi`，由 daemon 并发执行并按输入顺序汇总；
/// - `--dry-run`：只解析目标和校验参数，输出计划，不执行。
pub(crate) fn dispatch_page(
    op: &str,
    path: &str,
    payload: Value,
    tab: Option<&str>,
    browser: Option<&str>,
    profile: Option<&str>,
    timeout: f64,
) -> Result<()> {
    dispatch_page_with(op, path, payload, tab, browser, profile, timeout, None)
}

/// 同 [`dispatch_page`]，额外在打印前对 daemon 回包做一次后处理。
///
/// 目前只有 `exec` 用它把 `exec_timeout` 的 hint 改写成可直接执行的
// `atb job result <exec_id> --wait 60`；其它命令传 `None`，行为完全不变。
#[allow(clippy::too_many_arguments)]
fn dispatch_page_with(
    op: &str,
    path: &str,
    payload: Value,
    tab: Option<&str>,
    browser: Option<&str>,
    profile: Option<&str>,
    timeout: f64,
    post: Option<fn(Value) -> Value>,
) -> Result<()> {
    let mut payload = payload;
    // 写类命令的显式目标护栏：默认关闭，ATB_REQUIRE_TAB=1 时生效。
    enforce_write_tab(op, tab)?;
    if let Err(problem) = finalize_page_payload(op, &mut payload) {
        return usage_error(&problem);
    }
    let spec = match TargetSpec::parse(tab.unwrap_or("")) {
        Ok(spec) => spec,
        Err(err) => return usage_error(&spec_problem(err)),
    };
    let dry_run = DRY_RUN.load(Ordering::Relaxed);
    if dry_run || spec.is_multi() || spec.needs_daemon_resolution() {
        let mut args = payload;
        if let Some(object) = args.as_object_mut() {
            // 目标由 daemon 按解析结果注入，避免两边各算一遍还算出不同结果。
            object.remove("switch_tab_id");
            // 截图/PDF 的输出路径只有在**真的多目标**时才要求占位符。
            // 以前这段写在 `dry_run || is_multi()` 分支里，于是 `--tab @1 --out a.png`
            // 和任何 `--out a.png --dry-run` 都被判成冲突并以退出码 2 拒绝。
            if spec.is_multi() {
                if let Some(out) = object.get("out").and_then(Value::as_str) {
                    if !out.contains("{tab}") && !out.contains("{index}") {
                        return usage_error(
                            &preflight::Problem::new(
                                "output_path_collision",
                                format!("多目标写同一个输出文件 {out:?} 会互相覆盖"),
                                "在 --out 里加 {tab} 或 {index} 占位符，例如 shots/{index}-{tab}.png",
                            )
                            .with_field("out"),
                        );
                    }
                }
            }
        }
        let body = json!({
            "op": op,
            "args": args,
            "target": tab.unwrap_or(""),
            "browser": browser,
            "profile": profile,
            "dry_run": dry_run,
        });
        let value = request("POST", "/multi", Some(body), timeout)?;
        if dry_run {
            // dry-run 的字段形状要和真实执行一致，否则 Agent 用 dry-run 校准字段名后
            // 正式执行必然解析失败。
            print_json(normalize_dry_run(value));
            return Ok(());
        }
        print_json(apply_post(value, post));
        return Ok(());
    }
    print_json(apply_post(
        request("POST", path, Some(payload), timeout)?,
        post,
    ));
    Ok(())
}

/// 可选的回包后处理；`None` 时原样返回，保证旧调用点零行为变化。
fn apply_post(value: Value, post: Option<fn(Value) -> Value>) -> Value {
    match post {
        Some(post) => post(value),
        None => value,
    }
}

/// 把 `/multi` 的 `targets`（计划）改写成与真实执行同形的 `results`。
fn normalize_dry_run(value: Value) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };
    let Some(targets) = map
        .remove("targets")
        .and_then(|value| value.as_array().cloned())
    else {
        return Value::Object(map);
    };
    let results: Vec<Value> = targets
        .into_iter()
        .map(|target| {
            json!({
                "index": target.get("index").cloned().unwrap_or(Value::Null),
                "tab_id": target.get("tab_id").cloned().unwrap_or(Value::Null),
                "title": target.get("title").cloned().unwrap_or(Value::Null),
                "url": target.get("url").cloned().unwrap_or(Value::Null),
                "ok": true,
                "planned": true,
            })
        })
        .collect();
    let count = results.len();
    map.insert("count".to_string(), json!(count));
    map.insert("results".to_string(), Value::Array(results));
    Value::Object(map)
}

pub(crate) fn cmd_scan(args: ScanArgs, is_async: bool) -> Result<()> {
    let mut payload = json!({
        "tabs_only": args.tabs_only,
        "text_only": args.text_only,
        "fast": args.fast,
        "max_nodes": args.max_nodes,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "timeout": args.timeout,
    });
    omit_absent_numbers(&mut payload, &["max_nodes"]);
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("scan", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("scan", payload)?);
    } else {
        dispatch_page(
            "scan",
            "/scan",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_exec(args: ExecArgs, is_async: bool) -> Result<()> {
    // 位置参数脚本与 --file 同时给时，旧实现静默只用 --file，用户写的那段
    // 脚本被无声丢弃（改成 --file 后忘了删旧参数就会踩）。两者的优先级不该靠猜，
    // 直接在发请求前以 exit 2 拒绝。位置参数缺省是空串，不算「给了」。
    if !args.script.is_empty() && !args.file.is_empty() {
        return Err(usage_failure(
            "unsupported_combination",
            "exec: 位置参数脚本与 --file 只能给一个",
            "要么 atb exec '<js>'，要么 atb exec --file script.js（`--file -` 读 stdin）",
        ));
    }
    let script = collect_exec_script(&args)?;
    warn_if_js_check_skipped(args.no_preflight);
    // `ATB_JS_CHECK=off` 与 `--no-preflight` 等价：payload 明确告诉 daemon/异步任务
    // 本地检查已跳过。`warn` 不写这个字段，但 preflight 会按档位跳过（见 js_check_skipped）。
    let no_preflight = args.no_preflight || crate::settings::js_check() == "off";
    let mut payload = json!({
        "script": script,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "no_monitor": !args.monitor,
        "wait_js": args.wait_js,
        "wait_timeout": args.wait_timeout,
        "wait_interval": args.wait_interval,
        "timeout": args.timeout,
        "max_bytes": args.max_bytes,
        "world": args.world.to_ascii_uppercase(),
        "frame_id": args.frame,
        "all_frames": args.all_frames,
    });
    // 只在确实跳过时下发：老 daemon 的 `ExecRequest` 是 deny_unknown_fields，
    // 常把 `no_preflight:false` 当成未知字段回 422（升级期间旧 daemon 仍在跑）。
    if no_preflight {
        payload["no_preflight"] = json!(true);
    }
    omit_absent_numbers(&mut payload, &["max_bytes"]);
    // `exec --async --dry-run` 以前会在 is_async 分支里直接提交 job。
    // 现在与其它命令走同一份计划输出：不 ensure_server、不发任何请求。
    if async_dry_run_plan("exec", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("exec", payload)?);
    } else {
        // 客户端要覆盖「页面执行 + 轮询等待」两段，并比服务端多留余量，
        // 让服务端的结构化超时错误先返回，而不是 reqwest 的裸 "operation timed out"。
        let timeout = http_budget(args.timeout + args.wait_timeout);
        // 单目标、多目标与 dry-run 都走 dispatch_page_with：后者在打印前对回包做
        // `exec_timeout` hint 改写，/multi 的 results[] 也会被递归处理。
        dispatch_page_with(
            "exec",
            "/exec",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            timeout,
            Some(post_process_exec_error),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_cookies(args: CookiesArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "url": args.url,
        "action": if args.clear { "clear" } else { "list" },
        "domain": args.domain,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("cookies", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("cookies", payload)?);
    } else {
        dispatch_page(
            "cookies",
            "/cookies",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_upload(args: UploadArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "target": args.target,
        "files": args.files,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "timeout": args.timeout,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("upload", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("upload", payload)?);
    } else {
        dispatch_page(
            "upload",
            "/upload",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_snapshot(args: SnapshotArgs, is_async: bool) -> Result<()> {
    let mut payload = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "offset": args.offset,
        "limit": args.limit,
        "details": args.details,
        "js": args.js,
        "interactive_only": args.interactive_only,
        "max_nodes": args.max_nodes,
        // daemon 的 /snapshot 早就有 skip_generic 字段，CLI 却一直没有下发通道，
        // 于是「不想被通用容器节点挤满 @e 编号」的用户无法表达。默认行为保持不变
        // （skip_generic=true），加 --no-skip-generic 才显式关闭。
        "skip_generic": !args.no_skip_generic,
        "timeout": args.timeout,
    });
    omit_absent_numbers(&mut payload, &["max_nodes"]);
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("snapshot", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("snapshot", payload)?);
    } else {
        dispatch_page(
            "snapshot",
            "/snapshot",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_read(args: ReadArgs, is_async: bool) -> Result<()> {
    let mut payload = json!({
        "selector": args.selector,
        "max_chars": args.max_chars,
        // 页面侧 page_read.js 的 innerTextLimit（默认 30000），None 不下发。
        "inner_text_limit": args.inner_text_limit,
        "links": args.links,
        "forms": !args.no_forms,
        "include_hidden": args.include_hidden,
        "budget_ms": args.budget_ms,
        "max_nodes": args.max_nodes,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "timeout": args.timeout,
    });
    omit_absent_numbers(
        &mut payload,
        &["max_nodes", "budget_ms", "inner_text_limit"],
    );
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("read", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("read", payload)?);
    } else {
        dispatch_page(
            "read",
            "/read",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_find(args: FindArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "selector": args.selector,
        "text": args.text,
        "xpath": args.xpath,
        "limit": args.limit,
        "exact": args.exact,
        "visible": !args.include_hidden,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "timeout": args.timeout,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("find", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("find", payload)?);
    } else {
        dispatch_page(
            "find",
            "/find",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_inject(args: InjectArgs, is_async: bool) -> Result<()> {
    let (action, name, code, cleanup_code, metadata) = if args.list {
        ("list", None, None, None, None)
    } else if let Some(name) = args.remove.clone() {
        ("remove", Some(name), None, None, None)
    } else {
        if args.script.is_some() && args.file.is_some() {
            return Err(usage_failure(
                "unsupported_combination",
                "inject: 位置参数脚本与 --file 只能给一个",
                "要么 atb inject '<js>'，要么 atb inject --file script.js（`--file -` 读 stdin）",
            ));
        }
        let code = match args.file.as_ref() {
            Some(path) => read_script_file("inject", path)?,
            None => args
                .script
                .clone()
                .ok_or_else(|| usage_err!("inject: 需要脚本内容，或 --file/--list/--remove"))?,
        };
        // 两个 `--file -` 会第二次读到 EOF，清理脚本静默为空；提前拒绝。
        if args
            .file
            .as_deref()
            .map(|path| path.as_os_str() == std::ffi::OsStr::new("-"))
            == Some(true)
            && args
                .cleanup_file
                .as_deref()
                .map(|path| path.as_os_str() == std::ffi::OsStr::new("-"))
                == Some(true)
        {
            return Err(usage_failure(
                "unsupported_combination",
                "inject: --file - 与 --cleanup-file - 不能同时使用",
                "stdin 只能读一次；把其中一个脚本保存成文件，或合并后再传",
            ));
        }
        let cleanup_code = match args.cleanup_file.as_ref() {
            Some(path) => Some(read_script_file("inject --cleanup-file", path)?),
            None => None,
        };
        if args.userscript {
            let metadata = super::cmd_measure::parse_userscript_metadata(&code);
            if !metadata.valid {
                eprintln!(
                    "warning: --userscript 但脚本没有 ==UserScript== 元数据块；仍按油猴环境注入（GM shim 可用）"
                );
            }
            let metadata = Some(metadata);
            let action = match args.persist.clone() {
                Some(name) => ("persist", Some(name), Some(code), cleanup_code, metadata),
                None => ("run", None, Some(code), cleanup_code, metadata),
            };
            validate_inject_safety(&args, action.0)?;
            if let Err(problem) =
                check_inject_syntax(&args, action.2.as_deref(), action.3.as_deref())
            {
                return usage_error(&problem);
            }
            action
        } else {
            let action = match args.persist.clone() {
                Some(name) => ("persist", Some(name), Some(code), cleanup_code, None),
                None => ("run", None, Some(code), cleanup_code, None),
            };
            validate_inject_safety(&args, action.0)?;
            if let Err(problem) =
                check_inject_syntax(&args, action.2.as_deref(), action.3.as_deref())
            {
                return usage_error(&problem);
            }
            action
        }
    };
    let mut payload = json!({
        "action": action,
        "world": args.world.to_ascii_uppercase(),
        "frame_id": args.frame,
        "all_frames": args.all_frames,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });
    // 只在跳过时加：`InjectRequest` 没有这个字段，finalize_page_payload 会摘掉它；
    // 这里先只表达「用户/环境要求跳过」，避免 payload 里出现误导性的 false。
    if args.no_preflight || crate::settings::js_check() == "off" {
        payload["no_preflight"] = json!(true);
    }
    if let Some(name) = name {
        payload["name"] = json!(name);
    }
    if let Some(code) = code {
        payload["code"] = json!(code);
    }
    if let Some(cleanup) = cleanup_code {
        payload["cleanup_code"] = json!(cleanup);
    }
    if !args.matches.is_empty() {
        payload["matches"] = json!(args.matches);
    }
    if !args.cleanup_selectors.is_empty() {
        payload["cleanup_selectors"] = json!(args.cleanup_selectors);
    }
    if args.userscript {
        payload["userscript"] = json!(true);
    } else if args.gm_namespace.is_some() {
        eprintln!("提示：--gm-namespace 只在 --userscript 时生效，已忽略");
    }
    if let Some(namespace) = inject_gm_namespace(&args, metadata.as_ref()) {
        payload["gm_namespace"] = json!(namespace);
    }
    // TTL 只在 persist 动作里有意义；list/remove 不校验、不下发。
    // 只有用户显式给了 --ttl 才下发 ttl_sec；否则让扩展按「扩展设置页
    // persistTtlSec → daemon 默认 ATB_INJECT_PERSIST_DEFAULT_TTL_SEC」取默认值。
    // 旧实现总是把 CLI 默认值写进 ttl_sec，导致扩展设置页的选项永远不生效。
    if action == "persist" {
        if args.persist_forever {
            payload["persist_forever"] = json!(true);
        } else if let Some(ttl) = args.ttl {
            payload["ttl_sec"] = json!(ttl);
        }
    }
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路
    // （inject 会改动页面，是最不能「静默照做」的一类命令）。
    if async_dry_run_plan("inject", &payload, is_async)? {
        return Ok(());
    }
    // `--async inject` 以前静默同步执行：/job/submit 支持 inject，
    // 这里补上真正的后台提交。
    ensure_server()?;
    if is_async {
        print_json(submit_job("inject", payload)?);
        return Ok(());
    }
    dispatch_page(
        "inject",
        "/inject",
        payload,
        args.tab.as_deref(),
        args.browser.as_deref(),
        args.profile.as_deref(),
        http_budget(args.timeout),
    )?;
    Ok(())
}

// `inject --persist` 的安全规则：默认 TTL、显式永久、`--ttl` 值域。
///
/// `--list` / `--remove` 不进入这里：它们不需要脚本，也不该因为残留的 `--ttl`
/// 参数被拒绝。
fn validate_inject_safety(args: &InjectArgs, action: &str) -> Result<()> {
    // `--i-know-this-is-long-lived` 单独出现时只是无效，不报错（明确告知即可）。
    if args.i_know_this_is_long_lived && !args.persist_forever {
        eprintln!("提示：--i-know-this-is-long-lived 只在 --persist-forever 时有效，已忽略");
    }
    if action != "persist" {
        if args.persist_forever {
            return Err(usage_failure(
                "unsupported_combination",
                "inject: --persist-forever 需要配合 --persist <name>",
                "要永久驻留请写 `atb inject --persist my-script --persist-forever --i-know-this-is-long-lived`",
            ));
        }
        if args.ttl.is_some() {
            eprintln!("提示：--ttl 只对 --persist 生效，本次 run 注入已忽略");
        }
        return Ok(());
    }
    let max_ttl = crate::settings::inject_persist_max_ttl_sec();
    if args.persist_forever {
        // clap 已经通过 requires 拦住一次；这里再校验，直连 args 的测试/未来调用
        // 也不会绕过「永久驻留必须显式确认」。
        if !args.i_know_this_is_long_lived {
            return Err(usage_failure(
                "invalid_argument",
                "inject: --persist-forever 必须同时给 --i-know-this-is-long-lived",
                "永久注入会与页面/用户脚本长期共存：确认后加 --i-know-this-is-long-lived",
            ));
        }
        if args.ttl.is_some() {
            return Err(usage_failure(
                "unsupported_combination",
                "inject: --ttl 与 --persist-forever 不能同时使用",
                "要么用 --ttl <秒> 自动过期，要么用 --persist-forever --i-know-this-is-long-lived 永久驻留",
            ));
        }
        return Ok(());
    }
    if let Some(ttl) = args.ttl {
        if ttl == 0 || ttl > max_ttl {
            return Err(usage_failure(
                "invalid_argument",
                format!("inject: --ttl 必须在 1..={max_ttl} 秒之间（收到 {ttl}）"),
                format!(
                    "默认 {} 秒；上限可用 ATB_INJECT_PERSIST_MAX_TTL_SEC 调整，永久驻留用 --persist-forever",
                    crate::settings::inject_persist_default_ttl_sec()
                ),
            ));
        }
    } else {
        eprintln!(
            "持久注入使用默认 TTL（扩展设置页 persistTtlSec，未设置时回落到 {} 秒）；\
             要指定寿命加 --ttl <秒>，永久驻留需 --persist-forever --i-know-this-is-long-lived",
            crate::settings::inject_persist_default_ttl_sec()
        );
    }
    Ok(())
}

// inject 脚本的本地语法预检：失败给 `syntax_error` + 行列 + 40 字符片段。
fn check_inject_syntax(
    args: &InjectArgs,
    code: Option<&str>,
    cleanup_code: Option<&str>,
) -> std::result::Result<(), preflight::Problem> {
    let policy = crate::settings::js_check();
    if args.no_preflight || policy != "strict" {
        warn_if_js_check_skipped(args.no_preflight);
        return Ok(());
    }
    if let Some(code) = code {
        if let Err(problem) = preflight::validate_js(code) {
            return Err(js_syntax_problem("inject 脚本", code, problem));
        }
    }
    if let Some(code) = cleanup_code {
        if let Err(problem) = preflight::validate_js(code) {
            return Err(js_syntax_problem("inject 清理脚本", code, problem));
        }
    }
    Ok(())
}

/// GM 存储命名空间：显式 `--gm-namespace` → 元数据 `@name` → 文件名。
fn inject_gm_namespace(
    args: &InjectArgs,
    metadata: Option<&super::cmd_measure::UserscriptMetadata>,
) -> Option<String> {
    if !args.userscript {
        return None;
    }
    if let Some(namespace) = args.gm_namespace.clone() {
        return Some(namespace);
    }
    if let Some(name) = metadata.and_then(|metadata| metadata.name.clone()) {
        return Some(name);
    }
    args.file
        .as_ref()
        .filter(|path| path.as_os_str() != std::ffi::OsStr::new("-"))
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty())
}

pub(crate) fn cmd_frames(args: FramesArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("frames", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    // 同 inject：frames 在 JOB_OPS 里，`--async frames` 应该真的异步。
    if is_async {
        print_json(submit_job("frames", payload)?);
        return Ok(());
    }
    dispatch_page(
        "frames",
        "/frames",
        payload,
        args.tab.as_deref(),
        args.browser.as_deref(),
        args.profile.as_deref(),
        http_budget(args.timeout),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_timeout_hint_points_to_job_result() {
        let value = json!({
            "ok": false,
            "error": "exec_timeout: 页面在 60.0s 内没有返回结果。exec_id=abc-123；结果可能已通过 GET /exec-result/abc-123 查询",
        });
        let out = post_process_exec_error(value);
        assert_eq!(out["error_code"], "exec_timeout");
        assert_eq!(out["exec_id"], "abc-123");
        assert!(
            out["hint"]
                .as_str()
                .unwrap()
                .contains("atb job result abc-123 --wait 60"),
            "{out}"
        );
    }

    #[test]
    fn exec_timeout_hint_is_rewritten_inside_multi_results() {
        let value = json!({
            "ok": false,
            "results": [
                { "ok": false, "error": "exec_timeout: ... exec_id=deadbeef ..." },
                { "ok": true }
            ]
        });
        let out = post_process_exec_error(value);
        let entry = &out["results"][0];
        assert_eq!(entry["exec_id"], "deadbeef");
        assert!(entry["hint"]
            .as_str()
            .unwrap()
            .contains("atb job result deadbeef --wait 60"));
        assert!(out["results"][1].get("hint").is_none());
    }

    #[test]
    fn exec_id_extraction_stops_at_punctuation() {
        assert_eq!(
            extract_exec_id("... exec_id=755c193e-b28e-4a2d-b07c-304ffe65f2a8；结果..."),
            Some("755c193e-b28e-4a2d-b07c-304ffe65f2a8".to_string())
        );
        assert_eq!(extract_exec_id("no id here"), None);
    }

    #[test]
    fn syntax_error_snippet_carries_line_and_column() {
        let code = "line1;\nline2;\nif (a) {\n  return 1;\n";
        let problem = preflight::validate_js(code).unwrap_err();
        let problem = js_syntax_problem("inject 脚本", code, problem);
        assert_eq!(problem.code, "syntax_error");
        assert!(problem.message.contains("第 3 行"), "{}", problem.message);
        assert!(problem.message.contains("附近："), "{}", problem.message);
    }
}
