//! 用户可调参数。
//!
//! 设计原则：
//! - **默认值保持向后兼容**：不设置任何环境变量时，行为与本轮修复前一致或更安全；
//! - **用户可覆盖**：所有阈值、档位、开关都能用 `ATB_*` 环境变量显式调参；
//! - **越界即夹紧**：非法值/超出值域的值回落到缺省，并夹到合法区间，绝不 panic。
//!
//! 值域与语义写在每个访问器上方；完整清单同步在 `README` 的「环境变量」章节。

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// 读取环境变量：`ATB_*` 优先，旧前缀 `AGENT_BROWSER_CLI_*` 继续生效。
///
/// 把参数集中到 `settings.rs` 时只认新前缀，旧前缀会静默失效（尤其
/// `NO_DEBUGGER` 这种安全开关）。这里统一走 `config::env_compat`，一个入口同时
/// 兼容两套前缀；非 UTF-8 值按 `to_string_lossy` 处理（配置值本就是 ASCII）。
fn read_env(name: &str) -> Option<String> {
    if let Ok(value) = env::var(name) {
        return Some(value);
    }
    name.strip_prefix("ATB_")
        .and_then(crate::config::env_compat)
        .map(|value| value.to_string_lossy().into_owned())
}

/// 读取字符串型环境变量；空串视为未设置。
pub(crate) fn env_string(name: &str, default: &str) -> String {
    match read_env(name) {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => default.to_string(),
    }
}

/// 读取布尔型环境变量；非法值回落缺省。
pub(crate) fn env_bool(name: &str, default: bool) -> bool {
    read_env(name)
        .and_then(|value| parse_bool(&value))
        .unwrap_or(default)
}

/// 读取浮点型环境变量并夹到 `[min, max]`；NaN/非法值回落缺省。
pub(crate) fn env_f64_clamped(name: &str, default: f64, min: f64, max: f64) -> f64 {
    match read_env(name).and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() => value.clamp(min, max),
        _ => default.clamp(min, max),
    }
}

/// 读取无符号整数环境变量并夹到 `[min, max]`；非法值回落缺省。
pub(crate) fn env_u64_clamped(name: &str, default: u64, min: u64, max: u64) -> u64 {
    match read_env(name).and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(value) => value.clamp(min, max),
        None => default.clamp(min, max),
    }
}

/// 读取 `usize` 环境变量并夹到 `[min, max]`。
pub(crate) fn env_usize_clamped(name: &str, default: usize, min: usize, max: usize) -> usize {
    env_u64_clamped(name, default as u64, min as u64, max as u64) as usize
}

/// 读取枚举型环境变量；不在白名单内回落缺省。
pub(crate) fn env_enum(name: &str, default: &str, allowed: &[&str]) -> String {
    match read_env(name) {
        Some(value) => {
            let value = value.trim().to_ascii_lowercase();
            if allowed.iter().any(|candidate| *candidate == value) {
                value
            } else {
                default.to_string()
            }
        }
        None => default.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 结果语义
// ---------------------------------------------------------------------------

/// `ATB_RESULT_STRICT`：组合命令（`--wait-js`、broadcast、scrape 汇总等）的严格度。
///
/// - `off`：子结论失败不改变顶层 `ok`（旧行为，不推荐）；
/// - `warn`（默认）：顶层 `ok:false` + `error_code`，但保留已产生的结果；
/// - `strict`：与 `warn` 相同，并让 CLI 退出码固定为非零（CI 门禁用）。
pub(crate) fn result_strict() -> String {
    env_enum("ATB_RESULT_STRICT", "warn", &["off", "warn", "strict"])
}

// ---------------------------------------------------------------------------
// 超时预算
// ---------------------------------------------------------------------------

/// `ATB_WAIT_MAX_TOTAL_SEC`：`wait` / `--wait-js` 单条命令的总预算上限（秒）。
/// 值域 `1..=3600`，默认 `600`（与动作类命令上限一致）。
pub(crate) fn wait_max_total_sec() -> f64 {
    env_f64_clamped("ATB_WAIT_MAX_TOTAL_SEC", 600.0, 1.0, 3600.0)
}

/// `ATB_WAIT_MIN_INTERVAL_MS`：`wait --interval` 的最小值（毫秒）。
/// 值域 `1..=60000`，默认 `50`；防止 `interval=0` 忙轮询。
pub(crate) fn wait_min_interval_ms() -> u64 {
    env_u64_clamped("ATB_WAIT_MIN_INTERVAL_MS", 50, 1, 60_000)
}

/// `ATB_CHALLENGE_MAX_IFRAMES`：单次挑战探测最多处理的 iframe 数。
/// 值域 `1..=64`，默认 `8`；超出按可视面积取前 K 个并写进 diag。
pub(crate) fn challenge_max_iframes() -> usize {
    env_usize_clamped("ATB_CHALLENGE_MAX_IFRAMES", 8, 1, 64)
}

/// `ATB_CHALLENGE_CLICK_TIMEOUT_SEC`：坐标点击整条链的硬上限（秒）。
/// 值域 `1..=120`，默认 `8`；与调用方 `--timeout` 取小。
pub(crate) fn challenge_click_timeout_sec() -> f64 {
    env_f64_clamped("ATB_CHALLENGE_CLICK_TIMEOUT_SEC", 8.0, 1.0, 120.0)
}

/// `ATB_HTTP_SLACK_SEC`：CLI 计算 HTTP 超时时在业务超时之上追加的余量（秒）。
/// 值域 `0..=60`，默认 `10`；统一各命令的「daemon 还在跑、CLI 先断」问题。
pub(crate) fn http_slack_sec() -> f64 {
    env_f64_clamped("ATB_HTTP_SLACK_SEC", 10.0, 0.0, 60.0)
}

/// `ATB_DIAG_DEPTH`：诊断信息里保留的底层错误条数。
/// 值域 `0..=20`，默认 `3`；`0` 表示只保留一句归因。
pub(crate) fn diag_depth() -> usize {
    env_usize_clamped("ATB_DIAG_DEPTH", 3, 0, 20)
}

// ---------------------------------------------------------------------------
// 文件写入与隐私
// ---------------------------------------------------------------------------

/// `ATB_IO_OVERWRITE`：`--out` 等文件输出的覆盖策略。
///
/// - `backup`（默认）：覆盖前把旧文件改名为 `<name>.<ts>.bak`；
/// - `force`：直接覆盖（旧行为，脚本化场景）；
/// - `ask`：存在即报 `output_exists`，要求显式 `--overwrite`；
/// - `append`：追加写（仅文本类输出支持）。
pub(crate) fn io_overwrite() -> String {
    env_enum(
        "ATB_IO_OVERWRITE",
        "backup",
        &["backup", "force", "ask", "append"],
    )
}

/// `ATB_IO_SANITIZE_FILENAME`：下载/落盘文件名净化档位。
/// `strict`（默认）取 basename 并拒绝 `..`、分隔符、Windows 保留名；`compat` 只告警。
pub(crate) fn io_sanitize_filename() -> String {
    env_enum("ATB_IO_SANITIZE_FILENAME", "strict", &["strict", "compat"])
}

/// `ATB_IO_MAX_OUTPUT_MB`：单个输出文件/内存缓冲的字节上限（MB）。
/// 值域 `1..=512`，默认 `50`（与 PDF 上限一致）。
pub(crate) fn io_max_output_bytes() -> usize {
    env_usize_clamped("ATB_IO_MAX_OUTPUT_MB", 50, 1, 512) * 1024 * 1024
}

// `ATB_EXEC_MAX_BYTES`：`exec` 回包在 daemon 侧的默认大小预算。
///
/// 只在不传 `--max-bytes` 时生效：直连 API 的调用方从来不带这个字段，旧实现
/// 等于完全不截断，一条 `exec` 就能把整页 innerHTML 之类的几百 MB 回包打给客户端。
///
/// 值域：`0` 表示**不限**；其余夹到 `4 KiB..=256 MiB`，默认 `1 MiB`。
/// 口径与 `--max-bytes` 一致，按**字符**计（字段名沿用历史叫法只为线协议兼容），
/// 所以中文页面的实际字节上限约为 3×该值。
pub(crate) fn exec_max_bytes() -> usize {
    const DEFAULT_BYTES: u64 = 1024 * 1024;
    const MIN_BYTES: u64 = 4 * 1024;
    const MAX_BYTES: u64 = 256 * 1024 * 1024;
    let raw = match env::var("ATB_EXEC_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        Some(value) => value,
        None => DEFAULT_BYTES,
    };
    if raw == 0 {
        return 0;
    }
    raw.clamp(MIN_BYTES, MAX_BYTES) as usize
}

/// `ATB_IDEMPOTENCY_PERSIST`：幂等缓存落盘策略。
///
/// - `redacted`（默认）：只落 `{key, scope, at, status}` 元数据，不落响应体；
/// - `full`：落完整响应（需用户显式知情，供审计/回放）；
/// - `off`：纯内存，不落盘。
pub(crate) fn idempotency_persist() -> String {
    env_enum(
        "ATB_IDEMPOTENCY_PERSIST",
        "redacted",
        &["redacted", "full", "off"],
    )
}

// ---------------------------------------------------------------------------
// 多智能体与会话
// ---------------------------------------------------------------------------

/// `ATB_SESSION_ISOLATION`：具名 agent 无自己的目标时是否允许回退全局默认。
/// `legacy`（默认，兼容旧 skill）/ `strict`（返回 `agent_no_default`）。
pub(crate) fn session_isolation() -> String {
    env_enum("ATB_SESSION_ISOLATION", "legacy", &["legacy", "strict"])
}

/// `ATB_MULTI_SCOPE`：`/multi` 的默认解析范围。
/// `agent`（默认，只解析本 agent 打开的标签）/ `all`（显式跨 agent）。
pub(crate) fn multi_scope_default() -> String {
    env_enum("ATB_MULTI_SCOPE", "agent", &["agent", "all"])
}

/// `ATB_DOWNLOAD_AUTO_CLAIM`：`download-wait` 无 `--id` 时是否自动认领最近下载。
/// 默认 `true`（兼容旧用法）；`false` 要求显式 `--id`。
pub(crate) fn download_auto_claim() -> bool {
    env_bool("ATB_DOWNLOAD_AUTO_CLAIM", true)
}

/// `ATB_DOWNLOAD_CLAIM_TTL_SEC`：自动认领的「近期完成」窗口（秒）。
/// 值域 `30..=900`，默认 `600`。
pub(crate) fn download_claim_ttl_sec() -> u64 {
    env_u64_clamped("ATB_DOWNLOAD_CLAIM_TTL_SEC", 600, 30, 900)
}

// ---------------------------------------------------------------------------
// WS 信任档位
// ---------------------------------------------------------------------------

/// `ATB_WS_TRUST_MODE`：扩展 WS 的握手信任档位。
///
/// - `compat`（默认）：沿用旧行为（Origin 前缀 + 可选 token），并在 `status` 里给出告警；
/// - `whitelist`：要求扩展 ID 在 `ws-trust.json` 白名单内；
/// - `token`：要求握手 token；
/// - `both`：白名单与 token 同时满足（共用机器的推荐档）。
pub(crate) fn ws_trust_mode() -> String {
    env_enum(
        "ATB_WS_TRUST_MODE",
        "compat",
        &["compat", "whitelist", "token", "both"],
    )
}

// ---------------------------------------------------------------------------
// 启动 / 生命周期 / 进程（2026-11-06 收拢：原散落于 boot / run / launch / config / cli）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 调试器 / 扩展 / 会话 / WS
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 幂等 / 网络 / 挑战 / 截图 / 监控 / 下载 / CLI 开关
// ---------------------------------------------------------------------------

/// 读取可选字符串：未设置或空白返回 `None`（`ATB_*` 的空值一律视同未设置）。
/// 与 `env_string` 一样走 `read_env`，旧前缀继续生效。
fn env_opt_string(name: &str) -> Option<String> {
    read_env(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `ATB_AGENT`（旧名 `AGENT_BROWSER_CLI_AGENT`）：显式调用方身份。
pub(crate) fn agent_from_env() -> Option<String> {
    env_opt_string("ATB_AGENT").or_else(|| env_opt_string("AGENT_BROWSER_CLI_AGENT"))
}

/// `ATB_HOME`：配置目录覆盖（mock 扩展自检靠它跑独立 daemon）。
pub(crate) fn atb_home() -> Option<PathBuf> {
    env_opt_string("ATB_HOME").map(PathBuf::from)
}

/// `ATB_PACKAGE_DIR`：npm 包目录覆盖（install-skill / 扩展资产定位）。
pub(crate) fn package_dir() -> Option<PathBuf> {
    env_opt_string("ATB_PACKAGE_DIR").map(PathBuf::from)
}

/// `ATB_BROWSER_EXE`：`launch --restart` 使用的浏览器可执行文件。
pub(crate) fn browser_exe() -> Option<String> {
    env_opt_string("ATB_BROWSER_EXE")
}

/// `ATB_EXTENSION_ID`：多份扩展共存时识别自身 iframe（默认自动探测）。
pub(crate) fn extension_id() -> String {
    env_opt_string("ATB_EXTENSION_ID").unwrap_or_default()
}

/// `ATB_FILE_HOSTS`：允许的 `file://host/...` 主机，逗号分隔；默认只允许 localhost。
pub(crate) fn file_hosts() -> String {
    env_string("ATB_FILE_HOSTS", "")
}

/// `ATB_EXIT0`（旧名 `AGENT_BROWSER_CLI_EXIT0`）：trim 后为 `1` / `true` 时，
/// `status` 恢复旧版恒 0 退出码（语义以文档为准：`yes` / `on` 不启用）。
pub(crate) fn exit0_enabled() -> bool {
    read_env("ATB_EXIT0")
        .map(|value| matches!(value.trim(), "1" | "true"))
        .unwrap_or(false)
}

/// `ATB_FORCE_ASSUME_ATB`：`restart --force` / `doctor --fix` 的身份判据。
/// 值域 `strict | loose`，默认 `strict`（只处理能确认属于本安装的进程）。
pub(crate) fn force_assume_atb() -> String {
    env_enum("ATB_FORCE_ASSUME_ATB", "strict", &["strict", "loose"])
}

/// `ATB_HTTP_NO_PROXY`：CLI 对回环地址跳过系统代理，默认开。
pub(crate) fn http_no_proxy() -> bool {
    env_bool("ATB_HTTP_NO_PROXY", true)
}

/// `ATB_REQUIRE_TAB`：写/破坏性命令必须显式 `--tab`，默认关。
pub(crate) fn require_tab() -> bool {
    env_bool("ATB_REQUIRE_TAB", false)
}

/// `ATB_ALLOW_RELATIVE_OUT`：`broadcast` 保留相对输出路径（旧行为），默认关。
pub(crate) fn allow_relative_out() -> bool {
    env_bool("ATB_ALLOW_RELATIVE_OUT", false)
}

/// `ATB_IDLE_TTL`：无扩展连接时的空闲退出秒数，默认 300；0 = 禁用空闲退出。
pub(crate) fn idle_shutdown_ttl_sec() -> Option<u64> {
    // 未设置 / 0 / 非法值都回落到 `None`（boot 用内置默认 300s），
    // 只有显式给出正整数才覆盖；不设上界（与原实现一致）。
    read_env("ATB_IDLE_TTL")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
}

/// `ATB_MAX_RUNNING_JOBS`：同时运行的 job 上限，默认 32；仅 >0 生效。
/// 进程级 `OnceLock` 缓存：`/health` 与拒绝分支必须看到同一个值。
pub(crate) fn max_running_jobs() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        read_env("ATB_MAX_RUNNING_JOBS")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(32)
    })
}

/// `ATB_PROC_CMD_TIMEOUT_MS`：进程枚举 / `tasklist` 类命令超时，默认 5000，值域 100..=60000。
pub(crate) fn proc_cmd_timeout_ms() -> u64 {
    env_u64_clamped("ATB_PROC_CMD_TIMEOUT_MS", 5_000, 100, 60_000)
}

/// `ATB_LAUNCH_SETTLE_MS`：`launch` 后等待窗口稳定，默认 800，值域 0..=10000。
pub(crate) fn launch_settle_ms() -> u64 {
    env_u64_clamped("ATB_LAUNCH_SETTLE_MS", 800, 0, 10_000)
}

/// `ATB_LAUNCH_CLOSE_WAIT_MS`：温和关闭浏览器的最长等待，默认 3000，值域 100..=60000。
pub(crate) fn launch_close_wait_ms() -> u64 {
    env_u64_clamped("ATB_LAUNCH_CLOSE_WAIT_MS", 3_000, 100, 60_000)
}

/// `ATB_LAUNCH_KILL_WAIT_MS`：强杀后的最长等待，默认 8000，值域 100..=120000。
pub(crate) fn launch_kill_wait_ms() -> u64 {
    env_u64_clamped("ATB_LAUNCH_KILL_WAIT_MS", 8_000, 100, 120_000)
}

/// `ATB_LAUNCH_POLL_MS`：启动期进程轮询间隔，默认 500，值域 20..=5000。
pub(crate) fn launch_poll_ms() -> u64 {
    env_u64_clamped("ATB_LAUNCH_POLL_MS", 500, 20, 5_000)
}

/// `ATB_LOCK_RETRIES`：锁文件重试次数，默认 40，值域 1..=500。
pub(crate) fn lock_retries() -> u64 {
    env_u64_clamped("ATB_LOCK_RETRIES", 40, 1, 500)
}

/// `ATB_LOCK_RETRY_MS`：锁重试间隔（毫秒），默认 25，值域 1..=2000。
pub(crate) fn lock_retry_ms() -> u64 {
    env_u64_clamped("ATB_LOCK_RETRY_MS", 25, 1, 2_000)
}

/// `ATB_NO_DEBUGGER`：彻底禁止 `chrome.debugger` attach，默认关。
pub(crate) fn no_debugger() -> bool {
    env_bool("ATB_NO_DEBUGGER", false)
}

/// `ATB_BROWSERS_SNAPSHOT_TTL_SEC`：`browsers` 快照短 TTL，默认 5；0 = 不缓存。
pub(crate) fn browsers_snapshot_ttl_sec() -> u64 {
    env_u64_clamped("ATB_BROWSERS_SNAPSHOT_TTL_SEC", 5, 0, 300)
}

/// `ATB_PREFERRED_MISS_LIMIT`：首选默认目标连续未命中多少次后放弃，默认 3。
pub(crate) fn preferred_miss_limit() -> u32 {
    env_u64_clamped("ATB_PREFERRED_MISS_LIMIT", 3, 1, 1_000) as u32
}

/// `ATB_PREFERRED_MISS_TTL_SEC`：首选默认目标未命中持续多久后放弃，默认 120。
pub(crate) fn preferred_miss_ttl_sec() -> u64 {
    env_u64_clamped("ATB_PREFERRED_MISS_TTL_SEC", 120, 1, 86_400)
}

/// `ATB_WS_OUTBOUND_MAX`：扩展下行控制通道容量，默认 512（8–65536）。
pub(crate) fn ws_outbound_max() -> usize {
    env_usize_clamped("ATB_WS_OUTBOUND_MAX", 512, 8, 65_536)
}

/// `ATB_IDEMPOTENCY_FILE_MAX_MB`：`idempotency.jsonl` 运行期上限，默认 32MB。
pub(crate) fn idempotency_file_max_mb() -> u64 {
    env_u64_clamped("ATB_IDEMPOTENCY_FILE_MAX_MB", 32, 1, 512)
}

/// `ATB_CAPTURE_MAX_PIXELS`：拼接截图 / 图片 PDF 的像素总量上限，默认 8000 万。
pub(crate) fn capture_max_pixels() -> u64 {
    env_u64_clamped("ATB_CAPTURE_MAX_PIXELS", 80_000_000, 1_000_000, 500_000_000)
}

/// `ATB_CHALLENGE_FOCUS_ON_NEED_HUMAN`：挑战需要人工时自动切前台，默认开。
pub(crate) fn challenge_focus_on_need_human() -> bool {
    env_bool("ATB_CHALLENGE_FOCUS_ON_NEED_HUMAN", true)
}

/// `ATB_CHALLENGE_SRCDOC_MIN_AREA`：srcdoc / about: iframe 纳入候选的最小面积，默认 2000，值域 0..=1e9。
pub(crate) fn challenge_srcdoc_min_area() -> f64 {
    env_f64_clamped("ATB_CHALLENGE_SRCDOC_MIN_AREA", 2_000.0, 0.0, 1.0e9)
}

/// `ATB_NET_RULE_MAX_BYTES`：单条规则 body 字节上限，默认 2MiB。
pub(crate) fn net_rule_max_bytes() -> usize {
    env_usize_clamped(
        "ATB_NET_RULE_MAX_BYTES",
        2 * 1024 * 1024,
        1024,
        16 * 1024 * 1024,
    )
}

/// `ATB_NET_RULE_MAX_RULES`：每 profile 规则条数上限，默认 64。
pub(crate) fn net_rule_max_rules() -> usize {
    env_usize_clamped("ATB_NET_RULE_MAX_RULES", 64, 1, 500)
}

/// `ATB_NET_MAX_ENTRIES`：network 缓冲上限，默认 5000。
pub(crate) fn net_max_entries() -> usize {
    env_usize_clamped("ATB_NET_MAX_ENTRIES", 5_000, 100, 100_000)
}

/// `ATB_CONSOLE_MAX_ENTRIES`：console 缓冲上限，默认 5000。
pub(crate) fn console_max_entries() -> usize {
    env_usize_clamped("ATB_CONSOLE_MAX_ENTRIES", 5_000, 100, 100_000)
}

/// `ATB_NET_LOG_ITEM_CHARS`：单条 url/body/headers 字符上限，默认 8192，值域 256..=1048576。
pub(crate) fn net_log_item_chars() -> usize {
    env_usize_clamped("ATB_NET_LOG_ITEM_CHARS", 8_192, 256, 1_048_576)
}

/// `ATB_MONITOR_SAMPLES`：`--monitor` 保留的样本条数，默认 50。
pub(crate) fn monitor_samples() -> usize {
    env_usize_clamped("ATB_MONITOR_SAMPLES", 50, 1, 500)
}

/// `ATB_HTML_KEEP_SCRIPTS`：HTML 优化保留 `<script>`（调试用），默认关。
pub(crate) fn html_keep_scripts() -> bool {
    env_bool("ATB_HTML_KEEP_SCRIPTS", false)
}

/// `ATB_DOWNLOAD_REGISTRY_DEPTH`：每个 agent 记住的下载 id 条数，默认 8。
pub(crate) fn download_registry_depth() -> usize {
    env_usize_clamped("ATB_DOWNLOAD_REGISTRY_DEPTH", 8, 1, 64)
}

/// `ATB_DOWNLOAD_WAIT_LIMIT`：`download-wait` 每轮查询条数，默认 200。
pub(crate) fn download_wait_limit() -> usize {
    env_usize_clamped("ATB_DOWNLOAD_WAIT_LIMIT", 200, 1, 500)
}

/// `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT`：`download-wait` 超时是否向浏览器发起取消，默认关。
pub(crate) fn download_cancel_on_timeout() -> bool {
    env_bool("ATB_DOWNLOAD_CANCEL_ON_TIMEOUT", false)
}

// ---------------------------------------------------------------------------
// 2026-11-13 改进轮：长任务 / 持久注入 / 目标提示 / 等待与截图
// ---------------------------------------------------------------------------

/// `ATB_EXEC_RESULT_TTL_SEC`：`GET /exec-result/:exec_id` 的迟到结果保留时长。
///
/// 旧实现写死 300s：长任务（建索引、批量写库）90s 超时后结果 5 分钟就丢，
/// 等用户看到提示时已经查不到了。值域 `60..=86400`，默认 `3600`。
pub(crate) fn exec_result_ttl_sec() -> u64 {
    env_u64_clamped("ATB_EXEC_RESULT_TTL_SEC", 3_600, 60, 86_400)
}

/// `ATB_INJECT_PERSIST_DEFAULT_TTL_SEC`：`inject --persist` 未显式给 `--ttl` 时的默认寿命。
///
/// 持久注入会与用户自己的油猴脚本抢执行，旧实现永久驻留且无提示。默认 2 小时；
/// 要永久驻留必须显式 `--persist-forever --i-know-this-is-long-lived`。
/// 值域 `60..=2592000`（30 天），默认 `7200`。
pub(crate) fn inject_persist_default_ttl_sec() -> u64 {
    env_u64_clamped("ATB_INJECT_PERSIST_DEFAULT_TTL_SEC", 7_200, 60, 2_592_000)
}

/// `ATB_INJECT_PERSIST_MAX_TTL_SEC`：`--ttl` 允许的最大值，默认 2592000（30 天）。
pub(crate) fn inject_persist_max_ttl_sec() -> u64 {
    env_u64_clamped("ATB_INJECT_PERSIST_MAX_TTL_SEC", 2_592_000, 60, 31_536_000)
}

/// `ATB_BROWSER_BANNER`：CLI 是否在 stderr 提示「当前操作的是哪个浏览器」。
///
/// - `off`：从不提示；
/// - `once`（默认）：同一 agent + 同一浏览器在 10 分钟内只提示一次；
/// - `always`：每条命令都提示。
pub(crate) fn browser_banner() -> String {
    env_enum("ATB_BROWSER_BANNER", "once", &["off", "once", "always"])
}

/// `ATB_OPEN_SPECIAL_WAIT_SEC`：`chrome://` / `chrome-extension://` / `about:` 等
/// 特殊页面的 `--wait-for` 默认预算（秒）。正常页面仍用 `--timeout`。
/// 值域 `0.5..=60`，默认 `3`。
pub(crate) fn open_special_wait_sec() -> f64 {
    env_f64_clamped("ATB_OPEN_SPECIAL_WAIT_SEC", 3.0, 0.5, 60.0)
}

/// `ATB_SCREENSHOT_SETTLE_MS`：`screenshot` 未显式给 `--settle-ms` 时的默认稳定等待。
/// 值域 `0..=10000`，默认 `0`（不额外等待，保持旧行为）。
pub(crate) fn screenshot_settle_ms() -> u64 {
    env_u64_clamped("ATB_SCREENSHOT_SETTLE_MS", 0, 0, 10_000)
}

/// `ATB_SCREENSHOT_NETWORK_IDLE_MS`：`--network-idle` 的静默窗口（毫秒）。
/// 值域 `50..=10000`，默认 `500`。
pub(crate) fn screenshot_network_idle_ms() -> u64 {
    env_u64_clamped("ATB_SCREENSHOT_NETWORK_IDLE_MS", 500, 50, 10_000)
}

/// `ATB_WAIT_DOM_STABLE_MS`：`wait --dom-stable` 的静默窗口（毫秒）。
/// 值域 `50..=10000`，默认 `500`。
pub(crate) fn wait_dom_stable_ms() -> u64 {
    env_u64_clamped("ATB_WAIT_DOM_STABLE_MS", 500, 50, 10_000)
}

/// `ATB_CONSOLE_HISTORY_SETTLE_MS`：`console history` 挂上 CDP 监听后等待缓冲
/// 事件回放的窗口（毫秒）。值域 `50..=5000`，默认 `300`。
pub(crate) fn console_history_settle_ms() -> u64 {
    env_u64_clamped("ATB_CONSOLE_HISTORY_SETTLE_MS", 300, 50, 5_000)
}

/// `ATB_JS_CHECK`：本地 JS 配平检查档位。
///
/// - `strict`（默认）：判为不配平时直接 exit 2；
/// - `warn`：不拦截，结果里带 `warning` 交给浏览器执行；
/// - `off`：完全跳过本地检查（`exec --no-preflight` 等价）。
pub(crate) fn js_check() -> String {
    env_enum("ATB_JS_CHECK", "strict", &["off", "warn", "strict"])
}

/// `ATB_TABS_LARGE_WARNING`：`tabs` 结果超过该条数时附加精简建议。
/// 值域 `10..=500`，默认 `50`。
pub(crate) fn tabs_large_warning() -> usize {
    env_usize_clamped("ATB_TABS_LARGE_WARNING", 50, 10, 500)
}

/// `ATB_HIDDEN_TAB_WARNING`：目标标签页在后台时是否附加
/// `warnings:["tab_hidden","timers_throttled"]` 与 `focus` 提示。
/// 默认开启；确实不关心可见性差异的批量脚本可设为 off。
pub(crate) fn hidden_tab_warning() -> bool {
    env_bool("ATB_HIDDEN_TAB_WARNING", true)
}

/// `ATB_JOB_STALL_SEC`：`job list/tail` 判定「卡住」的心跳超时（秒）。
/// 值域 `5..=3600`，默认 `60`。
pub(crate) fn job_stall_sec() -> u64 {
    env_u64_clamped("ATB_JOB_STALL_SEC", 60, 5, 3_600)
}

/// `ATB_SCRIPT_FILE_MAX_MB`：`exec --file` / `inject --file` 单个脚本文件上限（MiB）。
/// 值域 `1..=256`，默认 `32`（Windows 命令行 32K 上限的另一条替代路径）。
pub(crate) fn script_file_max_mb() -> usize {
    env_usize_clamped("ATB_SCRIPT_FILE_MAX_MB", 32, 1, 256)
}

/// `ATB_DOWNLOAD_CANCEL_ERASE`：取消后是否擦除下载记录，默认关。
pub(crate) fn download_cancel_erase() -> bool {
    env_bool("ATB_DOWNLOAD_CANCEL_ERASE", false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_falls_back_on_invalid_value() {
        // 不依赖进程环境（测试并行），只验证纯函数分支。
        assert_eq!(parse_bool(" YES "), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn defaults_are_within_documented_ranges() {
        assert!((1.0..=3600.0).contains(&wait_max_total_sec()));
        assert!((1..=60_000).contains(&wait_min_interval_ms()));
        assert!((1..=64).contains(&challenge_max_iframes()));
        assert!((1..=512).contains(&(io_max_output_bytes() / (1024 * 1024))));
    }

    #[test]
    fn consolidated_defaults_are_within_documented_ranges() {
        // 2026-11-06 收拢轮的默认值域断言（不依赖进程环境；CI 不设这些变量）。
        assert_eq!(max_running_jobs(), 32);
        assert_eq!(proc_cmd_timeout_ms(), 5_000);
        assert_eq!(launch_settle_ms(), 800);
        assert_eq!(launch_close_wait_ms(), 3_000);
        assert_eq!(launch_kill_wait_ms(), 8_000);
        assert_eq!(launch_poll_ms(), 500);
        assert!(http_no_proxy());
        assert_eq!(force_assume_atb(), "strict");
        assert_eq!(lock_retries(), 40);
        assert_eq!(lock_retry_ms(), 25);
        assert!(!no_debugger());
        assert_eq!(browsers_snapshot_ttl_sec(), 5);
        assert_eq!(preferred_miss_limit(), 3);
        assert_eq!(preferred_miss_ttl_sec(), 120);
        assert_eq!(ws_outbound_max(), 512);
        assert_eq!(idempotency_file_max_mb(), 32);
        assert_eq!(net_rule_max_bytes(), 2 * 1024 * 1024);
        assert_eq!(net_rule_max_rules(), 64);
        assert_eq!(net_max_entries(), 5000);
        assert_eq!(console_max_entries(), 5000);
        assert_eq!(net_log_item_chars(), 8192);
        assert!(challenge_focus_on_need_human());
        assert_eq!(challenge_srcdoc_min_area(), 2000.0);
        assert_eq!(capture_max_pixels(), 80_000_000);
        assert!(!html_keep_scripts());
        assert_eq!(file_hosts(), "");
        assert_eq!(monitor_samples(), 50);
        assert_eq!(download_registry_depth(), 8);
        assert_eq!(download_wait_limit(), 200);
        assert!(!download_cancel_on_timeout());
        assert!(!download_cancel_erase());
        assert!(!require_tab());
        assert!(!allow_relative_out());
        assert!(browser_exe().is_none());
    }
}
