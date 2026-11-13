use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// `atb launch`：用带 `--silent-debugger-extension-api` 的参数把浏览器拉起来。
///
/// 这条命令解决的是一个**只能靠启动参数**的问题：Chrome 被 `chrome.debugger` 附加时
/// 会在页面顶部挂「已开始调试此浏览器」，运行时没有任何 API 能去掉它。
/// 所以这里要做的不是「加个开关」，而是「按正确参数重启」，并且把风险说清楚：
/// 默认不动已在运行的窗口，只有 `--restart` 才温和关闭。
pub(crate) fn run_launch(args: LaunchArgs) -> Result<()> {
    // 相对 `--user-data-dir` 必须在这里绝对化：daemon 由 CLI 以 `current_dir(home)` 拉起，
    // 原样下发会被解析成 home 下的目录（启动与路径）。
    let user_data_dir = args
        .user_data_dir
        .as_ref()
        .map(|path| absolutize_path(&path.display().to_string()));
    if DRY_RUN.load(Ordering::Relaxed) {
        // 只算计划，不碰进程：判断「要不要 --restart」时先看这个。
        let kind = match args.browser.as_deref() {
            Some(raw) => launch::Kind::parse(raw)?,
            None => launch::auto_kind()?,
        };
        let plan = launch::plan(
            kind,
            user_data_dir.as_deref().map(std::path::Path::new),
            args.restart,
            &args.extra_args,
            &args.urls,
            args.restart && !args.no_restore,
        )?;
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "browser": plan.kind.name(),
                "exe": plan.exe.display().to_string(),
                "user_data_dir": plan.user_data_dir.display().to_string(),
                "args": plan.args,
                "would_close": plan.running.iter().map(|item| item.pid).collect::<Vec<_>>(),
                "already_silent": plan.already_silent,
                "silent_debug_banner": true,
            }
        }));
        return Ok(());
    }

    ensure_server()?;
    let before: Vec<String> = browser_targets_snapshot()
        .map(|targets| targets.into_iter().map(|(key, _)| key).collect())
        .unwrap_or_default();

    // 交给 daemon 去启动：浏览器必须是常驻进程的子进程，否则命令一结束就被回收。
    let payload = json!({
        "browser": args.browser,
        "user_data_dir": user_data_dir,
        "restart": args.restart,
        "force": args.force,
        "restore": !args.no_restore,
        "args": args.extra_args,
        "urls": args.urls,
    });
    let mut value = request("POST", "/launch", Some(payload), 90.0)?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        print_json(value);
        return Ok(());
    }
    // 已经带开关在跑：不用再等「新标签页出现」，直接如实返回。
    if value.pointer("/result/launched").and_then(Value::as_bool) == Some(false) {
        print_json(value);
        return Ok(());
    }

    // 再等「这个新实例的标签页」出现：用我们请求的 URL 匹配。
    // 不能只看「有没有新的 session_key」—— 那衡量的是 daemon 全局连接，
    // 别的浏览器或扩展周期性重连都会让它为真。
    let expected: Vec<String> = if args.urls.is_empty() {
        vec!["about:blank".to_string(), "chrome://newtab".to_string()]
    } else {
        args.urls.clone()
    };
    let deadline = Instant::now() + secs(args.timeout.max(1.0));
    let mut new_keys: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        if let Ok(targets) = browser_targets_snapshot() {
            new_keys = targets
                .iter()
                .filter(|(key, url)| {
                    !before.contains(key) && expected.iter().any(|want| url_matches(url, want))
                })
                .map(|(key, _)| key.clone())
                .collect();
            if !new_keys.is_empty() {
                break;
            }
        }
        sleep(Duration::from_millis(250));
    }

    let connected = !new_keys.is_empty();
    if let Some(result) = value.get_mut("result") {
        result["connected"] = json!(connected);
        result["session_keys"] = json!(new_keys);
        result["hint"] = json!(if connected {
            "浏览器已带 --silent-debugger-extension-api 启动：之后所有 CDP 操作都不会再出现「已开始调试此浏览器」横幅"
        } else {
            "浏览器已启动，但没等到它的标签页：确认这个 profile 装了 ATB 扩展（新 profile 需要在 chrome://extensions 手动加载一次），或直接 atb tabs 复查"
        });
    }
    print_json(value);
    Ok(())
}

/// 前进/后退/刷新这类会打断执行上下文的操作：脚本先返回再触发导航，
/// 避免 daemon 白等到超时。
///
// 走统一的 [`dispatch_page`] 而不是直接 `POST /exec`：这样 `--tab @1` /
/// `--tab url:` 的目标解析、`--dry-run` 的「只解析不执行」、`--async` 的 job 提交
/// 都与其它页面命令同一条路径，不会再出现「back --tab @1 被当成 tab_id 字符串」。
/// 底层 op 仍是 `exec`——它就是一段 JS，daemon 的目标解析与 job 白名单都按 exec 走。
// 导航命令的 HTTP 预算。
///
/// 导航脚本自己只等 `min(5s)`，把 `--timeout` 原样放大成 HTTP 预算会让
/// `--timeout 600` 的 back/reload 在 daemon 已返回后还干等十分钟。取 `min(30)+5`。
fn navigation_budget(timeout: f64) -> f64 {
    timeout.min(30.0) + 5.0
}

pub(crate) fn run_navigation_command(
    op: &str,
    script: &str,
    args: TabOnlyArgs,
    is_async: bool,
) -> Result<()> {
    // 1e300 / NaN 能通过 `is_finite()` 之外的宽松校验并让 Duration 构造 panic。
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 3600.0 {
        return Err(usage_failure(
            "invalid_timeout",
            format!("{op} --timeout 必须在 0~3600 秒之间"),
            "默认 30 秒；导航类操作通常在 1 秒内完成",
        ));
    }
    let mut payload = json!({
        "script": script,
        // clone：下面还要用 args.tab/browser/profile 做 dry-run 与多目标分发。
        "switch_tab_id": args.tab.clone(),
        "browser": args.browser.clone(),
        "profile": args.profile.clone(),
        "no_monitor": true,
        // 脚本只负责触发导航后立即返回；真正的等待交给页面加载，不占 exec 的预算。
        "timeout": args.timeout.min(5.0),
    });
    // async 与同步路径必须对 `--tab @1` / `--tab url:` 给出一致结论。
    // 以前 async 直接 submit_job，本地不解析目标，同一条命令只是去掉 --async 就
    // 从「执行」变成「参数错误」/反之。现在先走同一份解析与预检。
    let spec = match TargetSpec::parse(args.tab.as_deref().unwrap_or("")) {
        Ok(spec) => spec,
        Err(err) => return usage_error(&spec_problem(err)),
    };
    if let Err(problem) = finalize_page_payload("exec", &mut payload) {
        return usage_error(&problem);
    }
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("exec", &payload, is_async)? {
        return Ok(());
    }
    if is_async {
        // 需要 daemon 解析的多目标（all/@N/url:/title:/group:）在异步路径下拿不到
        // 每条的 job 结果；与 submit_job 的拒绝语义对齐为 exit 2。
        if spec.is_multi() || spec.needs_daemon_resolution() {
            return usage_error(
                &preflight::Problem::new(
                    "unsupported_combination",
                    "--async 不能和 all / @N / url: 这类需要解析的 --tab 一起用",
                    "要么去掉 --async（同步路径会解析目标并执行），要么用明确的 tab id 逐个提交",
                )
                .with_example("atb exec --tab all 'return document.title'"),
            );
        }
        print_json(submit_job("exec", payload)?);
        return Ok(());
    }
    dispatch_page(
        "exec",
        "/exec",
        payload,
        args.tab.as_deref(),
        args.browser.as_deref(),
        args.profile.as_deref(),
        navigation_budget(args.timeout),
    )?;
    Ok(())
}

pub(crate) fn cmd_daemon(is_async: bool) -> Result<()> {
    reject_async_if_set("daemon", is_async)?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(server::run_daemon())
}

pub(crate) fn cmd_browsers(is_async: bool) -> Result<()> {
    reject_async_if_set("browsers", is_async)?;
    ensure_server()?;
    print_json(request("GET", "/browsers", None, 30.0)?);
    Ok(())
}

/// 把 open_target 的解析错误翻译成 CLI 统一的 `preflight::Problem`（exit 2）。
///
/// `open_target` 的错误是给 daemon/内部用的（带 details，没有 hint）；对外契约要求
/// `error_code` + `hint` 稳定，所以在 CLI 侧补一层。`did_you_mean` 之类的候选会拼进
/// hint，用户不用再自己猜文件名。
fn open_target_problem(err: crate::open_target::OpenTargetError) -> preflight::Problem {
    let suggestion = err
        .details
        .get("did_you_mean")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut hint = match err.code {
        "unsupported_scheme" => {
            // chrome-extension 已经支持；其余浏览器保留页面只能手动打开。
            err.details
                .get("hint")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    "只支持 http / https / file / chrome-extension 与本地文件路径；\
                     chrome:// / edge:// / about: / devtools:// 页面不能由扩展创建，请手动打开后 `atb tabs` 选择目标"
                        .to_string()
                })
        }
        "invalid_extension_url" => {
            "chrome-extension:// 必须带扩展 ID（32 位）与页面路径；其它扩展的页面会被 Chrome 拒绝"
                .to_string()
        }
        "local_resource_not_found" | "file_not_found" => {
            "确认路径存在；相对路径按当前 shell 的工作目录解析".to_string()
        }
        "foreign_platform_path" => {
            "路径语法与当前系统不符：Windows 用 C:\\...，macOS/Linux 用 /...".to_string()
        }
        "local_resource_not_file" => {
            "open 只能打开文件，不能打开目录；要打开目录请用文件管理器".to_string()
        }
        "invalid_file_url" | "unsupported_file_host" => {
            "file:// URL 必须是绝对路径且不带用户名/端口；也可以直接传本地文件路径".to_string()
        }
        _ => "atb open --help 查看支持的写法（URL / 本地文件 / file://）".to_string(),
    };
    if !suggestion.is_empty() {
        hint.push_str(&format!("；同目录下的相近文件：{}", suggestion.join(", ")));
    }
    preflight::Problem::new(err.code, err.message, hint).with_field("targets")
}

// `--wait-for` 的语法校验。
///
/// 旧实现只 `starts_with("selector:")`：`--wait-for 'selector:'` / `'text:   '`
/// 会被当成合法条件原样下发，daemon 等一个空条件直到超时，用户看到的是
/// 「打开超时」。现在只接受四类，且 `selector:`/`text:` 的值 trim 后不能为空。
fn validate_wait_for(cond: &str) -> std::result::Result<(), String> {
    let normalized = cond.trim().to_ascii_lowercase();
    // none / off / skip 表示「打开后不等待」。
    if matches!(normalized.as_str(), "none" | "off" | "skip") {
        return Ok(());
    }
    if normalized == "networkidle" || normalized == "dom-quiet" {
        return Ok(());
    }
    if let Some(value) = cond.strip_prefix("selector:") {
        if value.trim().is_empty() {
            return Err("`selector:` 后面的 CSS 选择器不能为空".to_string());
        }
        return Ok(());
    }
    if let Some(value) = cond.strip_prefix("text:") {
        if value.trim().is_empty() {
            return Err("`text:` 后面的页面文本不能为空".to_string());
        }
        return Ok(());
    }
    Err(format!("--wait-for 的值 {cond:?} 不合法"))
}

pub(crate) fn cmd_open(args: OpenArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("open", is_async)?;
    // 1e300 能通过 `is_finite` 与 `> 0`，但 `Duration::from_secs_f64` 会 panic。
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 3600.0 {
        return Err(usage_failure(
            "invalid_timeout",
            "open --timeout 必须在 0~3600 秒之间",
            "例如 --timeout 30；页面很慢时可以调到 120",
        ));
    }
    // --challenge-timeout 必须和 --timeout 同口径校验：它既被加进 HTTP 超时
    // 预算，又原样下发给 daemon；nan / 负数 / 1e308 会让等待预算变成无意义的值，
    // 用户却只看到「open 超时」。0 是合法值（= 不等待，等同 --challenge off）。
    if !args.challenge_timeout.is_finite()
        || args.challenge_timeout < 0.0
        || args.challenge_timeout > 3600.0
    {
        return Err(usage_failure(
            "invalid_argument",
            "--challenge-timeout 必须是 0~3600 之间的有限秒数",
            "0 表示不等待（等同 --challenge off）；例如 --challenge-timeout 20",
        ));
    }
    for target in &args.targets {
        if let Err(problem) = preflight::validate_open_target(target) {
            return usage_error(&problem);
        }
    }
    // --wait-for 的语法在这里拦住（exit 2）：旧链路会把拼错的条件原样发给 daemon，
    // 页面里执行时才报错，用户看到的是「打开超时」，根本想不到是条件写错了。
    if let Some(cond) = args.wait_for.as_deref() {
        if let Err(reason) = validate_wait_for(cond) {
            return Err(usage_failure(
                "invalid_argument",
                format!("--wait-for {reason}"),
                "支持 none / off / skip（不等待）、networkidle / dom-quiet / selector:<CSS选择器> / text:<页面文本>，例如 --wait-for 'selector:.result'",
            ));
        }
    }
    // `--wait-for-timeout` 是独立预算，必须与 --timeout 同口径校验。
    if let Some(timeout) = args.wait_for_timeout {
        if !timeout.is_finite() || timeout < 0.5 || timeout > 3600.0 {
            return Err(usage_failure(
                "invalid_argument",
                "--wait-for-timeout 必须是 0.5~3600 之间的有限秒数",
                "普通页面的默认预算跟随 --timeout；chrome:// / chrome-extension:// 等特殊页面默认 3s（ATB_OPEN_SPECIAL_WAIT_SEC）",
            ));
        }
    }
    if DRY_RUN.load(Ordering::Relaxed) {
        // 真实执行返回 `{ok, result:{...}}`；dry-run 也放进 `result`，
        // 调用方用同一套 `jq .result` 就能解析，不必按 dry_run 分支。
        //
        // 必须调用真正的 open_target 解析：只跑 preflight 时
        // `atb open 'ftp://x' --dry-run` / 缺失文件都会返回 ok:true（假绿），
        // 去掉 --dry-run 才发现打不开。这里把「解析结果」本身作为计划输出。
        let cwd = env::current_dir().context("无法读取当前工作目录")?;
        let mut resolved: Vec<Value> = Vec::new();
        for target in &args.targets {
            match crate::open_target::resolve_open_target(target, Some(&cwd)) {
                Ok(item) => resolved.push(json!({
                    "input": target,
                    "navigation_url": item.navigation_url,
                    "kind": match item.kind {
                        crate::open_target::TargetKind::Web => "web",
                        crate::open_target::TargetKind::LocalResource => "local_resource",
                    },
                })),
                Err(err) => return usage_error(&open_target_problem(err)),
            }
        }
        // dry-run 的 `result.targets` 必须与真实 `/open` 回包同形
        // （`targets[]{url, tab_id}`）。以前这里回的是原始输入字符串数组，
        // 调用方按 dry-run 校准字段后再真跑必然解析失败。原始输入另放 `inputs`。
        let planned_targets: Vec<Value> = resolved
            .iter()
            .map(|item| {
                json!({
                    "url": item.get("navigation_url").cloned().unwrap_or(Value::Null),
                    "tab_id": Value::Null,
                })
            })
            .collect();
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "open",
                "count": planned_targets.len(),
                "targets": planned_targets,
                "inputs": args.targets,
                "resolved": resolved,
                "active": !args.background,
                "note": "目标已解析通过；去掉 --dry-run 即执行。多个目标由 daemon 并发打开。",
            },
        }));
        return Ok(());
    }
    ensure_server()?;
    let cwd = env::current_dir().context("无法读取当前工作目录")?;
    // 挑战等待是 open 的一部分：HTTP 超时必须把它算进去，否则服务端还在等，
    // CLI 已经断开并报超时（用户只看到「open 超时」，其实是挑战没放行）。
    let challenge_budget = if args.challenge == "off" {
        0.0
    } else {
        args.challenge_timeout.max(0.0)
    };
    // `--wait-for` 的等待预算独立成字段（普通页面默认 = --timeout），
    // CLI 与服务端按同一字段计算；以前只有 CLI 侧用 args.timeout 估算。
    //
    // `none/off/skip` 不产生等待预算；特殊页面（chrome:// /
    // chrome-extension:// / about: / edge:// / devtools://）未显式给预算时用
    // ATB_OPEN_SPECIAL_WAIT_SEC，而不是 --timeout 的默认 10s。
    let wait_cond = args
        .wait_for
        .as_deref()
        .map(str::trim)
        .filter(|cond| !cond.is_empty());
    let skip_wait = wait_cond
        .is_some_and(|cond| matches!(cond.to_ascii_lowercase().as_str(), "none" | "off" | "skip"));
    let special_target = args
        .targets
        .iter()
        .any(|target| crate::open_target::is_special_page(target.trim()));
    let wait_budget = if skip_wait {
        0.0
    } else if let Some(timeout) = args.wait_for_timeout {
        timeout
    } else if special_target {
        crate::settings::open_special_wait_sec()
    } else {
        args.timeout.max(0.0)
    };
    let mut request_timeout = http_budget(args.timeout + challenge_budget + 10.0).max(30.0);
    if wait_cond.is_some() && !skip_wait {
        request_timeout = (request_timeout + wait_budget).max(30.0);
    }
    let payload = json!({
        // 目标统一走 targets 数组：daemon 内部并发，一个请求开完所有标签页。
        "targets": args.targets,
        "cwd": cwd,
        "active": !args.background,
        "window": args.window,
        "allow_focus": args.focus,
        "keep_alive": true,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "session": args.session,
        "group_title": args.group_title,
        "readiness_timeout": args.timeout,
        "challenge": args.challenge,
        "challenge_timeout": args.challenge_timeout,
        // wait_for / wait_for_timeout 已并入 OpenRequest（原先的 flatten wrapper
        // 与 deny_unknown_fields 互斥， 收口时已合并）；旧字段不动，
        // 旧版 daemon 收到多余字段会忽略，天然向后兼容。
        "wait_for": args.wait_for,
        // 只有显式给了才下发；缺省由 daemon 决定（普通页面 = readiness_timeout，
        // 特殊页面 = ATB_OPEN_SPECIAL_WAIT_SEC）。
        "wait_for_timeout": args.wait_for_timeout,
        "warm": args.warm,
    });
    print_json(request("POST", "/open", Some(payload), request_timeout)?);
    Ok(())
}

pub(crate) fn cmd_cleanup(args: CleanupArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("cleanup", is_async)?;
    ensure_server()?;
    let payload = json!({
        "agent": agent_id(),
        "all_agents": args.all_agents,
        "dry_run": args.dry_run || DRY_RUN.load(Ordering::Relaxed),
    });
    print_json(request("POST", "/cleanup", Some(payload), 60.0)?);
    Ok(())
}

/// `close` 的一个待关闭目标：解析当时的 id，以及当时看到的 url/title 快照。
///
/// url/title 有两个用途：
/// - 随 `/close` 作为 `expect_url`/`expect_title` 下发（旧 daemon 不认识的键会被忽略，
///   向后兼容），将来 daemon 可以据此拒绝「解析后页面已经换了内容」的关闭；
/// - 回显在输出里，和 `resolved_at` 一起让调用方事后能核对「当时关的是哪一页」。
struct CloseTarget {
    id: String,
    url: Option<String>,
    title: Option<String>,
}

/// 把解析结果渲染成输出用的快照数组（字段名与 `/multi` 的 targets 保持一致）。
fn close_targets_snapshot(targets: &[CloseTarget]) -> Value {
    json!(targets
        .iter()
        .map(|target| json!({
            "tab_id": target.id,
            "url": target.url,
            "title": target.title,
        }))
        .collect::<Vec<_>>())
}

pub(crate) fn cmd_close(args: CloseArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("close", is_async)?;
    // 空 `--tab` 以前会原样下发 `/close`，由 daemon 关掉「当前标签页」——
    // 破坏性命令静默命中默认目标是不可接受的。close 必须给出明确目标。
    if args.tab.trim().is_empty() {
        return usage_error(
            &preflight::Problem::new(
                "target_not_found",
                "close 需要明确的 --tab（不能为空）",
                "先用 `atb tabs --ids` 看现有标签，再写 `--tab <tab_id>` / `--tab @1`；按组关闭用 `--tab group:<标题> --yes`",
            )
            .with_field("tab"),
        );
    }
    let spec = match TargetSpec::parse(&args.tab) {
        Ok(spec) => spec,
        Err(err) => return usage_error(&spec_problem(err)),
    };
    // `all` / `url:` / `title:` 仍然拒绝：url:/title: 命中范围是「模糊子串」，
    // 关错了没法撤回。group: 原来也一并拒绝（unsupported_combination），现在改为
    // 显式确认形态 `close --tab group:<标题> --yes`——先解析出将要关闭的标签数，
    // 不带 --yes 就报错让调用方确认，防手滑。
    if matches!(
        spec,
        TargetSpec::All | TargetSpec::Url(_) | TargetSpec::Title(_)
    ) {
        return usage_error(
            &preflight::Problem::new(
                "unsupported_combination",
                "close 只接受明确的 tab id 或列表；批量形态只支持 --tab group:<标题> --yes，不接受 all / url: / title:",
                "先用 `tabs --ids` 看清要关哪些，再写 `--tab 111,222`；按组关闭用 `--tab group:<标题> --yes`",
            )
            .with_field("tab"),
        );
    }
    // 逐条 CloseTarget：序号/组/窗口/当前标签都要先换算成具体 id，
    // 换算时顺手记下 url/title（TOCTOU 快照）。
    let mut targets_list: Vec<CloseTarget> = Vec::new();
    let mut group_close = false;
    // `resolved_at` 只在「经过 daemon 换算」时有值：它是快照的时间锚点。
    let mut resolved_at: Option<f64> = None;
    match &spec {
        TargetSpec::Ids(ids) => {
            targets_list = ids
                .iter()
                .map(|id| CloseTarget {
                    id: id.clone(),
                    url: None,
                    title: None,
                })
                .collect();
        }
        TargetSpec::Index(_)
        | TargetSpec::Group(_)
        | TargetSpec::Window(_)
        | TargetSpec::Latest
        | TargetSpec::Current => {
            // 序号 / 标签组 / 窗口 / 当前标签都要 daemon 解析：先拿一次列表按同样顺序
            // 换算成 id。`window:` / `current` 以前被原样透传成 tab_id（`close --tab
            // current` 等于拿字符串 "current" 去关），必须走同一条换算路径。
            ensure_server()?;
            let value = request(
                "POST",
                "/multi",
                Some(json!({
                    "op": "exec",
                    "args": { "script": "return 1" },
                    "target": args.tab,
                    "dry_run": true,
                })),
                30.0,
            )?;
            // 换算失败（扩展过旧 / 目标语法不认识）交给原来的打印路径：那里有
            // error_code/hint，不能被下面的 target_not_found 覆盖掉。
            if value.get("ok").and_then(Value::as_bool) == Some(false) {
                print_json(value);
                return Ok(());
            }
            resolved_at = Some(server::epoch_secs());
            if let Some(items) = value.get("targets").and_then(Value::as_array) {
                for item in items {
                    let id = item
                        .get("tab_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let url = item
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    targets_list.push(CloseTarget {
                        id,
                        url: (!url.is_empty()).then_some(url),
                        title: (!title.is_empty()).then_some(title),
                    });
                }
            }
            if targets_list.is_empty() {
                // 换算出来 0 个目标 = 目标写错了：以前会打印 daemon 的
                // `{ok:true,dry_run:true,count:0}` 并以 0 退出，是个假绿。
                return usage_error(
                    &preflight::Problem::new(
                        "target_not_found",
                        format!("close: --tab {} 没有匹配到任何标签页", args.tab),
                        "先用 `atb tabs --ids` 看现有标签；序号从 1 开始，window:/current 需要扩展 3.5+",
                    )
                    .with_field("tab"),
                );
            }
            group_close = matches!(spec, TargetSpec::Group(_));
            if group_close && !args.yes {
                // 关闭前先报数：智能体看到「将关闭 N 个」才知道影响面，加 --yes 才放行。
                return Err(usage_failure(
                    "unsupported_combination",
                    format!(
                        "close --tab {} 将关闭 {} 个标签页，必须显式加 --yes 确认",
                        args.tab,
                        targets_list.len()
                    ),
                    "确认无误后执行 close --tab 'group:<标题>' --yes；先看清单可加 --dry-run",
                ));
            }
        }
        // 走到这里只剩 Default（没写 --tab / 空字符串）：沿用旧行为透传。
        _ => {
            targets_list = vec![CloseTarget {
                id: args.tab.clone(),
                url: None,
                title: None,
            }];
        }
    }
    // `close --dry-run` 以前真的会关标签页：先解析目标，再只输出计划。
    if DRY_RUN.load(Ordering::Relaxed) {
        let mut plan = json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "op": "close",
                "count": targets_list.len(),
                "tabs": targets_list.iter().map(|target| target.id.clone()).collect::<Vec<_>>(),
                "hint": "去掉 --dry-run 才会真正关闭；单标签时返回结构与本结果不同（直接是 /close 的原始返回）",
            },
        });
        // 经过 daemon 换算的目标都带上快照与时间锚点：智能体核对「关的是不是这批」
        // 不用再跑一次 tabs，也能对上关闭时真正发生的跳转。
        if let Some(at) = resolved_at {
            plan["result"]["resolved_at"] = json!(at);
            plan["result"]["targets"] = close_targets_snapshot(&targets_list);
        }
        print_json(plan);
        return Ok(());
    }
    ensure_server()?;
    let mut results = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    let mut failed = 0usize;
    for target in &targets_list {
        let mut payload = json!({
            "tab_id": target.id,
            "browser": args.browser,
            "profile": args.profile,
        });
        // 解析快照随请求下发：旧 daemon 不认识这两个键会直接忽略（CloseRequest 没有
        // deny_unknown_fields），新 daemon 可以据此做「页面对不上就不关」的二次校验。
        if let Some(url) = &target.url {
            payload["expect_url"] = json!(url);
        }
        if let Some(title) = &target.title {
            payload["expect_title"] = json!(title);
        }
        // 单条 `/close` 失败（连接断、daemon 重启）以前用 `?` 提前返回。
        // 此时前面的标签已经关掉、后面的还没处理，部分结果全丢，调用方既看不到
        // 已关的也拿不到失败项。现在逐条降级为 `ok:false` 记录后继续。
        let value = request("POST", "/close", Some(payload), http_budget(args.timeout))
            .unwrap_or_else(|err| {
                // 保留 tab/tab_id：单目标路径下这里会原样作为最终输出，调用方要能对上是哪一页。
                json!({
                    "ok": false,
                    "tab": target.id,
                    "tab_id": target.id,
                    "error": err.to_string(),
                    "error_code": "unexpected_error",
                })
            });
        let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            failed += 1;
        }
        // tab_id/title 是新增字段：group 关闭时智能体要逐条核对；普通 close 多带
        // 这两个字段是纯增量，不破坏已有脚本（旧字段 tab 保留不动）。url 同理。
        results.push(json!({
            "tab": target.id,
            "tab_id": target.id,
            "title": target.title,
            "url": target.url,
            "ok": ok,
            "result": value.get("result").cloned(),
            "error": value.get("error").cloned(),
        }));
        raw.push(value);
    }
    if results.len() == 1 && !group_close {
        // 单标签保持旧输出（就是 /close 的原始返回），避免破坏已有脚本。
        let mut out = raw.into_iter().next().unwrap_or_else(|| json!({}));
        // 换算出来的目标额外补时间锚点 + 快照：不新增顶层结构，只往同一对象里加键。
        if let (Some(map), Some(at)) = (out.as_object_mut(), resolved_at) {
            map.insert("resolved_at".to_string(), json!(at));
            map.insert(
                "target_snapshot".to_string(),
                close_targets_snapshot(&targets_list),
            );
        }
        print_json(out);
    } else {
        let mut out =
            json!({ "ok": failed == 0, "count": results.len(), "failed": failed, "tabs": results });
        if group_close {
            out["closed_by"] = json!(args.tab);
        }
        if let Some(at) = resolved_at {
            out["resolved_at"] = json!(at);
            out["target_snapshot"] = close_targets_snapshot(&targets_list);
        }
        print_json(out);
    }
    Ok(())
}

/// 「关掉当前 daemon 并确认端口已释放」的结构化失败。
///
/// 与 `restart` 共用，避免「关没关掉」在两个命令里各有一套判定。`code` 会原样进
/// `error_code`：`daemon_token_mismatch` 指向「换 token / 强杀」，`shutdown_failed`
/// 指向「端口没释放」——两者的救援动作完全不同，不能合并成一句。
pub(crate) struct ShutdownFailure {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) hint: String,
}

impl ShutdownFailure {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "error_code": self.code,
            "error": self.message,
            "hint": self.hint,
        })
    }
}

/// 请求 daemon 退出并**确认**它真的停了（API 端口 + 插件端口都释放）。
///
/// 以前两处调用都是 `let _ = request("/shutdown"); wait_server_stopped(...)`：
/// 401 被吞、`false` 返回值被丢，于是「没停下来」被当成成功。这里把两种
/// 失败分开：token 不匹配给 `daemon_token_mismatch`（可 `restart --force` 自救），
/// 其余给 `shutdown_failed`。
fn stop_daemon_confirmed() -> std::result::Result<(), ShutdownFailure> {
    let reply = request("POST", "/shutdown", Some(json!({})), 3.0);
    if wait_server_stopped(Duration::from_secs(5)) {
        return Ok(());
    }
    let token_rejected = matches!(
        reply.as_ref().err().and_then(http_failure_code),
        Some("invalid_api_token")
    );
    if token_rejected || daemon_token_mismatch() {
        return Err(ShutdownFailure {
            code: "daemon_token_mismatch",
            message: format!(
                "{PORT} 上的 daemon 不认当前 API token，/shutdown 被拒绝，它仍在运行（新配置尚未生效）"
            ),
            hint: format!(
                "结束占用 {PORT} 的进程后重试：Windows `netstat -ano | findstr :{PORT}` 再 `taskkill /PID <pid> /F`；\
                 或直接 `atb restart --force`（自动核对进程身份后强杀）"
            ),
        });
    }
    let detail = match &reply {
        Ok(_) => "daemon 已回包，但端口仍在监听".to_string(),
        Err(err) => err.to_string(),
    };
    Err(ShutdownFailure {
        code: "shutdown_failed",
        message: format!("关闭 daemon 未确认完成：{detail}"),
        hint: format!(
            "稍后重试；若 {PORT} 被别的程序占着，先结束占用进程（`netstat -ano | findstr :{PORT}`），\
             或用 `atb restart --force` 让本工具核对身份后处理"
        ),
    })
}

pub(crate) fn cmd_set_extension_port(args: SetExtensionPortArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("set-extension-port", is_async)?;
    // 0 和 18767（API 端口）都不是合法插件端口：以前直接下发 config::set_extension_port，
    // 由它报错 → 落成「执行失败」（exit 1），调用方分不清是命令写错了还是环境坏了。
    // 用法问题必须在 CLI 侧就给出 exit 2。u16 已保证上限不超过 65535。
    if args.port == 0 || args.port == PORT {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "扩展端口 {} 不可用：端口 0 与 API 端口 {PORT} 都不能作为插件端口",
                args.port
            ),
            "换一个 1~65535 之间且未被占用的端口，例如 atb set-extension-port 18768",
        ));
    }
    // `--dry-run` 以前会真的改写配置并重启 daemon。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "result": {
            "dry_run": true,
            "op": "set-extension-port",
            "to": args.port,
            // 只读探测：dry-run 不碰配置文件，所以不回显当前端口（读它可能创建文件）。
            "restart_required": is_server_alive(),
            },
        }));
        return Ok(());
    }
    let was_running = is_server_alive();
    let config = config::set_extension_port(args.port)?;
    if !was_running {
        // 没在跑就没有「重启」可言：新端口在下次 daemon 启动时生效。
        print_json(json!({
            "ok": true,
            "extension_port": config.extension_port,
            "restarted": false,
            "note": "daemon 未运行，新端口在下次启动时生效",
        }));
        return Ok(());
    }
    // 关闭必须**确认**完成。以前 `/shutdown` 的 401 被 `let _ =` 吞掉、
    // `wait_server_stopped` 的 false 被丢弃，于是输出 `restarted:true`，而实际生效的
    // 还是旧 daemon 的旧端口。
    if let Err(failure) = stop_daemon_confirmed() {
        let mut value = failure.to_json();
        // 配置已经写盘（`config::set_extension_port` 在关闭之前完成）：说清楚
        // 「文件改了、进程没换」，调用方才知道下一步该动进程而不是再改一次配置。
        value["extension_port"] = json!(config.extension_port);
        value["config_written"] = json!(true);
        value["restarted"] = json!(false);
        print_json(value);
        return Ok(());
    }
    ensure_server()?;
    print_json(json!({
        "ok": true,
        "extension_port": config.extension_port,
        "restarted": true
    }));
    Ok(())
}

pub(crate) fn cmd_stop(is_async: bool) -> Result<()> {
    reject_async_if_set("stop", is_async)?;
    // `stop --dry-run` 以前会真的把 daemon 停掉：停 daemon 是全局副作用，
    // 必须只输出计划。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "stop",
                "running": is_server_alive(),
                "would": ["POST /shutdown（让 18767 上的 daemon 退出）"],
                "note": "去掉 --dry-run 才会真正停止 daemon；正在执行的后台任务会随之中断",
            },
        }));
        return Ok(());
    }
    match request("POST", "/shutdown", Some(json!({})), 3.0) {
        Ok(value) => print_json(value),
        // 端口还在监听却连不上/被拒 = 有一个用别的 token 的残留 daemon。
        // 这种情况必须报错：报 not_running 会让人以为已经停了，后续命令继续踩坑。
        Err(err) if is_server_alive() => print_json(json!({
            "ok": false,
            "status": "shutdown_failed",
            "error": err.to_string(),
            "hint": "18767 端口上还有一个 daemon 不认当前 token，它没被停掉。请先结束占用 18767 的进程（Windows: netstat -ano | findstr 18767 后 taskkill /PID <pid> /F）"
        })),
        Err(_) => print_json(json!({ "ok": true, "status": "not_running" })),
    }
    Ok(())
}

/// 探测「18767 有 daemon 在跑，但它不认本地 API token」。
///
/// 复用 daemon 的鉴权语义（boot.rs `validate_api_access`）：`/health` 带 token 请求
/// 返回 401 + `error_code:"invalid_api_token"` 即为不匹配。连不上、非 JSON 或其它
/// 错误一律视为「证据不足」，不做强杀判定——宁可让既有报错路径处理，也不能把
/// 正常 daemon 误判成残留进程。
pub(crate) fn daemon_token_mismatch() -> bool {
    let Ok(client) = http_client() else {
        return false;
    };
    let mut builder = client
        .get(format!("http://{HOST}:{PORT}/health"))
        .timeout(Duration::from_secs(2));
    // 不走 api_token() 的进程内缓存负向缓存问题也无所谓：这里只关心「拿得到的
    // token 是否被 daemon 接受」，拿不到 token 时 daemon 端的 401 同样说明不匹配。
    if let Some(token) = api_token() {
        builder = builder.header(config::API_TOKEN_HEADER, token);
    }
    builder
        .send()
        .ok()
        .and_then(|response| response.json::<Value>().ok())
        .and_then(|value| {
            value
                .get("error_code")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some("invalid_api_token")
}

/// 一次强杀尝试的结果。字段名与对外输出一致（`killed_pid` / `identity` /
/// `skipped_reason`），Agent 不必解析中文就能判断「到底杀没杀、为什么没杀」。
pub(crate) struct KillOutcome {
    pub(crate) pid: u32,
    pub(crate) identity: Option<String>,
    pub(crate) killed: bool,
    pub(crate) error: Option<String>,
    pub(crate) skipped_reason: Option<String>,
}

impl KillOutcome {
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({
            "killed_pid": if self.killed { json!(self.pid) } else { Value::Null },
            "pid": self.pid,
            "identity": self.identity.clone(),
            "killed": self.killed,
        });
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        if let Some(reason) = &self.skipped_reason {
            value["skipped_reason"] = json!(reason);
        }
        value
    }
}

/// 读进程身份（可执行路径 + 命令行）。
///
/// 只用于「强杀前确认这是自己的 atb daemon」。拿不到身份时返回 `None`，调用方必须
/// **跳过**而不是照样杀：18767 可能被用户自己的其它程序占着，杀错进程的代价远高于
/// 让用户手工处理。
#[cfg(windows)]
pub(crate) fn process_identity(pid: u32) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 一次调用同时拿可执行路径与命令行：受保护进程可能只有其中一项有值，
    // 两项都拿不到才认为「无法确认身份」。
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\"; \
         if ($p) {{ 'EXE=' + $p.ExecutablePath; 'CMD=' + $p.CommandLine }}"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut exe = String::new();
    let mut cmdline = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("EXE=") {
            exe = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("CMD=") {
            cmdline = value.trim().to_string();
        }
    }
    if exe.is_empty() && cmdline.is_empty() {
        return None;
    }
    Some(format!("exe={exe} cmdline={cmdline}").trim().to_string())
}

/// 非 Windows：优先读 `/proc/<pid>/{exe,cmdline}`（Linux），
/// 没有 /proc 时退回 `ps -o command= -p <pid>`（macOS / 其它 Unix）。
#[cfg(not(windows))]
pub(crate) fn process_identity(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.display().to_string());
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .replace('\0', " ")
                .trim()
                .to_string()
        });
    if exe.is_some() || cmdline.as_deref().is_some_and(|text| !text.is_empty()) {
        return Some(
            format!(
                "exe={} cmdline={}",
                exe.unwrap_or_default(),
                cmdline.unwrap_or_default()
            )
            .trim()
            .to_string(),
        );
    }
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(format!("cmdline={text}"))
}

/// `--force-assume-atb strict|loose` 的等价环境开关（`ATB_FORCE_ASSUME_ATB`）。
///
/// strict（默认）：进程名必须等于当前可执行文件名，且参数含独立词 `daemon`；
/// loose：沿用旧的「路径含 exe / 同时含 atb+daemon」宽松判据（自动化救援用）。
pub(crate) fn force_assume_atb() -> String {
    crate::settings::force_assume_atb()
}

/// 从 `process_identity` 的输出里拆出 (exe, cmdline)。
fn identity_parts(identity: &str) -> (String, String) {
    let exe = identity
        .split("exe=")
        .nth(1)
        .map(|rest| rest.split(" cmdline=").next().unwrap_or(rest))
        .unwrap_or("")
        .trim()
        .to_string();
    let cmdline = identity
        .split("cmdline=")
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string();
    (exe, cmdline)
}

/// 身份是否确实是「本机的 atb daemon」。
///
// 旧兜底是「命令行同时含 atb 与 daemon」的纯子串匹配，`/opt/atb-monitor/daemon`、
/// `my-atb-daemon-watchdog`、`catb-daemon` 都会被判成自己的 daemon 并强杀。
/// strict（默认）收紧为「进程名 == current_exe().file_name() 且参数含独立词 daemon」；
/// loose 保留旧判据，供自动化救援场景显式开启。
pub(crate) fn identity_is_atb_daemon(identity: &str) -> bool {
    let (exe, cmdline) = identity_parts(identity);
    if force_assume_atb() == "loose" {
        let lower = identity.to_ascii_lowercase();
        if let Ok(current) = env::current_exe() {
            let current = current.to_string_lossy().to_ascii_lowercase();
            if !current.is_empty() && lower.contains(&current) {
                return true;
            }
        }
        return lower.contains("atb") && lower.contains("daemon");
    }
    let Some(expected) = env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
    }) else {
        return false;
    };
    let raw_name = if !exe.is_empty() {
        exe
    } else {
        cmdline
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .to_string()
    };
    let process_name = Path::new(&raw_name)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if process_name != expected {
        return false;
    }
    cmdline.split_whitespace().any(|word| {
        word.trim_matches(|ch| ch == '"' || ch == '\'')
            .eq_ignore_ascii_case("daemon")
    })
}

#[cfg(windows)]
fn force_kill_pid(pid: u32) -> std::result::Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| format!("执行 taskkill /PID {pid} /F 失败: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "taskkill /PID {pid} /F 退出码 {:?}（可能权限不足）",
            output.status.code()
        ));
    }
    // taskkill 在「进程已经不存在」的某些语言/版本下仍返回 0，只在 stderr 里写
    // 「没有运行的任务」。不认这一条就会把「没杀到」报成 killed。
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
    .to_lowercase();
    if text.contains("没有运行的任务")
        || text.contains("not found")
        || text.contains("no running instance")
    {
        return Err("进程已不存在（taskkill 未找到任务）".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn force_kill_pid(pid: u32) -> std::result::Result<(), String> {
    let output = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map_err(|err| format!("执行 kill -9 {pid} 失败: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "kill -9 {pid} 退出码 {:?}（可能权限不足）",
            output.status.code()
        ));
    }
    Ok(())
}

/// 核对身份后强杀单个 PID。身份不明或不像 atb daemon 时**不杀**，返回 skipped_reason。
pub(crate) fn kill_verified_pid(pid: u32) -> KillOutcome {
    let Some(identity) = process_identity(pid) else {
        return KillOutcome {
            pid,
            identity: None,
            killed: false,
            error: None,
            skipped_reason: Some(
                "读不到该进程的可执行路径/命令行，无法确认是 atb daemon，已跳过（避免误杀）"
                    .to_string(),
            ),
        };
    };
    if !identity_is_atb_daemon(&identity) {
        return KillOutcome {
            pid,
            identity: Some(identity),
            killed: false,
            error: None,
            skipped_reason: Some("进程身份不是 atb daemon，已跳过（避免误杀其它程序）".to_string()),
        };
    }
    match force_kill_pid(pid) {
        Ok(()) => KillOutcome {
            pid,
            identity: Some(identity),
            killed: true,
            error: None,
            skipped_reason: None,
        },
        Err(err) => KillOutcome {
            pid,
            identity: Some(identity),
            killed: false,
            error: Some(err),
            skipped_reason: None,
        },
    }
}

/// 逐个核对身份并强杀；一个都没杀掉时给出结构化原因（不能静默返回成功）。
fn finish_force_kill(pids: Vec<u32>, source: &str) -> Result<Vec<KillOutcome>> {
    if pids.is_empty() {
        return Err(anyhow!(
            "未通过 {source} 找到监听 {PORT} 的进程；端口可能已被释放或占用方式不同，请手工核查"
        ));
    }
    let outcomes: Vec<KillOutcome> = pids.iter().map(|pid| kill_verified_pid(*pid)).collect();
    if !outcomes.iter().any(|outcome| outcome.killed) {
        // 一个都没杀时必须把待处理的 pid + identity 原样交回，用户才能
        // 手工核对/处理；只给一句「身份不符」等于把问题藏起来。
        let details = outcomes
            .iter()
            .map(|outcome| {
                let reason = outcome
                    .skipped_reason
                    .clone()
                    .or_else(|| outcome.error.clone())
                    .unwrap_or_else(|| "未知原因".to_string());
                format!(
                    "pid={} identity={} 原因={reason}",
                    outcome.pid,
                    outcome
                        .identity
                        .clone()
                        .unwrap_or_else(|| "<无法读取>".to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        return Err(anyhow!(
            "占用 {PORT} 的进程没有通过身份核对，已放弃强杀（避免误杀其它程序）：{details}。\
             若确认这些 pid 就是本机 atb daemon，可设 ATB_FORCE_ASSUME_ATB=loose 后重试"
        ));
    }
    Ok(outcomes)
}

/// `--force` 路径：定位占用 API 端口（18767）且处于 LISTENING 状态的 PID 并强杀。
///
/// 旧实现只会在报错里让人手工执行 netstat/taskkill，而报错的对象往往是自动化流程
/// 本身，等于自锁（实测反馈）；这里替调用方执行同一条命令。强杀是破坏性动作，
/// 所以只认 LISTENING 行（不碰临时连接），并过滤掉自己的 PID 防误杀。
#[cfg(windows)]
fn kill_api_port_listener() -> Result<Vec<KillOutcome>> {
    let output = Command::new("netstat")
        .arg("-ano")
        .output()
        .context("执行 netstat -ano 失败，无法定位占用 18767 的进程")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let local_suffix = format!(":{PORT}");
    let mut pids: Vec<u32> = Vec::new();
    for line in text.lines() {
        // 行格式：`  TCP    127.0.0.1:18767    0.0.0.0:0    LISTENING    1234`。
        // 只认 TCP + LISTENING + 本地地址恰以 :18767 结尾，避免误杀其它状态/端口的行。
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 5
            || !tokens[0].eq_ignore_ascii_case("TCP")
            || !tokens[3].eq_ignore_ascii_case("LISTENING")
            || !tokens[1].ends_with(&local_suffix)
        {
            continue;
        }
        let Some(pid) = tokens[4].parse::<u32>().ok() else {
            continue;
        };
        // 防误杀：CLI 进程自己不可能 LISTENING 18767，但防御性过滤仍保留。
        if pid == std::process::id() {
            continue;
        }
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return Err(anyhow!(
            "未在 netstat -ano 中找到监听 {PORT} 的进程；端口可能已被其它程序释放或占用方式不同，请手工核查"
        ));
    }
    // 强杀前逐个核对身份：18767 也可能被用户自己的程序占着。
    finish_force_kill(pids, "netstat -ano")
}

/// 非 Windows 的 `--force` 路径：`lsof -t -i:18767` 拿 PID 后 `kill -9`。
#[cfg(not(windows))]
fn kill_api_port_listener() -> Result<Vec<KillOutcome>> {
    let output = Command::new("lsof")
        .args(["-t", &format!("-i:{PORT}")])
        .output()
        .context("执行 lsof 失败，无法定位占用 18767 的进程")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pids: Vec<u32> = Vec::new();
    for token in text.split_whitespace() {
        let Ok(pid) = token.parse::<u32>() else {
            continue;
        };
        // 防误杀：CLI 进程自己不可能 LISTENING 18767，但防御性过滤仍保留。
        if pid == std::process::id() || pids.contains(&pid) {
            continue;
        }
        pids.push(pid);
    }
    if pids.is_empty() {
        return Err(anyhow!("未通过 lsof 找到占用 {PORT} 的进程，请手工核查"));
    }
    // 同 Windows：核对身份后才 kill。
    finish_force_kill(pids, "lsof")
}

pub(crate) fn cmd_restart(args: RestartArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("restart", is_async)?;
    // `restart --dry-run` 以前会真的先 /shutdown 再拉起新 daemon。
    // 停 daemon 会打断所有正在跑的任务，必须只出计划；且不探测 /health（纯本地）。
    if DRY_RUN.load(Ordering::Relaxed) {
        let mut would = vec![
            "POST /shutdown（尽力优雅关闭当前 daemon）",
            "等待 18767 与插件端口释放",
            "拉起新 daemon 并 GET /health 校验",
        ];
        if args.force {
            would.insert(
                0,
                "若 API token 不匹配：定位占用 18767 的进程、核对身份后强杀",
            );
        }
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "restart",
                "force": args.force,
                "running": is_server_alive(),
                "would": would,
                "note": "去掉 --dry-run 才会真正重启；正在执行的后台任务会随旧 daemon 中断",
            },
        }));
        return Ok(());
    }
    // 旧实现（自锁，P0 实测反馈）：daemon 活着但 token 不匹配时，/shutdown 被 401
    // 拒绝并被 `let _` 吞掉，旧 daemon 继续占着 18767；随后带 token 的 /health 也以
    // 「结束占用 18767 的进程后重试」失败——可 restart 本身就是用来解这个局的，
    // 让调用方「重试 restart」是死循环。现在先显式探测这种状态：
    // - 默认：打印将执行的命令并以退出码 2 结束（结构化 JSON，Agent 可识别）；
    // - --force：替调用方定位并强杀占用 API 端口的进程，再照常重新拉起。
    let mut force_report: Option<Value> = None;
    if daemon_token_mismatch() {
        match force_if_running(&args)? {
            Some(report) => force_report = Some(report),
            // 不带 --force：先给出可执行的救援路径，再以退出码 2 结束。
            None => {
                return Err(usage_failure(
                    "daemon_token_mismatch",
                    "18767 上的 daemon 不认当前 API token（通常是残留的旧 daemon 或别的 atb 安装占用端口）",
                    format!(
                        "结束占用 {PORT} 的进程后重试；或直接执行 `atb restart --force`（自动完成，将运行）：\n  \
                         Windows: netstat -ano | findstr :{PORT} 然后 taskkill /PID <pid> /F\n  \
                         macOS/Linux: lsof -t -i:{PORT} 然后 kill -9 <pid>"
                    ),
                ))
            }
        }
    }
    // 关闭确认与 set-extension-port 共用同一原语。以前这里同样是
    // `let _ = /shutdown` + 丢弃 `wait_server_stopped` 的结果：端口没释放时
    // `ensure_server()` 会撞上「端口被占用但对 /ping 无响应」，报错指向环境而不是
    // 「这次关闭没成功」。
    if let Err(failure) = stop_daemon_confirmed() {
        print_json(failure.to_json());
        return Ok(());
    }
    ensure_server()?;
    let mut value = request("GET", "/health", None, 3.0)?;
    if let Some(report) = force_report {
        // 把强杀过程（killed_pid / identity / skipped_reason）并入最终输出：
        // 调用方不必去翻 stderr 才能知道到底杀了谁。
        value["force_kill"] = report;
    }
    print_json(value);
    Ok(())
}

/// `--force` 分支的执行体，单独拆出来让 cmd_restart 的主流程保持线性。
///
/// 返回 `Ok(None)`：没带 `--force`，由调用方给出救援指引；
/// 返回 `Ok(Some(report))`：已按身份核对强杀（report 含 killed_pid/identity/skipped_reason）；
/// 返回 `Err`：强杀失败（一个目标都没杀掉），错误沿 `?` 上抛（退出码 1）。
fn force_if_running(args: &RestartArgs) -> Result<Option<Value>> {
    if !args.force {
        return Ok(None);
    }
    let outcomes = kill_api_port_listener()?;
    let killed: Vec<u32> = outcomes
        .iter()
        .filter(|outcome| outcome.killed)
        .map(|outcome| outcome.pid)
        .collect();
    // stderr 只做进度提示，用 writeln 忽略 EPIPE，避免管道关闭时 panic。
    let _ = writeln!(
        io::stderr(),
        "restart --force: 已结束占用 {PORT} 的进程: {killed:?}"
    );
    let report = json!({
        "killed": killed,
        "attempts": outcomes.iter().map(KillOutcome::to_json).collect::<Vec<_>>(),
    });
    Ok(Some(report))
}

#[cfg(test)]
mod wait_for_validation_tests {
    use super::validate_wait_for;

    // `--wait-for none|off|skip`（大小写不敏感）必须在 CLI 侧通过。
    #[test]
    fn wait_for_none_variants_pass_validation() {
        for cond in ["none", "off", "skip", " NONE ", "Skip"] {
            assert!(validate_wait_for(cond).is_ok(), "cond: {cond:?}");
        }
    }

    /// 旧条件保持原语义；空 selector/text 仍在 CLI 侧拒绝（exit 2）。
    #[test]
    fn wait_for_conditions_keep_old_validation() {
        assert!(validate_wait_for("networkidle").is_ok());
        assert!(validate_wait_for("dom-quiet").is_ok());
        assert!(validate_wait_for("selector:.result").is_ok());
        assert!(validate_wait_for("text:登录成功").is_ok());
        assert!(validate_wait_for("selector:   ").is_err());
        assert!(validate_wait_for("text:").is_err());
        assert!(validate_wait_for("bogus").is_err());
    }
}
