use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// 把 `--header "Name: Value"` 解析成 JSON 对象。
///
/// 漏冒号必须报错、不能静默跳过：`--header Authorization` 被丢掉时请求会**匿名**发出，
/// 调用方看到的是 401/403，很难联想到是参数少了冒号。
pub(crate) fn parse_headers(pairs: &[String]) -> Result<Value> {
    let mut map = serde_json::Map::new();
    // HTTP 头名大小写不敏感：`-H "A: 1" -H "a: 2"` 以前会留下两个键（daemon 侧
    // 谁生效取决于实现），实际发出去的头等于「看运气」。重复一律报错。
    let mut seen: Vec<String> = Vec::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once(':') else {
            return Err(usage_err!(
                "--header / --request-header 需要 `Name: Value` 形式（缺少冒号）: {pair}"
            ));
        };
        let key = key.trim();
        let lower = key.to_ascii_lowercase();
        if seen.contains(&lower) {
            return Err(usage_err!(
                "--header 重复的键: {key}（同一 header 请合并成一个值）"
            ));
        }
        seen.push(lower);
        map.insert(key.to_string(), json!(value.trim()));
    }
    Ok(Value::Object(map))
}

/// 解析 `--body`：`{`/`[` 开头的一律按 JSON 严格解析，其余按纯文本字符串发送。
///
/// 早先是 `if let Ok(...)` 失败后落到 `json!(text)`，把坏 JSON 当字符串发出去，
/// 服务端收到字符串而非对象，表现为「mock 规则不生效」/「请求体字段全 undefined」。
/// 现在与 `run_broadcast` 的 `--args` 一致：解析失败直接报错，错误信息带行列位置。
pub(crate) fn parse_body(text: &str) -> Result<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return match serde_json::from_str::<Value>(text) {
            Ok(value) => Ok(value),
            // 用法错（退出码 2），不是执行失败：以前走 anyhow → 退出码 1 +
            // 纯文本，调用方会当成「可重试」。四个 --body 调用点统一继承。
            Err(err) => Err(usage_failure(
                "invalid_json",
                format!(
                    "--body 以 {{ 或 [ 开头但不是合法 JSON（第 {} 行第 {} 列）: {}",
                    err.line(),
                    err.column(),
                    err
                ),
                "检查引号/逗号/括号是否配对；要按纯文本发送，请不要以 { 或 [ 开头",
            )),
        };
    }
    Ok(json!(text))
}

pub(crate) fn run_net_command(cmd: NetCommand) -> Result<()> {
    let (payload, timeout) = match cmd.action {
        NetAction::Fetch(args) => {
            // 枚举值本地校验：非法值以前会原样下发，daemon 静默回落默认策略，
            // 调用方以为「omit 掉了 cookie」，实际按 include 发了。
            let credentials = args.credentials.to_ascii_lowercase();
            if !matches!(credentials.as_str(), "include" | "omit" | "same-origin") {
                return Err(usage_failure(
                    "invalid_argument",
                    format!("net fetch --credentials 的值 {:?} 不合法", args.credentials),
                    "可选 include（默认，带 cookie）/ omit（不带）/ same-origin（仅同源带）",
                ));
            }
            let mut payload = json!({
                "action": "fetch",
                "url": args.url,
                "method": args.method,
                "response_type": args.response_type,
                "max_chars": args.max_chars,
                "credentials": credentials,
                "parse_json": !args.no_parse_json,
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            });
            let headers = parse_headers(&args.headers)?;
            if headers.as_object().is_some_and(|map| !map.is_empty()) {
                payload["headers"] = headers;
            }
            if let Some(body) = args.body {
                payload["body"] = parse_body(&body)?;
            }
            (payload, args.timeout)
        }
        NetAction::Hook(args) => {
            // 只把**显式给出**的开关放进 payload：服务端对 None 的处理是「沿用已存值」。
            // 旧实现每次都给 capture_bodies/max_log 默认值，第二次 `net hook --persist`
            // 会把之前持久化的设置静默压回 false/200（端到端）。
            let mut payload = json!({
                "action": "hook",
                "persist": args.persist,
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            });
            if let Some(capture) = args.capture_bodies {
                payload["capture_bodies"] = json!(capture);
            }
            if let Some(max_log) = args.max_log {
                payload["max_log"] = json!(max_log);
            }
            if !args.matches.is_empty() {
                payload["matches"] = json!(args.matches);
            }
            (payload, args.timeout)
        }
        NetAction::Log(args) => {
            let mut payload = json!({
                "action": "log",
                "limit": args.limit,
                "clear": args.clear,
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            });
            if let Some(pattern) = args.matches {
                payload["match"] = json!(pattern);
            }
            // 只有显式打开时才下发：不带 --debug-pass 的普通读取不应关掉
            // 别人已经打开的调试开关。
            if args.debug_pass {
                payload["debug_pass"] = json!(true);
            }
            // 只有显式指定 engine 才下发；缺省（None）沿用服务端合并双源的行为。
            if let Some(engine) = args.engine {
                payload["engine"] = json!(engine);
            }
            (payload, args.timeout)
        }
        NetAction::Mock(args) => {
            let mut rule = json!({
                "match": args.pattern,
                "abort": args.abort,
            });
            if let Some(status) = args.status {
                rule["status"] = json!(status);
            } else if args.body.is_none()
                && args.request_headers.is_empty()
                && args.request_body.is_none()
                && args.response_status.is_none()
                && args.response_body.is_none()
                && !args.abort
            {
                // 什么修饰都没给：当成「返回 200 空响应」的纯 mock。
                rule["status"] = json!(200);
            }
            if let Some(method) = args.method {
                rule["method"] = json!(method);
            }
            if let Some(body) = args.body {
                rule["body"] = parse_body(&body)?;
            }
            let headers = parse_headers(&args.headers)?;
            if headers.as_object().is_some_and(|map| !map.is_empty()) {
                rule["headers"] = headers;
            }
            let request_headers = parse_headers(&args.request_headers)?;
            if request_headers
                .as_object()
                .is_some_and(|map| !map.is_empty())
            {
                rule["requestHeaders"] = request_headers;
            }
            if let Some(body) = args.request_body {
                rule["requestBody"] = parse_body(&body)?;
            }
            if args.response_status.is_some() || args.response_body.is_some() {
                let mut response = json!({});
                if let Some(status) = args.response_status {
                    response["status"] = json!(status);
                }
                if let Some(body) = args.response_body {
                    response["body"] = parse_body(&body)?;
                }
                rule["response"] = response;
            }
            let payload = json!({
                "action": "route",
                "rule": rule,
                "engine": args.engine,
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            });
            (payload, args.timeout)
        }
        NetAction::Routes(args) => (
            json!({
                "action": "routes",
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            }),
            args.timeout,
        ),
        NetAction::Unroute(args) => (
            json!({
                "action": "unroute",
                "match": args.pattern,
                // 不传 engine 的话服务端只会删页面内规则，cdpRules 永远清不掉。
                "engine": args.engine,
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            }),
            args.timeout,
        ),
        NetAction::Reset(args) => (
            json!({
                "action": "reset",
                "frame_id": args.frame,
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
            }),
            args.timeout,
        ),
    };
    // 所有参数解析/校验都在起 daemon 之前完成：坏参数应当 exit 2，
    // 而不是先把 daemon 拉起来再报错。
    ensure_server()?;
    print_json(request(
        "POST",
        "/net",
        Some(payload),
        http_budget(timeout),
    )?);
    Ok(())
}

pub(crate) fn run_network_command(args: NetworkCommand) -> Result<()> {
    ensure_server()?;
    match args.action {
        NetworkAction::Start(args) => print_json(request(
            "POST",
            "/network/start",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
        NetworkAction::List(args) => print_json(request(
            "POST",
            "/network/list",
            Some(json!({
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
                "filter": args.filter,
                "limit": args.limit,
            })),
            http_budget(args.timeout),
        )?),
        NetworkAction::Detail(args) => print_json(request(
            "POST",
            "/network/detail",
            Some(json!({
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
                "request_id": args.request_id,
            })),
            http_budget(args.timeout),
        )?),
        NetworkAction::Clear(args) => print_json(request(
            "POST",
            "/network/clear",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
        NetworkAction::Stop(args) => print_json(request(
            "POST",
            "/network/stop",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
    }
    Ok(())
}

pub(crate) fn run_console_command(args: ConsoleCommand) -> Result<()> {
    // History/Tail 各自先做参数校验再 ensure_server（坏参数不应先拉起 daemon）；
    // 其余子命令沿用「先确保 daemon，再发请求」的旧顺序。
    match &args.action {
        ConsoleAction::History(_) | ConsoleAction::Tail(_) => {}
        _ => ensure_server()?,
    }
    match args.action {
        ConsoleAction::Start(args) => print_json(request(
            "POST",
            "/console/start",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
        ConsoleAction::List(args) => print_json(request(
            "POST",
            "/console/list",
            Some(json!({
                "switch_tab_id": args.tab,
                "browser": args.browser,
                "profile": args.profile,
                "level": args.level,
                "limit": args.limit,
            })),
            http_budget(args.timeout),
        )?),
        ConsoleAction::Clear(args) => print_json(request(
            "POST",
            "/console/clear",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
        ConsoleAction::Stop(args) => print_json(request(
            "POST",
            "/console/stop",
            Some(
                json!({ "switch_tab_id": args.tab, "browser": args.browser, "profile": args.profile }),
            ),
            http_budget(args.timeout),
        )?),
        // 2026-11-13：一次性取回已有 console/error 历史 / 轮询跟随。
        ConsoleAction::History(args) => run_console_history(args)?,
        ConsoleAction::Tail(args) => run_console_tail(args)?,
    }
    Ok(())
}

// 占位函数已由 R5 实现：`console history`。
///
/// 一次取回「页面加载期已经产生」的 console / error：
/// 1. `/console/list` 探测是否已有采集会话；
/// 2. 没有就 `/console/start`（idempotent），并记住是不是本次启动的；
/// 3. `--clear` 时先清空缓冲；
/// 4. 等 `settle_ms` 让 CDP Runtime.enable / Log.enable 回放既有消息；
/// 5. `/console/list` 取 entries，按 `--types` 本地过滤；
/// 6. 若采集是本次启动的，采样后 `/console/stop` 恢复原状；原本就在采集绝不停。
pub(crate) fn run_console_history(args: ConsoleHistoryArgs) -> Result<()> {
    ensure_server()?;
    let settle_ms = args
        .settle_ms
        .unwrap_or_else(crate::settings::console_history_settle_ms)
        .min(60_000);
    let types = parse_console_types(args.types.as_deref());
    let timeout = http_budget(args.timeout);
    let selector = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });

    // 1) 探测：老 daemon 没有顶层 active 时回落到扩展的 status 字段；两者都没有
    // 就是「未知」（None），后续按「不擅自 stop」处理。
    let probe = request(
        "POST",
        "/console/list",
        Some(merge_payload(&selector, json!({ "limit": 1 }))),
        timeout,
    )?;
    if probe.get("ok").and_then(Value::as_bool) == Some(false) {
        print_json(probe);
        return Ok(());
    }
    let active_before = console_active(&probe);

    // 2) 未在采集才 start。扩展的 idempotent start 在已运行时回 already_running，
    // 用它兜住「探测与 start 之间别人刚好启动了采集」的窗口。
    let mut started_by_history = false;
    if active_before != Some(true) {
        let started = request("POST", "/console/start", Some(selector.clone()), timeout)?;
        if started.get("ok").and_then(Value::as_bool) == Some(false) {
            print_json(started);
            return Ok(());
        }
        let already_running = started
            .get("already_running")
            .and_then(Value::as_bool)
            .or_else(|| {
                started
                    .pointer("/result/already_running")
                    .and_then(Value::as_bool)
            });
        // 扩展明确回 already_running 时以它为准；没有这个信号（老扩展）时只在
        // 「探测明确说 stopped」的情况下停。探测未知（None）时保守不停：
        // console/stop 会清空缓冲，误停别人正在采集的会话代价更高。
        started_by_history = match already_running {
            Some(already) => !already,
            None => active_before == Some(false),
        };
    }

    // 3) --clear 必须在采样之前：先清掉旧缓冲，再等 settle 窗口收集新事件。
    if args.clear {
        let cleared = request("POST", "/console/clear", Some(selector.clone()), timeout)?;
        if cleared.get("ok").and_then(Value::as_bool) == Some(false) {
            let stop_error = if started_by_history {
                console_stop_quiet(&selector, timeout)
            } else {
                None
            };
            let mut out = cleared;
            if let Some(stop_error) = stop_error {
                out["stop_error"] = json!(stop_error);
            }
            print_json(out);
            return Ok(());
        }
    }

    // 4) 采样窗口：Runtime.enable / Log.enable 的回放事件在这段时间内到达。
    if settle_ms > 0 {
        sleep(Duration::from_millis(settle_ms));
    }

    // 5) 取 entries。limit 由 daemon 夹到 1..=500；类型过滤在本地做（大小写不敏感）。
    let listed = match request(
        "POST",
        "/console/list",
        Some(merge_payload(&selector, json!({ "limit": args.limit }))),
        timeout,
    ) {
        Ok(value) => value,
        Err(err) => {
            if started_by_history {
                let _ = console_stop_quiet(&selector, timeout);
            }
            return Err(err);
        }
    };
    if listed.get("ok").and_then(Value::as_bool) == Some(false) {
        let stop_error = if started_by_history {
            console_stop_quiet(&selector, timeout)
        } else {
            None
        };
        let mut out = listed;
        if let Some(stop_error) = stop_error {
            out["stop_error"] = json!(stop_error);
        }
        print_json(out);
        return Ok(());
    }
    let entries: Vec<Value> = console_entries(&listed)
        .into_iter()
        .filter(|entry| console_entry_matches_types(entry, &types))
        .collect();
    let count = entries.len();

    // 6) 只恢复「本次 history 自己启动」的采集；原本就在采集时保持不动。
    let mut stopped_after = false;
    let mut stop_error: Option<String> = None;
    if started_by_history {
        match console_stop_quiet(&selector, timeout) {
            None => stopped_after = true,
            Some(err) => stop_error = Some(err),
        }
    }

    let mut result = json!({
        "entries": entries,
        "count": count,
        "settle_ms": settle_ms,
        "started_by_history": started_by_history,
        "types": types,
        "active_before": active_before,
        "stopped_after": stopped_after,
    });
    if let Some(stop_error) = stop_error {
        result["stop_error"] = json!(stop_error);
    }
    print_json(json!({ "ok": true, "result": result }));
    Ok(())
}

// 占位函数已由 R5 实现：`console tail`。
///
/// 轮询 `/console/list`，把「上一轮没见过」的条目按到达顺序逐行输出：
/// - 默认 JSONL，一行一个 entry；
/// - 全局 `--pretty` 时输出人读行 `[ts] level text`；
/// - 去重键优先用 entry id，否则 `(timestamp, level, text)`；
/// - 结束时输出一行 `{"tail_finished":true,...}`。
///
/// 不新增 daemon 路由；采集本身需要先由 `console start` / `console history` 打开。
pub(crate) fn run_console_tail(args: ConsoleTailArgs) -> Result<()> {
    if !args.duration.is_finite() || args.duration < 0.0 {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "console tail --duration 必须是非负有限秒数（收到 {}）",
                args.duration
            ),
            "例如 --duration 10；只想取一次历史用 `atb console history`",
        ));
    }
    ensure_server()?;
    let duration = args.duration.min(3_600.0);
    let interval = if args.interval.is_finite() {
        args.interval.clamp(0.05, 5.0)
    } else {
        0.5
    };
    let types = parse_console_types(args.types.as_deref());
    let timeout = http_budget(args.timeout);
    let selector = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });
    let deadline = Instant::now() + Duration::from_secs_f64(duration);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut printed = 0usize;
    let mut active: Option<bool> = None;

    loop {
        let value = match request(
            "POST",
            "/console/list",
            Some(merge_payload(&selector, json!({ "limit": args.limit }))),
            timeout,
        ) {
            Ok(value) => value,
            Err(err) => {
                // 轮询期的传输错误按「本轮没取到」处理；但已经输出过内容后不能
                // 把整条命令判成成功，因此直接给出结构化错误并结束。
                print_json(json!({
                    "ok": false,
                    "error": format!("console tail 轮询失败: {err}"),
                    "error_code": "console_tail_failed",
                    "hint": "检查 daemon / 扩展连接（atb status）；已输出的日志行仍然有效",
                }));
                return Ok(());
            }
        };
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            print_json(value);
            return Ok(());
        }
        if let Some(current) = console_active(&value) {
            active = Some(current);
        }
        for entry in console_entries(&value) {
            let key = console_entry_key(&entry);
            // 先登记再过滤：类型不匹配的条目也要记住，避免它在后续轮次里
            // 因为过滤条件变化被当成「新增」重复输出。
            if !seen.insert(key) {
                continue;
            }
            if !console_entry_matches_types(&entry, &types) {
                continue;
            }
            if PRETTY_OUTPUT.load(Ordering::Relaxed) {
                write_stdout(&console_entry_human(&entry));
            } else {
                write_stdout(&serde_json::to_string(&entry).unwrap_or_else(|_| "null".to_string()));
            }
            printed += 1;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_secs_f64(interval)));
    }

    let mut finished = json!({
        "tail_finished": true,
        "duration_sec": duration,
        "count": printed,
    });
    if let Some(active) = active {
        finished["active"] = json!(active);
        if !active {
            finished["hint"] = json!(
                "console 采集当前未启动：先 `atb console start`（或 `atb console history`）再 tail"
            );
        }
    }
    write_stdout(&serde_json::to_string(&finished).unwrap_or_else(|_| "{}".to_string()));
    Ok(())
}

/// 把公共的标签选择字段与额外字段合并成一个请求体。
fn merge_payload(base: &Value, extra: Value) -> Value {
    let mut payload = base.clone();
    if let Value::Object(extra) = extra {
        if let Some(map) = payload.as_object_mut() {
            map.extend(extra);
        }
    }
    payload
}

/// `/console/list` 的采集状态：优先顶层 `active`（本轮 daemon 新增），
/// 老 daemon 回落到扩展的 `result.status`；都没有时返回 None（未知）。
fn console_active(value: &Value) -> Option<bool> {
    value.get("active").and_then(Value::as_bool).or_else(|| {
        value
            .pointer("/result/status")
            .and_then(Value::as_str)
            .map(|status| status == "started")
    })
}

/// 从 `/console/list` 回包里取 entries（扩展的 `logs` 数组）。
fn console_entries(value: &Value) -> Vec<Value> {
    value
        .pointer("/result/logs")
        .or_else(|| value.pointer("/result/result/logs"))
        .or_else(|| value.get("logs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `--types` 解析：逗号分隔、trim、小写；空项丢弃。
pub(crate) fn parse_console_types(spec: Option<&str>) -> Vec<String> {
    spec.map(|spec| {
        spec.split(',')
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// 类型过滤：`error/warn/log/info/debug/rejection` 对 entry 的 level/type/kind
/// 做大小写不敏感**包含**匹配（`warn` 命中 `warning`）。空 types 表示不过滤。
///
/// `rejection` 额外匹配文本里的 `rejection` / `unhandled`：CDP 把 unhandled
/// promise rejection 归到 `Runtime.exceptionThrown`（level=error），扩展没有
/// 独立的 type 字段，只看 level/type 会让 `--types rejection` 恒为空。
pub(crate) fn console_entry_matches_types(entry: &Value, types: &[String]) -> bool {
    if types.is_empty() {
        return true;
    }
    let fields: Vec<String> = ["level", "type", "kind"]
        .iter()
        .filter_map(|key| entry.get(*key).and_then(Value::as_str))
        .map(|value| value.to_ascii_lowercase())
        .collect();
    let text = entry
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    types.iter().any(|needle| {
        fields.iter().any(|field| field.contains(needle.as_str()))
            || (needle == "rejection"
                && (text.contains("rejection")
                    || text.contains("unhandled")
                    || text.contains("uncaught")))
    })
}

/// 去重键：优先 entry id，否则 `(timestamp, level, text)`。
fn console_entry_key(entry: &Value) -> String {
    if let Some(id) = entry.get("id").and_then(Value::as_str) {
        return format!("id:{id}");
    }
    let timestamp = entry
        .get("timestamp")
        .or_else(|| entry.get("ts"))
        .map(Value::to_string)
        .unwrap_or_default();
    let level = entry
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    // 扩展当前的日志条目没有 type；保留该维度让未来新增的 type 不互相去重。
    let kind = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let text = entry.get("text").and_then(Value::as_str).unwrap_or("");
    format!("{timestamp}\u{1f}{level}\u{1f}{kind}\u{1f}{text}")
}

/// `--pretty` 时的人读行：`[timestamp] level text`。
fn console_entry_human(entry: &Value) -> String {
    let level = entry.get("level").and_then(Value::as_str).unwrap_or("log");
    let text = entry.get("text").and_then(Value::as_str).unwrap_or("");
    match entry.get("timestamp").or_else(|| entry.get("ts")) {
        Some(timestamp) => format!("[{timestamp}] {level} {text}"),
        None => format!("[{level}] {text}"),
    }
}

/// 尽力停止 console 采集；成功返回 None，失败返回原因（不抛错）。
fn console_stop_quiet(selector: &Value, timeout: f64) -> Option<String> {
    match request("POST", "/console/stop", Some(selector.clone()), timeout) {
        Ok(value) if value.get("ok").and_then(Value::as_bool) != Some(false) => None,
        Ok(value) => Some(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("console/stop 失败")
                .to_string(),
        ),
        Err(err) => Some(err.to_string()),
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    // `--types` 大小写不敏感、支持逗号分隔；`warn` 命中 `warning`。
    #[test]
    fn console_types_filter_is_case_insensitive_and_contains() {
        let types = parse_console_types(Some(" Error , WARN ,rejection"));
        assert_eq!(types, vec!["error", "warn", "rejection"]);
        let warn = json!({ "level": "warning", "text": "deprecated" });
        let error = json!({ "level": "error", "text": "boom" });
        let log = json!({ "level": "log", "text": "hi" });
        assert!(console_entry_matches_types(&warn, &types));
        assert!(console_entry_matches_types(&error, &types));
        assert!(!console_entry_matches_types(&log, &types));
    }

    /// 空 types 表示不过滤；`type`/`kind` 字段同样参与匹配；unhandled rejection
    /// 在扩展里只有 level=error + 文本，`rejection` 需要兜底匹配文本。
    #[test]
    fn console_types_empty_or_alternate_fields() {
        let entry = json!({ "level": "info", "type": "unhandledrejection", "text": "x" });
        assert!(console_entry_matches_types(&entry, &[]));
        let types = parse_console_types(Some("rejection"));
        assert!(console_entry_matches_types(&entry, &types));
        let text_only = json!({ "level": "error", "text": "Uncaught (in promise) Error: x" });
        assert!(console_entry_matches_types(&text_only, &types));
        assert!(!console_entry_matches_types(
            &json!({ "level": "debug", "text": "x" }),
            &types
        ));
    }

    /// 去重键优先 id，否则 (timestamp, level, text)。
    #[test]
    fn console_entry_key_prefers_id() {
        let with_id = json!({ "id": "e1", "timestamp": 1, "level": "log", "text": "a" });
        assert_eq!(console_entry_key(&with_id), "id:e1");
        let first = json!({ "timestamp": 1.5, "level": "log", "text": "a" });
        let second = json!({ "timestamp": 1.5, "level": "log", "text": "a" });
        let different = json!({ "timestamp": 1.5, "level": "error", "text": "a" });
        assert_eq!(console_entry_key(&first), console_entry_key(&second));
        assert_ne!(console_entry_key(&first), console_entry_key(&different));
    }
}
