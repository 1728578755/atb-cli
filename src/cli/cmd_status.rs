use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// 只读取扩展端口，不创建配置文件。
///
/// `status` 是最需要用来诊断「HOME 只读 / 容器里没有配置」的命令，以前它调用
/// `config::load_or_create` 产生写副作用，只读环境下直接 exit 1 且没有 JSON。
///
/// 现在直接复用 `config::load()`（只读、不落盘、目标损坏时回退 `.bak`，并且会
/// `validate_port`）。以前这里自带**第三份**解析器：不读 `.bak`、不做端口校验、
/// `as u16` 还是截断转换（配置写 `70000` 会显示成 `4464`），于是 status 显示的
/// 「configured」可能和 daemon 实际用的值不一致 —— 而 status 正是用来查这件事的
/// 命令（v8）。解析失败不再静默当成默认值，而是把原因带出去。
fn configured_extension_port() -> (u16, Option<String>) {
    match config::load() {
        Ok(config) => (config.extension_port, None),
        Err(err) => (config::DEFAULT_EXTENSION_PORT, Some(err.to_string())),
    }
}

/// `/health` 探测的客户端超时。
///
/// 旧值是 1s：daemon 正在跑大快照/长管道时，/health 排队超过 1s 就会超时，于是
/// `atb status` 把「daemon 忙」误诊成「daemon 有问题」甚至「token 失配」，把人引去
/// 杀进程。放宽到 3s：既覆盖排队，又不至于让 status 自己变成慢命令。
const HEALTH_PROBE_TIMEOUT: f64 = 3.0;

// 探测失败的分类**只看结构化证据**。
///
/// 以前用「中英文子串」判 timeout/token，而且 timeout 判定写在 token 前面：daemon
/// 回 401 时错误文本里若恰好带 "timeout"（例如上游代理）就会被误判成「daemon 忙」。
/// 现在 `request()` 的失败带 [`HttpFailure`]（稳定 code + status），reqwest 自身的
/// `is_timeout()` 也已映射成 `health_timeout`，这里直接读码。
fn health_failure_code(err: &anyhow::Error) -> Option<&'static str> {
    http_failure_code(err)
}

/// 「端口在监听但 /health 拿不到」的统一输出骨架。
///
/// 以前这条分支的两份 JSON 各写一遍（重复了 ports/connection 块），改一处容易漏另一处。
fn unhealthy_value(
    summary: &str,
    message: String,
    hint: &str,
    error: Option<String>,
    configured: u16,
    configured_error: Option<String>,
) -> Value {
    let mut value = json!({
        "ok": false,
        "healthy": false,
        // ok:false 必须带稳定 error_code，调用方才能直接分支，不用解析中文。
        "error_code": summary,
        "summary": summary,
        "message": message,
        "hint": hint,
        "running": false,
        "ready": false,
        "ports": {
            "api": PORT,
            "extension": {
                "configured": configured,
                "configured_error": configured_error,
                "listening": null,
                "matched": false
            }
        },
        "connection": {
            "extension_connected": false,
            "active_tabs": 0
        }
    });
    // error 只在「有具体异常」时出现：字段只增不改，daemon_not_running 分支保持原样。
    if let Some(error) = error {
        value["error"] = json!(error);
    }
    value
}

pub(crate) fn status_value() -> Result<Value> {
    let (configured, configured_error) = configured_extension_port();
    let mut value = match request("GET", "/health", None, HEALTH_PROBE_TIMEOUT) {
        Ok(value) => enrich_status(value),
        Err(err) if is_server_alive() => {
            // 端口在监听却拿不到 /health：三种成因的下一步完全不同，不能一律归因 token
            //。
            match health_failure_code(&err) {
                Some("health_timeout") => unhealthy_value(
                    "health_timeout",
                    format!(
                        "{PORT} 端口在监听，但 /health 在 {HEALTH_PROBE_TIMEOUT}s 内没有返回：{err}"
                    ),
                    "daemon 可能正忙（大页面快照 / 长管道 / 批量抓取）或已卡住：稍后重试 atb status；持续如此用 atb doctor 看逐项诊断与日志，必要时 atb restart",
                    Some(err.to_string()),
                    configured,
                    configured_error,
                ),
                Some("invalid_api_token") => {
                    // TCP 通但 HTTP 401/403：最常见的成因是残留的旧 daemon 用了另一份 token。
                    unhealthy_value(
                        "daemon_token_mismatch",
                        format!("{PORT} 端口上有一个 daemon 在监听，但 API token 不匹配（通常是残留的旧实例）"),
                        &format!("先结束占用 {PORT} 的进程（Windows: netstat -ano | findstr {PORT} 后 taskkill /PID <pid> /F；macOS/Linux: lsof -i:{PORT}），再重试——新 daemon 启动时会刷新 token"),
                        Some(err.to_string()),
                        configured,
                        configured_error,
                    )
                }
                _ => unhealthy_value(
                    "health_failed",
                    format!("{PORT} 端口在监听，但 /health 请求失败：{err}"),
                    "先运行 atb doctor 看逐项诊断（它不会自动启动 daemon）；若怀疑是残留旧 daemon，用 atb restart --force",
                    Some(err.to_string()),
                    configured,
                    configured_error,
                ),
            }
        }
        Err(_) => unhealthy_value(
            "daemon_not_running",
            "daemon 未运行；执行 tabs/open/exec/scan 会自动启动，或运行 atb restart 手动启动"
                .to_string(),
            "执行 atb restart，或任意页面命令（tabs/open/exec）自动拉起 daemon",
            None,
            configured,
            configured_error,
        ),
    };
    // WS 鉴权现状（daemon 的 ws_security 优先，缺失时读本地配置）。
    // 放在最后统一挂载：healthy / unhealthy 两条路径都能看到同一块结论。
    let security = security_value(value.get("ws_security"));
    value["security"] = security;
    // 浏览器身份（healthy / unhealthy 两条路径都要有）。
    // 只从 /health 读取，不为 status 额外发请求。
    attach_browser_identity(&mut value);
    Ok(value)
}

/// `status` 里「正在操作哪个浏览器/profile」的推导结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StatusBrowserIdentity {
    browser_id: Option<String>,
    browser_label: Option<String>,
    profile_id: Option<String>,
    profile_label: Option<String>,
    active_tabs: u64,
    profile_count: usize,
}

impl StatusBrowserIdentity {
    fn browser_name(&self) -> Option<&str> {
        self.browser_label.as_deref().or(self.browser_id.as_deref())
    }

    fn profile_name(&self) -> Option<&str> {
        self.profile_label.as_deref().or(self.profile_id.as_deref())
    }

    /// 人读一行：「正在操作：Chrome … （默认，3 tabs）」。
    fn summary(&self) -> Option<String> {
        let browser = self.browser_name()?;
        let profile = self
            .profile_name()
            .map(str::to_string)
            .unwrap_or_else(|| "Profile 未知".to_string());
        Some(format!(
            "正在操作：{browser}（{profile}，{} tabs）",
            self.active_tabs
        ))
    }
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 从 `/health.connection.profiles[]` 推导默认浏览器身份（纯函数，便于单测）。
///
/// 优先级：`is_default` → 与 `default_browser_id/default_profile_id` 匹配 →
/// 只有一个 profile 时取它。多个 profile 且没有默认标记时返回空：
/// status 宁可不显示，也不能把「当前默认」指到错误的浏览器上。
fn status_browser_identity(value: &Value) -> StatusBrowserIdentity {
    let profiles = value
        .pointer("/connection/profiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let default_browser_id = value
        .pointer("/connection/default_browser_id")
        .and_then(Value::as_str);
    let default_profile_id = value
        .pointer("/connection/default_profile_id")
        .and_then(Value::as_str);
    let active_tabs = value
        .pointer("/connection/active_tabs")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let chosen = profiles
        .iter()
        .find(|profile| {
            profile
                .get("is_default")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| {
            profiles.iter().find(|profile| {
                let browser_id = profile.get("browser_id").and_then(Value::as_str);
                let profile_id = profile.get("profile_id").and_then(Value::as_str);
                (default_browser_id.is_some() || default_profile_id.is_some())
                    && default_browser_id
                        .map(|expected| browser_id == Some(expected))
                        .unwrap_or(true)
                    && default_profile_id
                        .map(|expected| profile_id == Some(expected))
                        .unwrap_or(true)
            })
        })
        .or_else(|| {
            if profiles.len() == 1 {
                profiles.first()
            } else {
                None
            }
        });

    let mut identity = StatusBrowserIdentity {
        active_tabs,
        profile_count: profiles.len(),
        ..StatusBrowserIdentity::default()
    };
    if let Some(profile) = chosen {
        identity.browser_id = non_empty_text(profile.get("browser_id"));
        identity.browser_label = non_empty_text(profile.get("browser_label"));
        identity.profile_id = non_empty_text(profile.get("profile_id"));
        identity.profile_label = non_empty_text(profile.get("profile_label"));
    } else if default_browser_id.is_some() || default_profile_id.is_some() {
        // 旧 daemon 没有 profiles[]，但仍有 default_*_id：至少回 id，不伪造 label。
        identity.browser_id = default_browser_id.map(str::to_string);
        identity.profile_id = default_profile_id.map(str::to_string);
    }
    identity
}

/// 把浏览器身份字段挂到 status 回包（只增字段，不改 `summary`/`ok`/`healthy`）。
///
/// 注意：既有的 `summary` 是**健康分类**（`ready` / `daemon_not_running` …），
/// 被文档与调用方当作稳定枚举使用；人读的一行放在新增的 `browser_summary`，
/// 不能覆盖 `summary`（§12 契约 3：字段只增不改含义）。
fn attach_browser_identity(value: &mut Value) {
    let identity = status_browser_identity(value);
    value["browser"] = json!(identity.browser_name());
    value["browser_label"] = json!(identity.browser_label.clone());
    value["browser_id"] = json!(identity.browser_id.clone());
    value["profile"] = json!(identity.profile_name());
    value["profile_label"] = json!(identity.profile_label.clone());
    value["profile_id"] = json!(identity.profile_id.clone());
    value["browser_summary"] = json!(identity.summary());
    if identity.browser_name().is_none() {
        value["browser_unknown_hint"] = json!(if identity.profile_count > 1 {
            "有多个浏览器/Profile 已连接但没有默认目标：先 atb tabs 确认，或用 --browser/--profile 明确目标"
        } else {
            "尚未确定正在操作的浏览器：扩展未连接或没有默认目标；先运行 atb tabs 或 atb open <url>"
        });
    } else if identity.browser_label.is_none() {
        value["browser_unknown_hint"] =
            json!("扩展未上报可读浏览器标识（browser_label）；browser 已回落到 browser_id");
    }
}

// 可观测性：WS 鉴权现状（只增字段，不改任何信任判定逻辑）。
///
/// 优先用 daemon `/health` 的 `ws_security`（daemon 读的是同一份 `ws-trust.json`，
/// 还知道配置是否损坏）；daemon 没跑时退回**只读**本地文件 + `ATB_WS_TRUST_MODE`，
/// 让「daemon 都没起来」时也能回答「我到底有没有开鉴权」。
fn security_value(daemon_block: Option<&Value>) -> Value {
    let local = load_local_ws_trust();
    // 配置路径只做展示：定位「到底读的哪份白名单」，不参与任何判定。
    let config_path = ws_trust_path().ok().map(|path| path.display().to_string());
    let mode = daemon_block
        .and_then(|block| block.get("mode"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(crate::settings::ws_trust_mode);
    let token_configured = daemon_block
        .and_then(|block| block.get("token_required"))
        .and_then(Value::as_bool)
        .or_else(|| local.as_ref().ok().map(local_token_configured))
        .unwrap_or(false);
    let extension_ids = daemon_block
        .and_then(|block| block.get("trusted_ids"))
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .or_else(|| local.as_ref().ok().map(local_extension_ids))
        .unwrap_or(0);
    // 配置损坏时 daemon 会把原因写进 trust_config_error；daemon 不在时用本地读取错误代替。
    let config_error = daemon_block
        .and_then(|block| block.get("trust_config_error"))
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| local.as_ref().err().map(|err| json!(err)));

    let mut block = json!({
        "ws_trust_mode": mode,
        "token_configured": token_configured,
        "extension_ids": extension_ids,
        // 双向鉴权：daemon 在 WS 握手下发 ws_hello proof 的前提就是配了 token。
        "mutual_auth": if token_configured { "on" } else { "off" },
        // daemon = 结论来自 /health 的 ws_security（含配置损坏判定）；local = 只读了文件。
        "source": if daemon_block.is_some() { "daemon" } else { "local" },
        "config_path": config_path,
    });
    if let Some(err) = config_error {
        block["config_error"] = err;
    } else if mode == "compat" && !token_configured {
        block["warning"] = json!(
            "当前为兼容档且没有握手 token：任何本机进程都可能连入扩展端口/冒充 daemon；\
             建议 atb trust-extension <扩展ID> --token <随机值>，并在扩展选项页填入同一 token"
        );
    }
    block
}

/// 只读 `ws-trust.json`：缺失或空文件按「未配置」处理，解析失败按错误上抛（不改文件）。
fn load_local_ws_trust() -> std::result::Result<Value, String> {
    let path = ws_trust_path().map_err(|err| err.to_string())?;
    match fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| format!("{} 不是合法 JSON: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(format!("读取 {} 失败: {err}", path.display())),
    }
}

fn local_token_configured(value: &Value) -> bool {
    value
        .get("token")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.trim().is_empty())
}

fn local_extension_ids(value: &Value) -> usize {
    value
        .get("extension_ids")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

pub(crate) fn enrich_status(mut value: Value) -> Value {
    let running = value
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ready = value.get("ready").and_then(Value::as_bool).unwrap_or(false);
    let matched = value
        .pointer("/ports/extension/matched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let extension_connected = value
        .pointer("/connection/extension_connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active_tabs = value
        .pointer("/connection/active_tabs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // daemon 的插件端口没绑上（os error 10048 之类）：这时「插件未连接」是结果，不是原因。
    let bind_error = value
        .pointer("/ports/extension/bind_error")
        .and_then(Value::as_str)
        .filter(|err| !err.is_empty())
        .map(str::to_string);
    // 版本兼容性：daemon 会在 /health 里带 compatibility 块。
    let compatibility = value.get("compatibility").cloned().unwrap_or(Value::Null);
    let compat_ok = compatibility
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let compat_fixes: Vec<Value> = compatibility
        .get("fixes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (healthy, summary, message): (bool, &str, String) = if !running {
        (
            false,
            "daemon_not_running",
            format!(
                "daemon 未运行；执行 tabs/open/exec/scan 会自动启动，或运行 atb restart 手动启动。\
                 若 {PORT} 已被别的进程/残留 daemon 占用，先结束它再启动"
            ),
        )
    } else if let Some(err) = bind_error {
        (
            false,
            "extension_port_bind_failed",
            format!(
                "插件端口没有监听：{err}。最常见的原因是刚重启过 daemon，旧连接还在 TIME_WAIT\
                 （Windows 约 2 分钟）；稍等一会重试，或 atb set-extension-port <其它端口> 换端口"
            ),
        )
    } else if !matched {
        (
            false,
            "port_mismatch",
            "CLI 配置的插件端口和 daemon 实际监听端口不一致；运行 atb restart 让配置生效"
                .to_string(),
        )
    } else if !extension_connected {
        (
            false,
            "extension_not_connected",
            "浏览器插件未连接；确认 Chrome 已打开、插件已启用，且插件端口和 CLI 配置一致"
                .to_string(),
        )
    } else if active_tabs == 0 || !ready {
        (
            false,
            "no_active_tabs",
            "没有可用的普通网页标签页；请在 Chrome 打开一个 http/https 页面后重试".to_string(),
        )
    } else {
        (
            true,
            "ready",
            "daemon、插件连接和浏览器标签页均可用".to_string(),
        )
    };
    // 只有真的 healthy 才 ok：否则 status 恒 ok:true，`$?` 永远判不出问题。
    // 版本不配套不一定「不能用」：旧扩展仍支持 core 命令，只是 window/group/current
    // 这类新选择器会明确报 extension_outdated。所以 healthy 保持按功能判断，
    // 兼容性单独给 warning + next_steps，避免升级后所有人先看到一片红。
    value["ok"] = json!(healthy);
    value["healthy"] = json!(healthy);
    value["summary"] = json!(summary);
    // 失败时 `summary` 本身就是稳定分类（daemon_not_running /
    // extension_not_connected / port_mismatch…），直接复用为 error_code。
    if !healthy {
        value["error_code"] = json!(summary);
    }
    value["message"] = json!(message);
    value["compatibility"] = compatibility;
    if !compat_ok {
        value["upgrade_recommended"] = json!(true);
        value["warning"] =
            json!("扩展版本低于 CLI 期望：core 命令可用，window/group/current 选择器会提示更新");
        value["next_steps"] = json!(if compat_fixes.is_empty() {
            json!([
                "执行 atb restart",
                "打开 chrome://extensions（Edge 为 edge://extensions）重新加载扩展",
                "再跑 atb status 确认 compatibility.ok=true"
            ])
        } else {
            json!(compat_fixes)
        });
    }
    // 扩展是磁盘上的文件副本：改了不重新加载，Chrome 里跑的还是旧代码，
    // 表现就是「明明修好了但报错照旧」。这里把它变成显式的一行提示。
    if value
        .pointer("/extension/stale")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(hint) = value.pointer("/extension/hint").cloned() {
            value["warning"] = hint;
        }
    }
    // 的 `security` 块由 `status_value` 统一挂载（healthy / unhealthy 两条路径
    // 都要有），这里不再重复写，避免两个实现互相覆盖。
    value
}

pub(crate) fn print_logs(tail: usize) -> Result<()> {
    let path = config::daemon_log_path()?;
    if !path.exists() {
        return Err(usage_err!(
            "日志文件不存在或尚无日志: {}；先运行 atb tabs/open/exec/scan 或 restart 启动 daemon",
            path.display()
        ));
    }
    // `logs` 以前把裸文本逐行写 stdout，与「所有命令输出结构化 JSON」的
    // 契约冲突：Agent 的 JSON 解析器会直接报错。现在统一成
    // `{"ok":true,"lines":[...],"path":...,"truncated":bool}`。
    if tail == 0 {
        print_json(json!({
            "ok": true,
            "path": path.display().to_string(),
            "lines": Vec::<String>::new(),
            "truncated": false,
        }));
        return Ok(());
    }
    let (lines, truncated) =
        tail_lines(&path, tail).with_context(|| format!("读取日志文件失败: {}", path.display()))?;
    // EPIPE：write_stdout 自带忽略「下游已关闭管道」的处理，不会 panic。
    print_json(json!({
        "ok": true,
        "path": path.display().to_string(),
        "lines": lines,
        "truncated": truncated,
    }));
    Ok(())
}

// 只从文件尾部按块倒读，凑够 `tail` 行就停：日志涨到几百 MB 时不必整份读进内存。
///
/// 返回 `(lines, truncated)`：`truncated=true` 表示文件里还有更早的内容没返回
// 。
pub(crate) fn tail_lines(path: &Path, tail: usize) -> Result<(Vec<String>, bool)> {
    use std::io::{Seek, SeekFrom};

    const CHUNK: u64 = 64 * 1024;
    let mut file = fs::File::open(path)?;
    let mut end = file.metadata()?.len();
    if end == 0 {
        return Ok((Vec::new(), false));
    }
    // 多要一个换行：文件末尾没有换行时，最后一行也要算一行。
    let mut newlines = 0usize;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    while end > 0 && newlines < tail.saturating_add(1) {
        let start = end.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunks.push(chunk);
        end = start;
    }
    let mut buffer = Vec::new();
    for chunk in chunks.into_iter().rev() {
        buffer.extend_from_slice(&chunk);
    }
    // 没读到文件头时，第一条一定是被块边界切断的半行（也可能切断多字节字符），
    // 用 from_utf8_lossy 兜住非法字节后整行丢弃。
    let text = String::from_utf8_lossy(&buffer);
    let all: Vec<&str> = text.lines().collect();
    let skip = usize::from(end > 0 && !all.is_empty());
    let lines = &all[skip..];
    let from = lines.len().saturating_sub(tail);
    let truncated = end > 0 || from > 0;
    Ok((
        lines[from..].iter().map(|line| line.to_string()).collect(),
        truncated,
    ))
}

pub(crate) fn doctor_value() -> Result<Value> {
    let exe_ok = env::current_exe().map(|p| p.is_file()).unwrap_or(false);
    let config_path = config::config_path()?;
    let config_exists = config_path.exists();
    let config_result = config::load_existing();
    let (configured_port, config_ok, config_hint) = match config_result {
        Ok(cfg) => (Some(cfg.extension_port), true, Value::Null),
        // 配置读/解析失败时给出可执行修复建议（不再只是把错误原样贴出来）。
        Err(err) => (
            None,
            false,
            json!(format!(
                "{err}；检查 {} 的内容，或删除该文件后重跑（会自动重建默认配置）",
                config_path.display()
            )),
        ),
    };
    let api_reachable = tcp_listening(PORT);
    // 与 status 同口径：1s 会把「daemon 正忙」判成「health 不通」。
    let health = request("GET", "/health", None, HEALTH_PROBE_TIMEOUT).ok();
    let listening_port = health
        .as_ref()
        .and_then(|v| v.pointer("/ports/extension/listening"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok());
    let extension_port_to_check = listening_port.or(configured_port);
    let extension_port_ok = extension_port_to_check.map(tcp_listening).unwrap_or(false);
    let extension_connected = health
        .as_ref()
        .and_then(|v| v.pointer("/connection/extension_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active_tabs = health
        .as_ref()
        .and_then(|v| v.pointer("/connection/active_tabs"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let file_scheme_profiles = health
        .as_ref()
        .and_then(|v| v.pointer("/connection/profiles"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let extension_stale = health
        .as_ref()
        .and_then(|v| v.pointer("/extension/stale"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let extension_bundled = health
        .as_ref()
        .and_then(|v| v.pointer("/extension/bundled_version"))
        .cloned()
        .unwrap_or(Value::Null);
    let extension_versions = health
        .as_ref()
        .and_then(|v| v.pointer("/extension/connected_versions"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let running = health
        .as_ref()
        .and_then(|v| v.get("running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ready = health
        .as_ref()
        .and_then(|v| v.get("ready"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let port_conflict = configured_port == Some(PORT);
    let port_matched =
        configured_port.is_some() && listening_port.is_some() && configured_port == listening_port;
    let log_path = config::daemon_log_path()?;

    let mut checks = Vec::new();
    checks.push(json!({
        "name": "cli",
        "ok": exe_ok,
        "hint": if exe_ok { Value::Null } else { json!("当前 CLI 二进制不可执行或无法定位") }
    }));
    checks.push(json!({
        "name": "api",
        "ok": api_reachable,
        "host": HOST,
        "port": PORT,
        "hint": if api_reachable { Value::Null } else { json!("daemon API 未监听；doctor 不会自动启动 daemon，可运行 atb restart") }
    }));
    checks.push(json!({
        "name": "config",
        "ok": config_ok,
        "path": config_path,
        "exists": config_exists,
        "extension_port": configured_port,
        "hint": config_hint
    }));
    checks.push(json!({
        "name": "extension_port",
        "ok": extension_port_ok && !port_conflict,
        "configured_port": configured_port,
        "listening_port": listening_port,
        "port_matched": port_matched,
        "hint": extension_port_hint(configured_port, listening_port, extension_port_ok, port_conflict)
    }));
    checks.push(json!({
        "name": "health",
        "ok": running,
        "running": running,
        "ready": ready,
        "extension_connected": extension_connected,
        "active_tabs": active_tabs,
        "hint": if running { Value::Null } else { json!("daemon 未运行；运行 atb restart 或执行 tabs/open/exec/scan 自动启动") }
    }));
    checks.push(json!({
        "name": "extension_connected",
        "ok": extension_connected,
        "configured_port": configured_port,
        "listening_port": listening_port,
        "connected": extension_connected,
        "hint": if extension_connected { Value::Null } else { json!(format!("浏览器插件未连接；确认插件已启用，且插件端口为 {}", configured_port.unwrap_or(config::DEFAULT_EXTENSION_PORT))) }
    }));
    checks.push(json!({
        "name": "active_tabs",
        "ok": active_tabs > 0,
        "active_tabs": active_tabs,
        "hint": if active_tabs > 0 { Value::Null } else { json!("Chrome 需要至少打开一个普通 http/https/file 网页标签页") }
    }));
    checks.push(json!({
        "name": "extension_fresh",
        "ok": !extension_stale,
        "optional": true,
        "bundled_version": extension_bundled,
        "connected_versions": extension_versions,
        "hint": if extension_stale {
            json!("浏览器里的扩展版本低于磁盘上的版本：打开 chrome://extensions 对本扩展点一次「重新加载」，否则代码改动不会生效")
        } else {
            Value::Null
        }
    }));
    checks.push(json!({
        "name": "file_scheme_access",
        "ok": true,
        "optional": true,
        "profiles": file_scheme_profiles,
        "hint": "文件网址访问是可选能力；关闭时不影响普通 http/https 页面控制"
    }));

    // WS 鉴权档位的可观测性 + 加固建议。
    // `ok` 恒 true：这是「可以更安全」的建议，不是故障，不能把 doctor 变成非 0。
    let security = security_value(health.as_ref().and_then(|value| value.get("ws_security")));
    let insecure = security
        .get("ws_trust_mode")
        .and_then(Value::as_str)
        .map(|mode| mode == "compat")
        .unwrap_or(false)
        && security
            .get("token_configured")
            .and_then(Value::as_bool)
            .map(|configured| !configured)
            .unwrap_or(true);
    checks.push(json!({
        "name": "ws_trust",
        "ok": true,
        "optional": true,
        "status": if insecure { "warn" } else { "ok" },
        "ws_trust_mode": security.get("ws_trust_mode").cloned().unwrap_or(Value::Null),
        "token_configured": security.get("token_configured").cloned().unwrap_or(Value::Null),
        "extension_ids": security.get("extension_ids").cloned().unwrap_or(Value::Null),
        "mutual_auth": security.get("mutual_auth").cloned().unwrap_or(Value::Null),
        // fix 只给命令模板，不自动执行：token 必须在扩展选项页里手工填同一个值。
        "fix": if insecure {
            json!("atb trust-extension <扩展ID> --token <随机值>")
        } else {
            Value::Null
        },
        "hint": if insecure {
            json!("当前为兼容档且没有握手 token：本机任意进程都可能占用插件端口冒充 daemon/扩展，读到标签页数据。建议执行 fix 里的命令切到 token 档（需在扩展选项页填同一 token）")
        } else {
            Value::Null
        }
    }));

    let ok = checks.iter().all(|c| {
        c.get("optional").and_then(Value::as_bool).unwrap_or(false)
            || c.get("ok").and_then(Value::as_bool).unwrap_or(false)
    });
    // doctor 的 ok:false 也带 error_code；具体哪一项失败看 `checks[]`。
    let mut report = json!({
        "ok": ok,
        "checks": checks,
        "log_path": log_path,
        "log_exists": log_path.exists()
    });
    if !ok {
        report["error_code"] = json!("doctor_checks_failed");
        report["hint"] = json!(
            "逐项看 checks[] 里 ok:false 的项；常见处置：atb restart 拉起 daemon、\
             打开 chrome://extensions 重新加载扩展、确认扩展端口与 CLI 配置一致"
        );
    }
    Ok(report)
}

pub(crate) fn extension_port_hint(
    configured: Option<u16>,
    listening: Option<u16>,
    extension_port_ok: bool,
    port_conflict: bool,
) -> Value {
    if port_conflict {
        json!(format!(
            "插件端口不能和 API 端口 {PORT} 相同；运行 atb set-extension-port <port> 修改"
        ))
    } else if configured.is_some() && listening.is_some() && configured != listening {
        json!("CLI 配置和 daemon 实际监听端口不一致；运行 atb restart 让配置生效")
    } else if !extension_port_ok {
        json!("当前插件端口未监听；daemon 未启动或 WebSocket 端口启动失败")
    } else {
        Value::Null
    }
}

pub(crate) fn tcp_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("{HOST}:{port}").parse().expect("固定地址应可解析"),
        Duration::from_millis(300),
    )
    .is_ok()
}

pub(crate) fn cmd_status(is_async: bool) -> Result<()> {
    reject_async_if_set("status", is_async)?;
    let value = status_value()?;
    // stdout 仍是纯 JSON；人读的一行只进 stderr，不影响 JSON / 退出码。
    // 注意这里打印的是 `browser_summary`：既有 `summary` 是健康分类枚举。
    let human_summary = value
        .get("browser_summary")
        .and_then(Value::as_str)
        .map(str::to_string);
    print_json(value);
    if let Some(summary) = human_summary {
        use std::io::Write;
        let _ = writeln!(io::stderr(), "→ {summary}");
    }
    Ok(())
}

pub(crate) fn cmd_doctor(args: DoctorArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("doctor", is_async)?;
    let mut report = doctor_value()?;
    if args.fix {
        // --fix：在诊断结果之上，对可自动修复的问题逐条处理；
        // 每条输出 {fix, executed, result}，executed=false 表示只给出命令/指引。
        // 全局 --dry-run 时只列计划：不杀进程、不删 token。
        let dry_run = DRY_RUN.load(Ordering::Relaxed);
        report["fix_results"] = json!(run_doctor_fixes(&report, args.force, dry_run));
        if dry_run {
            report["dry_run"] = json!(true);
        }
    }
    print_json(report);
    Ok(())
}

/// 找出占用指定端口处于 LISTENING 状态的进程 PID。
///
/// Windows 用 `netstat -ano`（不引入新依赖，注意隐藏控制台窗口）；
/// macOS/Linux 用 `lsof -t -i:TCP:<port>`。拿不到就返回空表，调用方降级为打印命令。
fn find_port_pids(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Ok(output) = Command::new("netstat")
            .arg("-ano")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        // netstat 行形如：TCP  127.0.0.1:18767  0.0.0.0:0  LISTENING  12345
        // 列：协议 / 本地地址 / 远端地址 / 状态 / PID
        let needle = format!(":{port}");
        let mut pids: Vec<u32> = Vec::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 || fields[0] != "TCP" || fields[3] != "LISTENING" {
                continue;
            }
            // 只匹配本地地址列，避免远端地址碰巧带同端口号的误杀。
            let local = fields[1].trim_end_matches(']');
            if local.ends_with(&needle) {
                if let Ok(pid) = fields[4].parse::<u32>() {
                    pids.push(pid);
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }
    #[cfg(not(windows))]
    {
        let Ok(output) = Command::new("lsof")
            .args(["-t", &format!("-i:TCP:{port}")])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect()
    }
}

/// 解析扩展源目录（assets/atb_bridge）的实际路径，用于「扩展过旧」的重载指引。
/// 与 `install-skill` 的解析口径一致：ATB_PACKAGE_DIR → exe 祖先目录 → cwd。
fn resolve_extension_source_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(package_dir) = crate::settings::package_dir() {
        candidates.push(package_dir.join("assets/atb_bridge"));
    }
    if let Ok(exe) = env::current_exe() {
        for ancestor in exe.ancestors().take(6) {
            candidates.push(ancestor.join("assets/atb_bridge"));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("assets/atb_bridge"));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("manifest.json").is_file())
}

/// `doctor --fix` 的执行体：从诊断报告里挑出可自动修复的问题逐条处理。
///
/// 三类修复（与任务口径一致）：
/// - 旧 daemon 占端口：先用 netstat/lsof 找 PID；带 `--force` 才真杀（并逐个核对进程
///   身份），不带时只打印要执行的命令（杀进程影响面大，必须显式授权）；
/// - API token 失配：**只有** `/health` 明确返回 `invalid_api_token` 时才删
// `~/.atb/api-token`，新 daemon 启动时会刷新；
/// - 扩展过旧：只能人工在 chrome://extensions 点「重新加载」，输出带实际路径的指引。
///
// `dry_run`（全局 `--dry-run`）：只列计划，不杀进程、不删 token。
fn run_doctor_fixes(report: &Value, force: bool, dry_run: bool) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    // 场景一：端口在监听但 /health 拿不到（旧 daemon 占端口，或 daemon 正忙超时）。
    // token 失配单独判定，不能从「/health 失败」推出来。
    let alive = is_server_alive();
    let health_reachable = alive && request("GET", "/health", None, 2.0).is_ok();
    if alive && !health_reachable {
        // a) 旧 daemon 占端口
        let pids = find_port_pids(PORT);
        if pids.is_empty() {
            results.push(json!({
                "fix": "stale_daemon_on_port",
                "executed": false,
                "result": {
                    "port": PORT,
                    "pids": [],
                    "hint": format!("未从 netstat/lsof 解析到 PID；请手动执行 netstat -ano | findstr {PORT} 查看占用进程")
                }
            }));
        } else {
            let pid_list = pids
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            #[cfg(windows)]
            let command = format!("taskkill /PID {pid_list} /F");
            #[cfg(not(windows))]
            let command = format!("kill -9 {pid_list}");
            if force && !dry_run {
                // 强杀前逐个核对进程身份：占端口的可能是用户自己的程序，
                // 也可能是「daemon 正忙导致 /health 超时」——杀错进程的代价极高。
                let attempts: Vec<KillOutcome> =
                    pids.iter().map(|pid| kill_verified_pid(*pid)).collect();
                let killed: Vec<u32> = attempts
                    .iter()
                    .filter(|outcome| outcome.killed)
                    .map(|outcome| outcome.pid)
                    .collect();
                let skipped: Vec<Value> = attempts
                    .iter()
                    .filter(|outcome| !outcome.killed)
                    .map(KillOutcome::to_json)
                    .collect();
                let killed_any = !killed.is_empty();
                let killed_count = killed.len();
                results.push(json!({
                    "fix": "stale_daemon_on_port",
                    "executed": killed_any,
                    "result": {
                        "port": PORT,
                        "killed": killed,
                        "attempts": attempts.iter().map(KillOutcome::to_json).collect::<Vec<_>>(),
                        "skipped": skipped,
                        "identity_checked": true,
                        // ：两段文案以前是反的（killed_any=true 却说
                        // 「未强杀」），与 executed 字段自相矛盾。这里按 killed_any 正向
                        // 生成，并把实际结束的进程数写进文案。
                        "hint": if killed_any {
                            json!(format!(
                                "已结束 {killed_count} 个占用 {PORT} 的进程；执行 atb restart 拉起新 daemon，再 atb status 确认"
                            ))
                        } else {
                            json!("占用端口的进程没有通过身份核对，未强杀；请人工核查占用者")
                        }
                    }
                }));
            } else if force && dry_run {
                results.push(json!({
                    "fix": "stale_daemon_on_port",
                    "executed": false,
                    "result": {
                        "port": PORT,
                        "pids": pids,
                        "command": command,
                        "dry_run": true,
                        "hint": "dry-run：只列出计划；强杀前会核对进程身份（必须包含本机 atb 可执行文件或 atb daemon 命令行）"
                    }
                }));
            } else {
                results.push(json!({
                    "fix": "stale_daemon_on_port",
                    "executed": false,
                    "result": {
                        "port": PORT,
                        "pids": pids,
                        "command": command,
                        "hint": "结束进程影响面大：加 --force 才会真正执行上面的命令"
                    }
                }));
            }
        }
    }
    // b) token 失配 → 删除 ~/.atb/api-token
    //
    // 只有 **结构化** 的 `invalid_api_token` 才算证据：以前只要「端口在听但
    // /health 失败」就删 token，而超时（daemon 正忙）、连接被重置、非 JSON 响应都会
    // 命中这条分支——删掉 token 并不会解决问题，反而让正常 daemon 在下次重启后
    // 与其它调用方的 token 失配。
    if daemon_token_mismatch() {
        let token_path = config::api_token_path();
        if dry_run {
            results.push(json!({
                "fix": "api_token_mismatch",
                "executed": false,
                "result": {
                    "dry_run": true,
                    "would_remove": token_path
                        .as_ref()
                        .ok()
                        .map(|path| json!(path.display().to_string()))
                        .unwrap_or(Value::Null),
                    "hint": "dry-run：不会删除 token；去掉 --dry-run 再执行 --fix",
                }
            }));
        } else {
            let token_result = match token_path {
                Ok(path) => {
                    if path.exists() {
                        match fs::remove_file(&path) {
                            Ok(()) => json!({
                                "removed": path,
                                "hint": "token 文件已删除；执行 atb restart（daemon 启动时会刷新 token），再 atb status 确认"
                            }),
                            Err(err) => {
                                json!({ "removed": Value::Null, "error": err.to_string(), "path": path })
                            }
                        }
                    } else {
                        json!({
                            "removed": Value::Null,
                            "hint": "token 文件本就不存在；残留 daemon 用的是别处配置，请核对占端口的进程"
                        })
                    }
                }
                Err(err) => json!({ "removed": Value::Null, "error": err.to_string() }),
            };
            results.push(json!({
                "fix": "api_token_mismatch",
                // removed 为非 null 才算真的执行了删除；文件不存在/读路径失败时如实报 false。
                "executed": matches!(token_result.get("removed"), Some(v) if !v.is_null()),
                "result": token_result,
            }));
        }
    }
    // c) 扩展过旧：人工步骤，只能给指引（带扩展源目录的实际路径）
    let extension_stale = report
        .get("checks")
        .and_then(Value::as_array)
        .and_then(|checks| {
            checks
                .iter()
                .find(|check| {
                    // 名字必须与 doctor_value 里注册的一致（`extension_fresh`）；
                    // 旧代码查 `extension_freshness`，于是「扩展过旧」的修复永远不出现。
                    check.get("name").and_then(Value::as_str) == Some("extension_fresh")
                })
                .map(|check| check.get("ok").and_then(Value::as_bool).unwrap_or(true))
        })
        .map(|ok| !ok)
        .unwrap_or(false);
    if extension_stale {
        let source_dir = resolve_extension_source_dir();
        // 先算出人读文本（避免 source_dir 被 format! 消耗后下面还要用）。
        let source_dir_text = source_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "随包的 assets/atb_bridge".to_string());
        results.push(json!({
            "fix": "extension_outdated",
            // Chrome 不提供「从外部触发扩展重载」的接口，这步只能人工执行。
            "executed": false,
            "result": {
                "steps": [
                    format!("用 {source_dir_text} 覆盖你加载的扩展目录"),
                    "打开 chrome://extensions（Edge 是 edge://extensions），对本扩展点「重新加载」",
                    "重跑 atb status 确认 extension.stale=false"
                ],
                "source_dir": source_dir
                    .map(|dir| json!(dir.display().to_string()))
                    .unwrap_or(Value::Null),
            }
        }));
    }
    // daemon 未在跑：没有可自动修复的动作，但值得明确说出来，避免 Agent 以为 --fix 漏做。
    if !alive {
        results.push(json!({
            "fix": "daemon_not_running",
            "executed": false,
            "result": {
                "hint": "daemon 未运行；执行 atb restart 或任意页面命令（tabs/open/exec）会自动拉起"
            }
        }));
    }
    if results.is_empty() {
        results.push(json!({
            "fix": "none",
            "executed": false,
            "result": "诊断全部通过，没有需要修复的问题"
        }));
    }
    results
}

/// 能力门禁阈值（CLI 侧写死并在此注明口径，避免 Agent 逐条命令踩坑才发现）：
/// - core（open/exec/tabs/close 等基础命令）：扩展连上即可用，无版本要求；
/// - open_session（open 的 --session / --group-title）：扩展 2.1+（daemon /open 校验口径）；
/// - group / window / current 选择器：扩展 3.5+（daemon 目标解析的提示口径）；
/// - challenge（挑战页自动求解）：扩展 5.0+（server/challenge.rs 的版本门禁）。
fn feature_min_versions() -> Vec<(&'static str, &'static str)> {
    vec![
        ("core", "0.0.0"),
        ("open_session", "2.1"),
        ("group", "3.5"),
        ("window", "3.5"),
        ("current", "3.5"),
        ("challenge", "5.0"),
    ]
}

/// 版本号比较：按 `.` 分段转数值逐段比（"2.10" > "2.1"），非数字段按 0 处理。
/// 与 daemon `server/open.rs::version_at_least` 同一套语义。
fn version_at_least(installed: &str, minimum: &str) -> bool {
    let parse = |version: &str| -> Vec<u64> {
        version
            .split('.')
            .map(|part| part.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let mut installed = parse(installed);
    let mut minimum = parse(minimum);
    let len = installed.len().max(minimum.len());
    installed.resize(len, 0);
    minimum.resize(len, 0);
    installed >= minimum
}

/// `atb capabilities`：一次探测 daemon/扩展版本与能力门禁。
///
/// 目的（实测反馈）：Agent 先探测再动手，而不是踩到 null/0 的响应才知道
/// 当前扩展不支持某个选择器/能力。
pub(crate) fn cmd_capabilities(is_async: bool) -> Result<()> {
    reject_async_if_set("capabilities", is_async)?;
    // daemon 不在时给明确错误：这不是「features 全 false」，是压根没得探测。
    //
    // 环境故障不是「命令写错了」。以前走 `usage_failure` → 退出码 2，与
    // `error_code=daemon_not_running`（不在 is_usage_error_code 白名单）语义矛盾。
    // 现在走 `print_json(ok:false)`，退出码由 print_json 判为 1（可重启/可重试）。
    if !is_server_alive() {
        print_json(json!({
            "ok": false,
            "error_code": "daemon_not_running",
            "error": "daemon 未运行，无法探测能力",
            "hint": "执行 atb restart 或任意页面命令（tabs/open/exec）自动拉起，再运行 atb capabilities",
        }));
        return Ok(());
    }
    let health = match request("GET", "/health", None, 5.0) {
        Ok(value) => value,
        Err(err) => {
            let code = health_failure_code(&err).unwrap_or("daemon_token_mismatch");
            print_json(json!({
                "ok": false,
                "error_code": code,
                "error": format!("{PORT} 有进程在监听，但 /health 不可达：{err}"),
                "hint": format!(
                    "通常是残留的旧 daemon：netstat -ano | findstr {PORT} 找 PID 后 taskkill /PID <pid> /F，或运行 atb doctor --fix"
                ),
            }));
            return Ok(());
        }
    };
    let daemon_version = health
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let extension_connected = health
        .pointer("/connection/extension_connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 扩展版本取 connected_versions 里的最大值：多浏览器并存时按最高的算，
    // 低于阈值的实例会在具体命令上单独报错，这里回答的是「最好情况能干什么」。
    let extension_version: Option<String> = health
        .pointer("/extension/connected_versions")
        .and_then(Value::as_array)
        .map(|versions| {
            versions
                .iter()
                .filter_map(Value::as_str)
                .max_by_key(|version| {
                    version
                        .split('.')
                        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
                        .collect::<Vec<_>>()
                })
                .map(str::to_string)
        })
        .unwrap_or(None);
    let installed = extension_version.as_deref().unwrap_or("");
    let mut features = serde_json::Map::new();
    let mut thresholds = serde_json::Map::new();
    for (feature, minimum) in feature_min_versions() {
        thresholds.insert(feature.to_string(), json!(minimum));
        // 未连接扩展时除 core 外全部 false：避免拿「未探测」冒充「支持」。
        let ok = extension_connected && version_at_least(installed, minimum);
        features.insert(feature.to_string(), json!(ok));
    }
    let compat_ok = health
        .pointer("/compatibility/ok")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut value = json!({
        "ok": extension_connected && compat_ok,
        "extension_connected": extension_connected,
        "extension_version": extension_version,
        "daemon_version": daemon_version,
        "features": features,
        "thresholds": thresholds,
    });
    if !compat_ok {
        value["error_code"] = json!("extension_outdated");
        value["hint"] =
            json!("扩展协议版本低于 daemon 期望：按 atb status 的 next_steps 更新扩展后重新探测");
    } else if !extension_connected {
        // ok:false 统一带稳定 error_code。
        value["error_code"] = json!("extension_not_connected");
        value["hint"] = json!("扩展未连接：确认浏览器已打开且扩展已启用，再运行 atb capabilities");
    }
    print_json(value);
    Ok(())
}

#[cfg(test)]
mod browser_identity_tests {
    use super::*;

    fn health_with_profiles(profiles: Value, active_tabs: u64) -> Value {
        json!({
            "ok": true,
            "running": true,
            "connection": {
                "extension_connected": true,
                "active_tabs": active_tabs,
                "default_browser_id": "b1",
                "default_profile_id": "p1",
                "profiles": profiles,
            }
        })
    }

    #[test]
    fn identity_prefers_default_profile_flag() {
        let health = health_with_profiles(
            json!([
                {"browser_id":"b1","browser_label":"Chrome 156 · Windows","profile_id":"p1","profile_label":"默认","is_default":true},
                {"browser_id":"b2","browser_label":"Edge · Windows","profile_id":"p2","profile_label":"工作","is_default":false}
            ]),
            3,
        );
        let identity = status_browser_identity(&health);
        assert_eq!(identity.browser_name(), Some("Chrome 156 · Windows"));
        assert_eq!(identity.profile_name(), Some("默认"));
        assert_eq!(
            identity.summary().as_deref(),
            Some("正在操作：Chrome 156 · Windows（默认，3 tabs）")
        );
    }

    #[test]
    fn identity_uses_default_ids_when_profiles_missing() {
        // 旧 daemon 没有 profiles[]：只回 id，不伪造 label / summary 里的可读名。
        let health = json!({
            "connection": {
                "active_tabs": 1,
                "default_browser_id": "b9",
                "default_profile_id": "p9"
            }
        });
        let identity = status_browser_identity(&health);
        assert_eq!(identity.browser_name(), Some("b9"));
        assert_eq!(identity.profile_name(), Some("p9"));
        assert!(identity.browser_label.is_none());
    }

    #[test]
    fn identity_is_empty_when_multiple_profiles_have_no_default() {
        let health = json!({
            "connection": {
                "active_tabs": 2,
                "profiles": [
                    {"browser_id":"b1","browser_label":"Chrome","profile_id":"p1","profile_label":"默认"},
                    {"browser_id":"b2","browser_label":"Edge","profile_id":"p2","profile_label":"工作"}
                ]
            }
        });
        let identity = status_browser_identity(&health);
        assert!(identity.browser_name().is_none());
        assert!(identity.summary().is_none());
        assert_eq!(identity.profile_count, 2);
    }

    #[test]
    fn attach_keeps_existing_summary_and_adds_fields() {
        let mut value = health_with_profiles(
            json!([
                {"browser_id":"b1","browser_label":"Chrome","profile_id":"p1","profile_label":"默认","is_default":true}
            ]),
            1,
        );
        // 既有健康分类字段：attach 不能覆盖。
        value["summary"] = json!("ready");
        value["ok"] = json!(true);
        attach_browser_identity(&mut value);
        assert_eq!(value["summary"], "ready");
        assert_eq!(value["browser"], "Chrome");
        assert_eq!(value["browser_label"], "Chrome");
        assert_eq!(value["profile"], "默认");
        assert_eq!(value["browser_summary"], "正在操作：Chrome（默认，1 tabs）");
        assert!(value.get("browser_unknown_hint").is_none());
    }

    #[test]
    fn attach_reports_unknown_browser() {
        let mut value = json!({"ok": false, "healthy": false, "summary": "daemon_not_running"});
        attach_browser_identity(&mut value);
        assert!(value["browser"].is_null());
        assert!(value["browser_summary"].is_null());
        assert!(value["browser_unknown_hint"]
            .as_str()
            .unwrap()
            .contains("尚未确定正在操作的浏览器"));
        assert_eq!(value["summary"], "daemon_not_running");
    }

    #[test]
    fn attach_reports_missing_label() {
        let mut value = json!({
            "connection": {
                "active_tabs": 1,
                "profiles": [{"browser_id":"b1","profile_id":"p1","is_default":true}]
            }
        });
        attach_browser_identity(&mut value);
        assert_eq!(value["browser"], "b1");
        assert!(value["browser_label"].is_null());
        assert!(value["browser_unknown_hint"]
            .as_str()
            .unwrap()
            .contains("browser_label"));
    }
}

#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn tail_lines_reports_truncation() {
        // `truncated` 必须真实反映「还有更早的行没返回」。
        let path = std::env::temp_dir().join(format!("atb-log-tail-{}.log", std::process::id()));
        let content: String = (0..50).map(|index| format!("line{index}\n")).collect();
        fs::write(&path, content).expect("写临时日志");

        let (lines, truncated) = tail_lines(&path, 10).expect("读取尾部");
        assert_eq!(lines.len(), 10, "应只返回最后 10 行");
        assert_eq!(lines.last().map(String::as_str), Some("line49"));
        assert!(truncated, "文件还有更早内容时应标 truncated");

        let (all, not_truncated) = tail_lines(&path, 1000).expect("读取全部");
        assert_eq!(all.len(), 50);
        assert!(!not_truncated, "读完整个文件时不应标 truncated");

        let _ = fs::remove_file(&path);
    }
}
