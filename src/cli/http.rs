use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// CLI 侧的超时构造入口（与 daemon 的 `server::secs` 同一套护栏）。
pub(crate) fn secs(value: f64) -> Duration {
    Duration::from_secs_f64(crate::preflight::clamp_timeout_seconds(value))
}

/// 把用户给的路径绝对化：展开 `~`，相对路径以**当前 shell 的 cwd** 为基准。
///
/// daemon 由 CLI 以 `current_dir(home)` 拉起，所以把相对路径原样下发会被解析成
/// `%USERPROFILE%\shots\a.png`（`--out` 不报错地写错位置）或「文件不存在」（`upload`）。
/// 唯一可靠的做法是 CLI 侧先绝对化。
pub(crate) fn absolutize_path(raw: &str) -> String {
    absolutize_path_inner(raw).0
}

// 同 [`absolutize_path`]，额外返回「是否发生了非 UTF-8 有损转换」。
///
/// `Path::to_string_lossy` 会把无法解码的字节替换成 U+FFFD：路径本身仍然「看起来
/// 正常」，但 daemon 打开的是另一个不存在的文件。调用方应把 `lossy=true` 写进回包
/// （`path_lossy`），让 Agent 知道这条路径不能直接复用。
fn absolutize_path_inner(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (raw.to_string(), false);
    }
    let expanded: PathBuf = if trimmed == "~" {
        home_dir().unwrap_or_else(|_| PathBuf::from(trimmed))
    } else if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|_| PathBuf::from(trimmed))
    } else {
        PathBuf::from(trimmed)
    };
    // Windows 盘符相对路径（`C:foo.png`）：`is_absolute()` 为 false，而 `cwd.join()`
    // 碰到盘符前缀会**直接返回参数本身**（盘符截断了基准路径），于是下发前仍是
    // `C:foo.png`，daemon 按 home 解析到别处。`std::path::absolute` 走
    // GetFullPathNameW，按 Windows 自己的「C: 盘当前目录」语义展开。
    #[cfg(windows)]
    let expanded = if is_drive_relative(&expanded) {
        std::path::absolute(&expanded).unwrap_or(expanded)
    } else {
        expanded
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        match env::current_dir() {
            Ok(cwd) => cwd.join(&expanded),
            Err(_) => expanded,
        }
    };
    // `cwd.join("./x.pdf")` 会保留 `./` 片段，报错信息里出现 `...\./x.pdf` 这种混合分隔符。
    let cleaned_path = path_clean::clean(&absolute);
    // 先判断再 lossy：`to_str()` 为 None 才说明原始字节不是合法 UTF-8。
    let lossy = cleaned_path.to_str().is_none();
    let cleaned = cleaned_path.to_string_lossy().to_string();
    // Windows 上 Chrome 的 `DOM.setFileInputFiles` 只认反斜杠路径：传 `E:/a/b.pdf`
    // 会「命令成功但文件没挂上」（一个真实触发面）。
    #[cfg(windows)]
    let cleaned = cleaned.replace('/', "\\");
    (cleaned, lossy)
}

/// Windows 盘符相对路径：`C:foo`（有盘符，但第三个字符不是分隔符）。
///
/// `C:foo` 与 `C:\foo` 是两种不同语义：前者相对「C 盘的当前目录」，后者才是绝对路径。
#[cfg(windows)]
fn is_drive_relative(path: &Path) -> bool {
    let Some(raw) = path.to_str() else {
        return false;
    };
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    !matches!(bytes.get(2), Some(b'/') | Some(b'\\'))
}

/// 只对「确实是本地路径字段」的键做绝对化，避免误改选择器/URL。
///
// 任一字段发生非 UTF-8 有损转换时，在 payload 顶层写 `path_lossy: true`。
pub(crate) fn absolutize_payload_paths(payload: &mut Value, keys: &[&str]) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let mut lossy = false;
    for key in keys {
        match object.get_mut(*key) {
            Some(Value::String(raw)) => {
                let (path, was_lossy) = absolutize_path_inner(raw);
                lossy |= was_lossy;
                *raw = path;
            }
            Some(Value::Array(items)) => {
                for item in items.iter_mut() {
                    if let Value::String(raw) = item {
                        let (path, was_lossy) = absolutize_path_inner(raw);
                        lossy |= was_lossy;
                        *raw = path;
                    }
                }
            }
            _ => {}
        }
    }
    if lossy {
        if let Some(object) = payload.as_object_mut() {
            object.insert("path_lossy".to_string(), Value::Bool(true));
        }
    }
}

pub(crate) fn query_escape(input: &str) -> String {
    input
        .bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => vec![b as char],
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

/// 复用同一个 HTTP Client：`reqwest::blocking::Client` 每次构建都会拉起独立的运行时线程，
/// 旧实现一条命令建 2 个，是热路径上最稳定的一笔固定开销。
pub(crate) fn http_client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    // 只连 127.0.0.1，默认必须绕开环境里的 HTTP_PROXY/HTTPS_PROXY：
    // 有代理的机器上（企业网络、调试代理），reqwest 默认会把 localhost 请求也发给代理，
    // 代理连不上就返回 502 文本，CLI 便误报「daemon 未运行 / token 不匹配」。
    //
    // 改为可配置 `ATB_HTTP_NO_PROXY`（默认 `true` 保持现状）。需要把本地
    // 请求交给抓包代理（Charles/mitmproxy）排查时设 `ATB_HTTP_NO_PROXY=0`。
    let mut builder = Client::builder();
    if crate::settings::http_no_proxy() {
        builder = builder.no_proxy();
    }
    let built = builder.build()?;
    let _ = CLIENT.set(built);
    Ok(CLIENT.get().expect("client just initialized"))
}

/// 进程内的 token 缓存。daemon 每次启动都会轮换 token 文件，所以缓存必须可失效
/// （`invalidate_api_token`），否则 restart 这类「先读 token 再拉起新 daemon」的
/// 流程会一直 401。
static API_TOKEN_CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 丢掉缓存的 token，下一次 `api_token()` 会重新读文件。
pub(crate) fn invalidate_api_token() {
    let cell = API_TOKEN_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = None;
    }
}

/// token 在进程内缓存；文件暂时不存在（冷启动）时不要缓存 None，
/// 否则会拿旧值去请求刚启动的 daemon，得到 401。
pub(crate) fn api_token() -> Option<String> {
    let cell = API_TOKEN_CACHE.get_or_init(|| Mutex::new(None));
    if let Some(token) = cell.lock().ok().and_then(|guard| guard.clone()) {
        return Some(token);
    }
    let fresh = config::load_api_token().ok().flatten();
    if let Some(token) = fresh.clone() {
        if let Ok(mut guard) = cell.lock() {
            *guard = Some(token);
        }
    }
    fresh
}

/// 一次 `/ping` 的三种结论。免鉴权、不触碰 token 缓存：冷启动时 daemon 刚写完 token，
/// 这里若走 request() 会把旧 token 固化进缓存。
pub(crate) enum DaemonPing {
    /// 端口没人监听，或监听方不是我们的 daemon。
    Down,
    /// daemon 正常，附带它声明的 API 协议版本。
    Ready(u32),
    /// daemon 活着但插件端口没绑上（`/ping` 的 `ws_bind_error` 有值）。
    Degraded(String),
}

pub(crate) fn ping_state() -> DaemonPing {
    let Ok(client) = http_client() else {
        return DaemonPing::Down;
    };
    let Ok(value) = client
        .get(format!("http://{HOST}:{PORT}/ping"))
        .timeout(Duration::from_secs(1))
        .send()
        .and_then(|response| response.json::<Value>())
    else {
        return DaemonPing::Down;
    };
    if value.get("running").and_then(Value::as_bool) != Some(true) {
        return DaemonPing::Down;
    }
    let protocol = value
        .get("protocol")
        .and_then(Value::as_u64)
        .map(|value| value as u32)
        // 旧 daemon 不报协议：按「过旧」处理（0），由 ensure_server 给出升级指引。
        .unwrap_or(0);
    match value.get("ws_bind_error").and_then(Value::as_str) {
        Some(err) => DaemonPing::Degraded(err.to_string()),
        None => DaemonPing::Ready(protocol),
    }
}

/// CLI 与 daemon 的 API 协议必须一致。
///
/// 不一致时**不要**尝试「尽力而为」：请求结构已经变了，继续跑只会得到
/// 难以定位的解析错误。这里直接给出两条可执行的修复路径。
pub(crate) fn ensure_protocol_compatible(protocol: u32) -> Result<()> {
    let expected = crate::protocol::PROTOCOL_VERSION;
    if protocol == expected {
        return Ok(());
    }
    Err(anyhow!(
        "CLI 与 daemon 版本不配套：CLI 协议 {expected}，{PORT} 上的 daemon 协议 {protocol}。\n\
         这通常是因为升级后旧 daemon 还在跑（它会一直占着端口）。\n\
         修复（二选一）：\n\
         1. 重启 daemon：atb restart\n\
         2. 若 restart 仍不生效，说明端口被别的安装占着：\n\
            Windows: netstat -ano | findstr {PORT} 然后 taskkill /PID <pid> /F\n\
            macOS/Linux: lsof -i:{PORT} 然后 kill <pid>\n\
         处理完再执行 atb status，确认 protocol={expected}"
    ))
}

pub(crate) fn daemon_log_path_hint() -> String {
    config::daemon_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<用户配置目录>/logs/daemon.log".to_string())
}

/// daemon 活着但插件端口没监听：扩展永远也连不上，必须当场说清楚，
/// 否则后续每条命令只会报「插件未连接」，看起来像扩展没装。
pub(crate) fn degraded_daemon_error(reason: &str) -> anyhow::Error {
    anyhow!(
        "daemon 在跑，但插件端口（默认 {}）没有监听：{reason}\n\
         最常见的原因是刚重启过 daemon，旧的扩展连接还在 TIME_WAIT（Windows 约 2 分钟），\
         新 daemon 绑定该端口时拿到 os error 10048。\n\
         处理：稍等一会再试；或立刻用 `atb set-extension-port <其它端口>` 换一个端口。\n\
         daemon 日志: {}",
        config::DEFAULT_EXTENSION_PORT,
        daemon_log_path_hint()
    )
}

// CLI 侧 HTTP 失败的**结构化**载体。
///
/// `request()` 以前只把 401 翻译成一句中文，调用方（`status`/`doctor`）只能靠字符串
/// 子串猜「是超时还是 token 不匹配」。现在失败带着稳定的 `code` 与 `status` 往上抛，
/// 上层用 `downcast_ref` 判断，不再解析文案。
#[derive(Debug)]
// `status` 目前只用于诊断展示；序列化/上层分支主要看 `code`。保留它是为了让
// 调用方能区分 401 与 403（例如 doctor 输出更精确的提示）。
#[allow(dead_code)]
pub(crate) struct HttpFailure {
    pub(crate) code: &'static str,
    pub(crate) status: Option<u16>,
    pub(crate) message: String,
    pub(crate) hint: String,
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HttpFailure {}

/// 从 `request()` 的 Err 里取结构化错误码；取不到返回 `None`。
///
/// 顺序刻意是「结构化优先」：先看我们自己的 [`HttpFailure`]，再把 reqwest 的
/// 分类（`is_timeout`/`is_connect`）映射成码。**不再**用中英文子串兜底判类型。
pub(crate) fn http_failure_code(err: &anyhow::Error) -> Option<&'static str> {
    if let Some(failure) = err.downcast_ref::<HttpFailure>() {
        return Some(failure.code);
    }
    if let Some(source) = err.downcast_ref::<reqwest::Error>() {
        if source.is_timeout() {
            return Some("health_timeout");
        }
        if source.is_connect() {
            return Some("daemon_not_running");
        }
    }
    None
}

pub(crate) fn request(
    method: &str,
    path: &str,
    payload: Option<Value>,
    timeout_secs: f64,
) -> Result<Value> {
    let url = format!("http://{HOST}:{PORT}{path}");
    // 统一超时护栏：`Duration::from_secs_f64` 遇到 NaN / 负数 / 超大值（>5.8e8 秒）
    // 会直接 panic。命令级的参数校验难免有漏，这里兜最后一道。
    //
    // 上限 24h 而不是 1h：单步动作的上限由 `preflight::TIMEOUT_MAX_SECONDS`（3600s）
    // 保证，而 `run`/`batch` 的 HTTP 超时是「所有步骤之和 + 余量」。旧上限 3600s
    // 会让「两步各 3000s」这类合法管道在 CLI 侧先断、daemon 继续跑成孤儿。
    let timeout_secs = if timeout_secs.is_finite() {
        timeout_secs.clamp(0.1, 86_400.0)
    } else {
        30.0
    };
    // 注意：这里**不能**走 `secs()`——它内部的 `clamp_timeout_seconds` 会把值再压回
    // 3600s（单个动作的上限），管道总预算就被它悄悄截断了。NaN/负数上面已经拦掉，
    // `try_from_secs_f64` 只做最后一道溢出保护（>5.8e8 秒返回 Err）。
    let timeout = Duration::try_from_secs_f64(timeout_secs).unwrap_or(Duration::from_secs(30));
    // token 在进程内缓存（见 `api_token`）。daemon 重启会**轮换** token 文件，缓存
    // 不清就会拿旧 token 打新 daemon：`restart` 的流程恰好是「先用旧 token 探测
    // → 拉起新 daemon → 再请求 /health」，必然踩到（实测 restart 100% 失败）。
    // 所以这里对 401 + invalid_api_token 做一次「丢缓存重读 + 重发」，仍 401 才认定
    // 真的不匹配。POST 重发是安全的：401 表示请求被鉴权层拒绝，从未进入业务逻辑。
    for attempt in 0..2 {
        // ：必须同时看 HTTP status 与响应体。只看 body 会在两种情况下
        // 漏判：daemon 的鉴权中间件直接回 401 但 body 不是我们的 JSON（或没有
        // error_code）；以及旧 daemon 回 403 表达同样的「token 不对」。
        let (status, value) = send_once(method, &url, &payload, timeout)?;
        let body_code = value.get("error_code").and_then(Value::as_str);
        let invalid_token = status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || body_code == Some("invalid_api_token");
        if invalid_token && attempt == 0 {
            invalidate_api_token();
            continue;
        }
        if invalid_token {
            let token_path = config::api_token_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "~/.atb/api-token".to_string());
            return Err(anyhow::Error::new(HttpFailure {
                code: "invalid_api_token",
                status: Some(status.as_u16()),
                message: format!(
                    "daemon 正在运行，但 API token 不匹配（{token_path}，HTTP {}）；通常是残留的旧 daemon 占用了 {PORT}。",
                    status.as_u16()
                ),
                hint: format!(
                    "结束占用 {PORT} 的进程后重试；或先关闭其他 atb 实例。也可运行 atb doctor --fix 自动处理"
                ),
            }));
        }
        return Ok(value);
    }
    unreachable!("request 的两轮重试必定有返回值")
}

/// 为单条子请求派生稳定的幂等子键。
///
/// 只依赖「方法 + URL + 请求体」：同一条命令重跑时子请求内容一致 → 子键一致，
/// 仍能命中 60s 回放；多请求命令的不同子请求（不同 tab/body）自然分开。
/// `DefaultHasher::new()` 使用固定键，不是 RandomState，跨进程稳定。
fn derive_idempotency_key(base: &str, method: &str, url: &str, payload: &Option<Value>) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    method.hash(&mut hasher);
    url.hash(&mut hasher);
    payload
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{base}:{:016x}", hasher.finish())
}

/// 发一次请求并把回包解析成 JSON；不处理 token 重试，也不把 401 翻译成人话。
///
/// 分成两步是为了让 `request` 能在「token 过期」时重发一次：回包必须先解析出来
/// 才能判断是不是 401，而 `Response` 的 body 只能读一次。
fn send_once(
    method: &str,
    url: &str,
    payload: &Option<Value>,
    timeout: Duration,
) -> Result<(reqwest::StatusCode, Value)> {
    let client = http_client()?;
    let builder = match method {
        "GET" => client.get(url).timeout(timeout),
        "POST" => client.post(url).timeout(timeout),
        _ => return Err(anyhow!("不支持的 HTTP 方法: {method}")),
    };
    let builder = match api_token() {
        Some(token) => builder.header(config::API_TOKEN_HEADER, token),
        None => builder,
    };
    // 身份走 header 而不是每个 payload 各写一遍：所有端点自动生效，
    // 也不影响既有的请求结构体（旧版 daemon 会忽略未知 header）。
    let builder = builder.header(AGENT_HEADER, agent_id());
    // 幂等键：CLI 全局 flag 透传给 daemon，daemon 对 60s 内同 key 的
    // POST 回放首次响应。旧版 daemon 忽略未知 header，向后兼容。
    //
    // v11：一个 `--idempotency-key` 不能原样挂到多请求命令的每一条 POST 上
    // （`close --tab a,b` 循环 /close、scrape 的 /open + 每个标签 /exec）。daemon 的
    // 幂等 scope 是 method+path+body，同 key 不同 scope 会直接 409，命令只执行第一条。
    // 这里按请求内容派生稳定子键：同一条子请求重发仍能回放，不同子请求互不冲突。
    let builder = match IDEMPOTENCY_KEY
        .get()
        .filter(|_| method == "POST")
        .map(|key| derive_idempotency_key(key, method, url, payload))
    {
        Some(key) => builder.header("x-atb-idempotency-key", key),
        None => builder,
    };
    // 结果未知语义：POST 的失败要分两种处理——
    // a) 请求根本没发出去（连接建立失败 / 请求构造失败）：返回 unexpected_error，可安全重试；
    // b) 请求已发出但响应没回来（超时 / body / decode / 响应截断）：动作可能已经执行，
    //    盲目重试有双击 / 双下单风险，返回结构化 outcome_unknown（退出码 4）。
    //
    // 判据**不能**用「端口还在不在监听」推断：18767 被别的程序占着、daemon
    // 恰好在超时后才崩、或端口只是处于 TIME_WAIT，都会让 TCP 探测给出与「请求是否送达」
    // 无关的答案。reqwest 自己的错误分类才是可靠证据：`is_connect()`/`is_request()`
    // 表示连接没建立或请求没发出，其余发送后错误一律按结果未知处理。
    // GET 没有副作用，失败语义不变。
    let response = if method == "POST" {
        let built = builder.json(&payload.clone().unwrap_or_else(|| json!({})));
        match built.send() {
            Ok(response) => response,
            Err(err) => {
                if err.is_connect() || err.is_request() {
                    return Err(err.into());
                }
                return Ok((
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    json!({
                        "ok": false,
                        "error_code": "outcome_unknown",
                        "outcome": "unknown",
                        "outcome_unknown": true,
                        "error": format!(
                            "POST 请求失败，但结果未知（请求可能已送达 daemon 并执行）: {err}"
                        ),
                        "hint": "结果未知，不要盲目重试动作类命令；可用 --idempotency-key 保证只执行一次",
                    }),
                ));
            }
        }
    } else {
        builder.send()?
    };
    let status = response.status();
    let value: Value = response.json().unwrap_or_else(|err| {
        let mut invalid = json!({
            "ok": false,
            "error": format!("daemon 返回了非 JSON 响应 (HTTP {}): {err}", status.as_u16()),
            "error_code": "invalid_response"
        });
        // 收到了 HTTP 响应但读不出 JSON（通常是响应被截断）：daemon 可能已经执行了
        // 动作，POST 必须按结果未知处理（退出码 4），不能当成「可安全重试的 1」。
        if method == "POST" {
            invalid["outcome_unknown"] = json!(true);
            invalid["outcome"] = json!("unknown");
            invalid["hint"] = json!(
                "响应不完整，动作可能已执行；不要盲目重试，可先用 --idempotency-key 重发或直接复查页面状态"
            );
        }
        invalid
    });
    Ok((status, value))
}

pub(crate) fn is_server_alive() -> bool {
    // 只做一次 TCP 探测：用于「端口有没有人在听」的粗判（restart/stop 等流程）。
    // 注意它无法区分「我们的 daemon」和「别的程序占着 18767」，
    // 所以 ensure_server() 的就绪判定必须用 /ping，不能用这个。
    tcp_listening(PORT)
}

pub(crate) fn ensure_server() -> Result<()> {
    // 就绪判定必须是 /ping 而不是 TCP：18767 可能被别的程序（或半死的旧 daemon）占着，
    // 只测 TCP 会把它当成可用，命令随后就变成一条 10s 的 reqwest 超时错误。
    match ping_state() {
        DaemonPing::Ready(protocol) => return ensure_protocol_compatible(protocol),
        DaemonPing::Degraded(reason) => return Err(degraded_daemon_error(&reason)),
        DaemonPing::Down => {}
    }
    // 锁文件必须放在用户可写目录：装在只读位置（Program Files / Nix store / 只读容器）时，
    // 往 exe 目录写会让所有命令以一个裸 IO 错误失败。
    let lock_dir = config::user_config_dir()?;
    fs::create_dir_all(&lock_dir)
        .with_context(|| format!("创建配置目录失败: {}", lock_dir.display()))?;
    let lock_path = lock_dir.join("daemon.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    // Windows 上子进程会继承可继承句柄：daemon 一旦继承锁文件句柄，
    // 即使 CLI 退出，文件锁仍被 daemon 持有，后续所有命令都会卡在 lock_exclusive()。
    make_handle_non_inheritable(&lock);
    lock.lock_exclusive()?;
    let result = ensure_server_locked();
    let _ = lock.unlock();
    result
}

pub(crate) fn ensure_server_locked() -> Result<()> {
    // 拿到锁之后必须用 /ping 二次确认，不能只看 TCP：
    // 「别人刚 bind 了 18767 但 axum 还没就绪」和「端口被别的程序占着」都会让 TCP 探测成功，
    // 只凭 TCP 就认为「已经在跑」或不加判断地 spawn，会让两个 daemon 同时去抢插件端口
    // 18765，后启动的那个在 WebSocket bind 处报 os error 10048。
    match ping_state() {
        DaemonPing::Ready(protocol) => return ensure_protocol_compatible(protocol),
        DaemonPing::Degraded(reason) => return Err(degraded_daemon_error(&reason)),
        DaemonPing::Down => {}
    }
    let log_path = daemon_log_path_hint();
    if is_server_alive() {
        // 18767 有人在听但 /ping 不通：可能是没有锁的旧版本 CLI 拉起的 daemon 正在就绪，
        // 也可能是别的程序占着端口。等一小会儿，仍然不通就明确报错，不再 spawn 第二个。
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            sleep(Duration::from_millis(150));
            match ping_state() {
                DaemonPing::Ready(protocol) => return ensure_protocol_compatible(protocol),
                DaemonPing::Degraded(reason) => return Err(degraded_daemon_error(&reason)),
                DaemonPing::Down => {}
            }
        }
        return Err(anyhow!(
            "{PORT} 已被占用，但对 /ping 无响应：可能是残留的旧 daemon 或别的程序。\n\
             处理：先结束占用 {PORT} 的进程再重试（Windows: netstat -ano | findstr {PORT} 后 \
             taskkill /PID <pid> /F；macOS/Linux: lsof -i:{PORT}）。\n\
             daemon 日志: {log_path}"
        ));
    }
    start_server()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // 冷启动路径不敏感，用 /ping 确认 axum 已经能处理请求，避免只测到 TCP 握手。
        match ping_state() {
            DaemonPing::Ready(protocol) => {
                ensure_protocol_compatible(protocol)?;
                // WebSocket 监听是独立 task，可能比 API 晚几毫秒才失败，再确认一次。
                sleep(Duration::from_millis(250));
                if let DaemonPing::Degraded(reason) = ping_state() {
                    return Err(degraded_daemon_error(&reason));
                }
                return Ok(());
            }
            DaemonPing::Degraded(reason) => return Err(degraded_daemon_error(&reason)),
            DaemonPing::Down => sleep(Duration::from_millis(150)),
        }
    }
    Err(anyhow!(
        "atb server 启动超时（15s）：{PORT} 可能被其它进程占用，或 daemon 启动即退出。\n\
         daemon 日志: {log_path}（也可运行 atb logs --tail 100 查看）"
    ))
}

pub(crate) fn start_server() -> Result<()> {
    let exe = env::current_exe()?;
    let log_path = config::ensure_log_file()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("打开 daemon 日志失败: {}", log_path.display()))?;
    let log_err = log.try_clone()?;
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        // 别拿 exe 目录当工作目录：相对路径参数（如 screenshot --out）会以安装目录为基准，
        // 既不可预期，装在只读位置时也可能直接失败。
        // 与 `~` 展开同源：统一走 `config::home_dir()`。
        .current_dir(config::home_dir().unwrap_or_else(env::temp_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不弹控制台窗口。
        // DETACHED_PROCESS：daemon 不挂在调用方的控制台上。
        // CREATE_BREAKAWAY_FROM_JOB：调用方（智能体/PowerShell/CI）常把子进程放进
        //   带「关闭即杀」的 Job Object，不脱离的话 CLI 一退出 daemon 就被一起回收，
        //   导致每条命令都冷启动、并频繁出现端口占用冲突。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 后台 daemon 必须脱离当前终端会话，否则 CLI 退出后子进程会被一起回收。
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // Windows: 调用方（智能体/PowerShell/Node）通常用管道捕获 CLI 输出。
    // 子进程默认会继承这些可继承句柄，导致 daemon 一直持有管道写端，
    // 上层读不到 EOF，命令看起来「卡死」直到超时。这里在 spawn 期间临时关闭继承。
    #[cfg(windows)]
    let inherit_guard = WindowsStdHandleInheritGuard::clear();
    let spawned = command.spawn();
    #[cfg(windows)]
    drop(inherit_guard);
    #[cfg(windows)]
    match spawned {
        Ok(_child) => {}
        Err(err) => {
            // 有些 Job Object 不允许 breakaway，退回不带该标志的方式再试一次。
            const ERROR_ACCESS_DENIED: i32 = 5;
            if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000 | 0x0000_0008);
                let inherit_guard = WindowsStdHandleInheritGuard::clear();
                let retry = command.spawn();
                drop(inherit_guard);
                retry?;
            } else {
                return Err(err.into());
            }
        }
    }
    #[cfg(not(windows))]
    spawned?;
    // 新 daemon 会在绑定端口后重写 token 文件：立刻丢掉旧缓存，让后续请求带上
    // 新 token（否则 restart/冷启动第一条命令会稳定吃到 401）。
    invalidate_api_token();
    Ok(())
}

/// 临时清除当前进程 std 句柄的「可继承」标记，spawn 后恢复。
#[cfg(windows)]
pub(crate) struct WindowsStdHandleInheritGuard {
    pub(crate) saved: Vec<(*mut std::ffi::c_void, u32)>,
}

#[cfg(windows)]
impl WindowsStdHandleInheritGuard {
    pub(crate) fn clear() -> Self {
        use std::ffi::c_void;
        const STD_INPUT_HANDLE: u32 = -10i32 as u32;
        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        const STD_ERROR_HANDLE: u32 = -12i32 as u32;
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn GetStdHandle(std_handle: u32) -> *mut c_void;
            fn GetHandleInformation(object: *mut c_void, flags: *mut u32) -> i32;
            fn SetHandleInformation(object: *mut c_void, mask: u32, flags: u32) -> i32;
        }

        let mut saved = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let handle = GetStdHandle(which);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut flags: u32 = 0;
                if GetHandleInformation(handle, &mut flags) != 0
                    && (flags & HANDLE_FLAG_INHERIT) != 0
                    && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
                {
                    saved.push((handle, flags));
                }
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for WindowsStdHandleInheritGuard {
    fn drop(&mut self) {
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        #[link(name = "kernel32")]
        extern "system" {
            fn SetHandleInformation(object: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        for (handle, flags) in &self.saved {
            unsafe {
                let _ = SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, *flags);
            }
        }
    }
}

pub(crate) fn wait_server_stopped(timeout: Duration) -> bool {
    // 除了 API 端口，还要等插件端口（WebSocket）也释放：旧 daemon 退出时两个监听
    // 不是同时关闭的，只要 API 一关就 spawn 新 daemon，新 daemon 会在 WS bind 处
    // 撞上还在监听的 18765（os error 10048），表现就是「扩展永远连不上」。
    let extension_port = config::load_existing()
        .map(|cfg| cfg.extension_port)
        .unwrap_or(config::DEFAULT_EXTENSION_PORT);
    let deadline = Instant::now() + timeout;
    loop {
        if !is_server_alive() && !tcp_listening(extension_port) {
            return true;
        }
        if Instant::now() >= deadline {
            return !is_server_alive() && !tcp_listening(extension_port);
        }
        sleep(Duration::from_millis(100));
    }
}

/// 关闭句柄继承，避免 daemon 继承 CLI 打开的启动锁 / 其它句柄。
#[cfg(windows)]
pub(crate) fn make_handle_non_inheritable(file: &std::fs::File) {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetHandleInformation(object: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    unsafe {
        let handle = file.as_raw_handle();
        let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
    }
}

#[cfg(not(windows))]
pub(crate) fn make_handle_non_inheritable(_file: &std::fs::File) {}

/// URL 是否匹配（去协议、去尾部斜杠后按前缀比，容忍重定向到 http(s)）。
///
/// scheme 只剥**一次**，且大小写不敏感：`trim_start_matches` 会反复剥离，
/// `http://http://x` 曾被剥成 `x`；而大写 `HTTP://Example.com` 完全剥不掉，
/// 于是同一个站点在重定向前后判成两个不同目标。
pub(crate) fn url_matches(actual: &str, expected: &str) -> bool {
    let norm = |raw: &str| -> String {
        let trimmed = raw.trim();
        // 先按小写副本判断前缀，再按字节长度切原文：ASCII 前缀的字节边界一定安全，
        // 仍用 get() 兜底防越界。
        let lower = trimmed.to_ascii_lowercase();
        let scheme_len = if lower.starts_with("https://") {
            "https://".len()
        } else if lower.starts_with("http://") {
            "http://".len()
        } else {
            0
        };
        let host_and_path = trimmed.get(scheme_len..).unwrap_or("");
        host_and_path.trim_end_matches('/').to_ascii_lowercase()
    };
    let actual = norm(actual);
    let expected = norm(expected);
    !expected.is_empty() && (actual == expected || actual.starts_with(&format!("{expected}/")))
}
/// 当前 daemon 看到的所有标签页：(session_key, url)。用来判断新实例有没有真的连上来。
pub(crate) fn browser_targets_snapshot() -> Result<Vec<(String, String)>> {
    let value = request("GET", "/browsers", None, 10.0)?;
    let targets = value
        .pointer("/result/targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(targets
        .iter()
        .filter_map(|item| {
            let key = item.get("session_key").and_then(Value::as_str)?;
            let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
            Some((key.to_string(), url.to_string()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对路径必须以**当前 shell 的 cwd** 为基准绝对化。
    #[test]
    fn absolutize_relative_path_uses_cwd() {
        let out = absolutize_path("shots/a.png");
        assert!(Path::new(&out).is_absolute(), "应绝对化，实际 {out}");
    }

    /// ：`C:foo.png` 既不是绝对路径，也不能靠 `cwd.join()`（盘符会截断
    /// 基准路径）。下发前必须是绝对路径。
    #[cfg(windows)]
    #[test]
    fn absolutize_drive_relative_path() {
        let out = absolutize_path("C:atb-closeout-dryrun-probe.png");
        assert!(Path::new(&out).is_absolute(), "应绝对化，实际 {out}");
        assert!(out.starts_with("C:\\"), "应落在 C 盘，实际 {out}");
    }

    /// 已经是绝对路径的盘符路径不能被改写。
    #[cfg(windows)]
    #[test]
    fn absolutize_drive_absolute_path_kept() {
        let out = absolutize_path(r"C:\Windows\Temp\atb-closeout.png");
        assert_eq!(out, r"C:\Windows\Temp\atb-closeout.png");
        assert!(!is_drive_relative(Path::new(&out)));
    }

    /// 空串保持原样：调用方用它表示「没传」，不能被绝对化成一个目录。
    #[test]
    fn absolutize_empty_returns_empty() {
        assert_eq!(absolutize_path("   "), "   ");
    }

    // v11：多请求命令（close 多标签 / scrape）不能共用一个裸幂等键，
    /// 否则 daemon 的 scope 判定会让第 2 个请求 409。子键只依赖请求内容，
    /// 同一条命令重跑仍然命中回放。
    #[test]
    fn idempotency_subkey_is_stable_and_body_sensitive() {
        let url = format!("http://{HOST}:{PORT}/close");
        let first = derive_idempotency_key("K", "POST", &url, &Some(json!({ "tab_id": "a" })));
        let again = derive_idempotency_key("K", "POST", &url, &Some(json!({ "tab_id": "a" })));
        let other_tab = derive_idempotency_key("K", "POST", &url, &Some(json!({ "tab_id": "b" })));
        let other_path = format!("http://{HOST}:{PORT}/exec");
        let other_url =
            derive_idempotency_key("K", "POST", &other_path, &Some(json!({ "tab_id": "a" })));

        assert!(first.starts_with("K:"), "子键应保留用户 key 前缀：{first}");
        assert_eq!(first, again, "同一请求必须派生同一个子键（否则无法回放）");
        assert_ne!(first, other_tab, "不同 body 必须派生不同子键（否则 409）");
        assert_ne!(first, other_url, "不同 URL 必须派生不同子键");
    }
}
