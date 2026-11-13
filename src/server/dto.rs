use super::*;

/// `/scan` 请求体。`tabs_only`/`text_only` 这类布尔字段以前传成
/// `tabsOnly` 会被 serde 静默丢掉，命令照样成功却扫了整页——请求 DTO 一律
/// 拒绝未知字段，历史 camelCase 写法则用 `alias` 兜住。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScanRequest {
    #[serde(default, alias = "tabsOnly")]
    pub(crate) tabs_only: bool,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default, alias = "textOnly")]
    pub(crate) text_only: bool,
    /// 快速模式：单次注入的紧凑文本抽取，超大页面也不整页序列化。
    #[serde(default)]
    pub(crate) fast: bool,
    /// 节点/元素处理上限（0 表示默认）。
    #[serde(default, alias = "maxNodes")]
    pub(crate) max_nodes: Option<usize>,
    /// 页面内容抓取上限（秒）。
    #[serde(default)]
    pub(crate) timeout: Option<f64>,
}

/// `/exec` 请求体。
///
/// `deny_unknown_fields`：字段名写错（`waitTimeout` vs `wait_timeout`）
/// 以前会被静默忽略，命令照常成功、参数却没生效。旧调用方用过 camelCase 的
/// 历史字段用 `alias` 兜住，只有真正不存在的字段才报错。
/// 响应侧 DTO（扩展回包）不加 `deny_unknown_fields`：daemon 必须容忍新扩展
/// 多带字段，否则一次扩展升级就会让旧 daemon 全盘失败。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecRequest {
    #[serde(default)]
    pub(crate) script: String,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default, alias = "noMonitor")]
    pub(crate) no_monitor: bool,
    #[serde(alias = "waitJs")]
    pub(crate) wait_js: Option<String>,
    #[serde(default = "default_wait_timeout", alias = "waitTimeout")]
    pub(crate) wait_timeout: f64,
    #[serde(default = "default_wait_interval", alias = "waitInterval")]
    pub(crate) wait_interval: f64,
    /// 页面脚本执行上限（秒）。后台长任务需要能放宽，默认 60 秒。
    #[serde(default)]
    pub(crate) timeout: Option<f64>,
    /// 返回值上限（字节）。超出时返回截断预览，避免一条命令占满上下文。
    /// 不传时用 `ATB_EXEC_MAX_BYTES`。
    #[serde(default, alias = "maxBytes")]
    pub(crate) max_bytes: Option<usize>,
    /// 执行环境：MAIN（默认，可访问页面变量）或 ISOLATED（不受页面 CSP 影响）。
    #[serde(default)]
    pub(crate) world: Option<String>,
    /// 指定 iframe 的 frameId；配合 frames 命令使用。
    #[serde(default, alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    /// 在标签页的所有 frame 里执行。
    #[serde(default, alias = "allFrames")]
    pub(crate) all_frames: bool,
    /// CLI 侧已跳过本地 JS 配平检查（daemon 不再重复检查，仅记录在响应里）。
    #[serde(default, alias = "noPreflight")]
    pub(crate) no_preflight: bool,
}

/// `/open` 请求体。
///
/// `wait_for` / `warm` / `wait_for_timeout` 以前挂在 `open.rs` 的 `OpenRequestExt`
/// 上（`#[serde(flatten)]` 包一层）。`flatten` 与 `deny_unknown_fields` 天然冲突
/// （flatten 会把所有键先缓冲再分派，内层 deny 会把外层的字段全部当成未知），
/// 直接把这三个字段并进共享结构，调用方 JSON 形状完全不变。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenRequest {
    pub(crate) target: Option<String>,
    /// 一次打开多个目标：daemon 内部并发执行，只占一个 HTTP 往返。
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    pub(crate) url: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    #[serde(default = "default_active")]
    pub(crate) active: bool,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) session: Option<String>,
    #[serde(alias = "groupTitle")]
    pub(crate) group_title: Option<String>,
    #[serde(default)]
    pub(crate) window: bool,
    #[serde(default, alias = "allowFocus")]
    pub(crate) allow_focus: bool,
    /// 保持受控标签页不被 Chrome 内存节省机制丢弃，后台长任务必需。
    #[serde(default = "default_true", alias = "keepAlive")]
    pub(crate) keep_alive: bool,
    #[serde(default = "default_open_readiness_timeout", alias = "readinessTimeout")]
    pub(crate) readiness_timeout: f64,
    /// 打开后是否自动处理挑战页：auto（默认）| wait | off。
    #[serde(default)]
    pub(crate) challenge: Option<String>,
    /// 自动等待挑战的总超时（秒）。
    #[serde(default, alias = "challengeTimeout")]
    pub(crate) challenge_timeout: Option<f64>,
    // 打开后继续等待的页面条件：dom-quiet | selector:<CSS> | text:<文本>。
    /// `networkidle` 仍接受，但实现是**近似**的「resource 计数 + DOM 稳定」采样，
    // 不是 CDP 在途请求计数。
    #[serde(default, alias = "waitFor")]
    pub(crate) wait_for: Option<String>,
    // 打开前先 ping 一次扩展，唤醒 service worker。
    #[serde(default)]
    pub(crate) warm: bool,
    // `--wait-for` 的等待预算（秒）。CLI 已按它扩了 HTTP 预算，
    /// 服务端必须用**同一个**字段，否则「服务端还在等、CLI 先断开」的旧问题依旧。
    /// 缺省时回落到 `readiness_timeout`（旧口径）。
    #[serde(default, alias = "waitForTimeout")]
    pub(crate) wait_for_timeout: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupRequest {
    /// 不传就用当前请求的调用方身份。
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// 清理所有智能体打开过的标签页（默认只清自己）。
    #[serde(default, alias = "allAgents")]
    pub(crate) all_agents: bool,
    /// 只列出将要关闭的标签页。
    #[serde(default, alias = "dryRun")]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseRequest {
    #[serde(alias = "tabId")]
    pub(crate) tab_id: String,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    /// 解析 `--tab @N` 时的 URL / 标题快照。
    ///
    /// CLI 先换算 id 再逐个关闭，两次请求之间标签可能被切换/重排。带上下发时
    /// daemon 在真正关闭前复核一次，不一致就拒绝，避免关错标签。旧 CLI 不传，
    /// 字段为 None 时行为不变（向后兼容）。
    #[serde(default, alias = "expectUrl")]
    pub(crate) expect_url: Option<String>,
    #[serde(default, alias = "expectTitle")]
    pub(crate) expect_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) offset: usize,
    #[serde(default = "default_snapshot_limit")]
    pub(crate) limit: usize,
    #[serde(default)]
    pub(crate) details: bool,
    /// 强制走 DOM 快照（不走 CDP），用于 CDP 被其他扩展 iframe 挡住的页面。
    #[serde(default)]
    pub(crate) js: bool,
    /// 只保留可操作元素（按钮/链接/输入框等），显著缩小大树。
    #[serde(default, alias = "interactiveOnly")]
    pub(crate) interactive_only: bool,
    /// 页面侧 `page_snapshot.js` 的 `skipGeneric` 开关（JS 侧默认 true，
    /// 跳过 generic 角色节点）。入站字段是 snake_case `skip_generic`
    /// （兼容 camelCase `skipGeneric`）；下发给页面脚本时才叫 `skipGeneric`。
    /// 以前 daemon 从不下发、CLI 也无对应 flag，属于「不可达配置」；
    /// 这里接通 daemon 侧（HTTP 请求体带 `skip_generic:false` 保留 generic 节点）。
    /// CLI flag 需改 cmd 侧，不在本轮范围。CDP 路径不受影响。
    #[serde(default, alias = "skipGeneric")]
    pub(crate) skip_generic: Option<bool>,
    /// 返回节点上限，超出截断（默认 0 表示不限制）。
    #[serde(default, alias = "maxNodes")]
    pub(crate) max_nodes: usize,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetActionRequest {
    pub(crate) target: String,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) monitor: bool,
    #[serde(alias = "waitJs")]
    pub(crate) wait_js: Option<String>,
    #[serde(default = "default_wait_timeout", alias = "waitTimeout")]
    pub(crate) wait_timeout: f64,
    #[serde(default = "default_wait_interval", alias = "waitInterval")]
    pub(crate) wait_interval: f64,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FillRequest {
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) value: String,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) append: bool,
    #[serde(default)]
    pub(crate) clear: bool,
    #[serde(default)]
    pub(crate) has_value: bool,
    #[serde(default)]
    pub(crate) monitor: bool,
    #[serde(alias = "waitJs")]
    pub(crate) wait_js: Option<String>,
    #[serde(default = "default_wait_timeout", alias = "waitTimeout")]
    pub(crate) wait_timeout: f64,
    #[serde(default = "default_wait_interval", alias = "waitInterval")]
    pub(crate) wait_interval: f64,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendKeysRequest {
    #[serde(default, alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    pub(crate) keys: String,
    pub(crate) target: Option<String>,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) monitor: bool,
    #[serde(alias = "waitJs")]
    pub(crate) wait_js: Option<String>,
    #[serde(default = "default_wait_timeout", alias = "waitTimeout")]
    pub(crate) wait_timeout: f64,
    #[serde(default = "default_wait_interval", alias = "waitInterval")]
    pub(crate) wait_interval: f64,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScreenshotRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) selector: Option<String>,
    pub(crate) out: Option<PathBuf>,
    #[serde(default = "default_screenshot_format")]
    pub(crate) format: String,
    pub(crate) quality: Option<u8>,
    #[serde(default, alias = "fullPage")]
    pub(crate) full_page: bool,
    /// 整页拼图的设备像素总高上限（CDP 不可用时的回落通道使用）。
    #[serde(default, alias = "maxHeightPx")]
    pub(crate) max_height_px: Option<u32>,
    /// 截图前额外等待（毫秒）；None 时取 ATB_SCREENSHOT_SETTLE_MS。
    #[serde(default, alias = "settleMs")]
    pub(crate) settle_ms: Option<u64>,
    /// 截图前先等该选择器出现（支持 `host >>> inner`）。
    #[serde(default, alias = "waitSelector")]
    pub(crate) wait_selector: Option<String>,
    /// 截图前等到在途请求归零并静默；值是静默窗口毫秒，0/缺省用配置默认。
    #[serde(default, alias = "networkIdle")]
    pub(crate) network_idle: Option<u64>,
    /// CDP 不可用时直接失败，不回落 captureVisibleTab。
    #[serde(default, alias = "requireCdp")]
    pub(crate) require_cdp: bool,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

/// `atb focus`：把目标标签页切到前台。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FocusRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    /// true 时不把浏览器窗口抢到系统前台（只切标签页）。
    #[serde(default, alias = "noWindow")]
    pub(crate) no_window: bool,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

/// `atb use`：把当前 agent 的默认目标固定到指定标签页。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UseRequest {
    /// 目标规格（tab_id / @N / url: / title: / @latest）。
    pub(crate) target: String,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

/// `atb extensions`：列出已安装扩展（需要用户授权 management 权限）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtensionsRequest {
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default, alias = "includeDisabled")]
    pub(crate) include_disabled: bool,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavePdfRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) out: Option<PathBuf>,
    #[serde(default = "default_pdf_paper")]
    pub(crate) paper: String,
    #[serde(default)]
    pub(crate) landscape: bool,
    #[serde(default = "default_pdf_scale")]
    pub(crate) scale: f64,
    #[serde(default = "default_true", alias = "printBackground")]
    pub(crate) print_background: bool,
    /// 图片型 PDF 回落时整页拼图的高度上限（设备像素）。
    #[serde(default, alias = "maxHeightPx")]
    pub(crate) max_height_px: Option<u32>,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

/// `/tabs` 查询参数。
///
/// 这里**不能**加 `deny_unknown_fields`：`tabs.rs` 的 `TabsQueryExt` 用
/// `#[serde(flatten)]` 把它包在里面并追加了 `mine`，flatten 会把所有键先缓冲再
/// 分派，内层 deny 会把外层字段一并当未知（serde 的 flatten 与 deny 语义互斥）。
/// 未知参数仍由外层 wrapper 兜住，不在本结构上收紧。
#[derive(Debug, Deserialize, Default)]
pub(crate) struct TabsQuery {
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    /// 输出完整 URL（默认截断到 80 字符以节省 token）。
    ///
    /// `TabsQueryExt` 用 `#[serde(flatten)]` 组合本结构：serde_urlencoded 在
    /// flatten 路径上把所有值先缓冲成字符串，bool 直接反序列化会得到
    /// `invalid type: string "true", expected a boolean`（HTTP 400）。
    /// 自定义反序列化同时接受 true/false、"true"/"false"、1/0。
    #[serde(default, deserialize_with = "de_boolish")]
    pub(crate) full: bool,
}

/// 查询串里的布尔值：serde_urlencoded + flatten 只会给字符串，这里统一兼容。
pub(crate) fn de_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boolish {
        Bool(bool),
        Int(i64),
        Text(String),
    }
    Ok(match Boolish::deserialize(deserializer)? {
        Boolish::Bool(value) => value,
        Boolish::Int(value) => value != 0,
        Boolish::Text(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    })
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct CookiesRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    /// 不传则用当前页面 URL。
    pub(crate) url: Option<String>,
    /// list（默认）/ clear。clear 用于反复测试挑战页与登录态切换。
    pub(crate) action: Option<String>,
    /// clear 时限定域（含子域），不传则按 url（默认当前页面）清。
    pub(crate) domain: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadRequest {
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) files: Vec<String>,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default = "default_high_level_timeout")]
    pub(crate) timeout: f64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookupTabQuery {
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct TabtreeQuery {
    pub(crate) tab: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) full: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionTreeItem {
    pub(crate) browser_id: String,
    pub(crate) profile_id: String,
    pub(crate) profile_label: Option<String>,
    pub(crate) tab_id: String,
    pub(crate) session_key: String,
    pub(crate) title: String,
    pub(crate) url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileLabelRequest {
    pub(crate) label: Option<String>,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TabRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkListRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) filter: Option<String>,
    #[serde(default = "default_debug_list_limit")]
    pub(crate) limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkDetailRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(alias = "requestId")]
    pub(crate) request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsoleListRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) level: Option<String>,
    #[serde(default = "default_debug_list_limit")]
    pub(crate) limit: usize,
}

pub(crate) fn default_active() -> bool {
    true
}

pub(crate) fn default_wait_timeout() -> f64 {
    3.0
}

pub(crate) fn default_open_readiness_timeout() -> f64 {
    10.0
}

pub(crate) fn default_wait_interval() -> f64 {
    0.1
}

pub(crate) fn default_snapshot_limit() -> usize {
    200
}

pub(crate) fn default_high_level_timeout() -> f64 {
    30.0
}

pub(crate) fn default_screenshot_format() -> String {
    "png".to_string()
}

pub(crate) fn default_pdf_paper() -> String {
    "a4".to_string()
}

pub(crate) fn default_pdf_scale() -> f64 {
    1.0
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_debug_list_limit() -> usize {
    100
}

pub(crate) fn default_job_list_limit() -> usize {
    20
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobSubmitRequest {
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) args: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobListQuery {
    #[serde(default = "default_job_list_limit")]
    pub(crate) limit: usize,
}

/// `/run`、`/batch` 的单步。`args` 是自由 JSON（按 op 再分发到具体请求 DTO），
/// 所以未知字段校验发生在下一层；这里只拒绝 step 自身多带的键。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStep {
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) args: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRequest {
    pub(crate) steps: Vec<RunStep>,
    /// 某一步失败后是否终止后续步骤，默认终止。
    #[serde(default = "default_true", alias = "stopOnError")]
    pub(crate) stop_on_error: bool,
}

pub(crate) fn default_read_max_chars() -> usize {
    20000
}

pub(crate) fn default_find_limit() -> usize {
    50
}

pub(crate) fn default_batch_concurrency() -> usize {
    4
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadRequest {
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) selector: Option<String>,
    #[serde(default = "default_read_max_chars", alias = "maxChars")]
    pub(crate) max_chars: usize,
    #[serde(default)]
    pub(crate) links: bool,
    #[serde(default = "default_true")]
    pub(crate) forms: bool,
    /// 连隐藏内容一起返回（textContent 语义，不做可见性过滤）。
    #[serde(default, alias = "includeHidden")]
    pub(crate) include_hidden: bool,
    /// visible / all，等价于 --include-hidden 的显式写法。
    pub(crate) mode: Option<String>,
    /// 页面内文本抽取时间预算（毫秒）。
    #[serde(alias = "budgetMs")]
    pub(crate) budget_ms: Option<u64>,
    /// 单次读取最多处理多少个节点。
    #[serde(alias = "maxNodes")]
    pub(crate) max_nodes: Option<usize>,
    /// 在指定 iframe 内读取。
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    /// DOM 节点数阈值（`page_read.js` 的 `innerTextLimit`，页面侧默认 30000）。
    /// 节点数不超过该值时用 `innerText` 精确抽取；超过则退化为近似可见文本
    /// （`textMode:"visible-approx"`）。**不是字符数上限**——控字符量用 `max_chars`。
    #[serde(default, alias = "innerTextLimit")]
    pub(crate) inner_text_limit: Option<usize>,
    pub(crate) timeout: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct FindRequest {
    pub(crate) selector: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) xpath: Option<String>,
    #[serde(default = "default_find_limit")]
    pub(crate) limit: usize,
    #[serde(default = "default_true")]
    pub(crate) visible: bool,
    #[serde(default)]
    pub(crate) exact: bool,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    pub(crate) timeout: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionRequest {
    pub(crate) op: String,
    pub(crate) target: Option<Value>,
    #[serde(default)]
    pub(crate) params: Value,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    pub(crate) timeout: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitRequest {
    pub(crate) js: Option<String>,
    pub(crate) selector: Option<String>,
    pub(crate) text: Option<String>,
    #[serde(default = "default_wait_timeout")]
    pub(crate) timeout: f64,
    /// 轮询间隔（秒），与 exec 的 --wait-interval 一致。
    #[serde(default = "default_wait_interval")]
    pub(crate) interval: f64,
    #[serde(default)]
    pub(crate) visible: bool,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    /// 等 DOM 连续静默；值是静默窗口毫秒，0/缺省用 ATB_WAIT_DOM_STABLE_MS。
    #[serde(default, alias = "domStable")]
    pub(crate) dom_stable: Option<u64>,
    /// 等页面在途请求归零并静默；值同上。
    #[serde(default, alias = "networkIdle")]
    pub(crate) network_idle: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct InjectRequest {
    /// run | persist | remove | list
    pub(crate) action: Option<String>,
    pub(crate) code: Option<String>,
    pub(crate) name: Option<String>,
    /// MAIN（默认）或 ISOLATED
    pub(crate) world: Option<String>,
    pub(crate) matches: Option<Vec<String>>,
    #[serde(alias = "frameId")]
    pub(crate) frame_id: Option<i64>,
    #[serde(default, alias = "allFrames")]
    pub(crate) all_frames: bool,
    #[serde(alias = "switchTabId")]
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
    /// 持久注入寿命（秒）；None 时用 ATB_INJECT_PERSIST_DEFAULT_TTL_SEC。
    #[serde(alias = "ttlSec")]
    pub(crate) ttl_sec: Option<u64>,
    /// 持久注入永不过期（CLI 已要求显式确认）。
    #[serde(default, alias = "persistForever")]
    pub(crate) persist_forever: bool,
    /// 按油猴脚本环境包装：注入 GM shim。
    #[serde(default)]
    pub(crate) userscript: bool,
    /// GM_* 存储命名空间。
    #[serde(default, alias = "gmNamespace")]
    pub(crate) gm_namespace: Option<String>,
    /// 注入前先删除匹配元素。
    #[serde(default, alias = "cleanupSelectors")]
    pub(crate) cleanup_selectors: Vec<String>,
    /// 注入前执行的清理脚本。
    #[serde(default, alias = "cleanupCode")]
    pub(crate) cleanup_code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchRequest {
    pub(crate) steps: Vec<RunStep>,
    /// 并发度：同时在多个标签页上执行的上限。
    #[serde(default = "default_batch_concurrency")]
    pub(crate) concurrency: usize,
    /// CLI 与 `/run` 共用同一份步骤文件合并逻辑，会带上这个键；
    /// `/batch` 是并发执行，不按步中断，但必须接受该字段而不是报 422。
    #[serde(default = "default_true", alias = "stopOnError")]
    pub(crate) stop_on_error: bool,
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// 拼错/多余的字段必须报错，而不是静默丢掉（命令成功但参数没生效）。
    #[test]
    fn exec_request_rejects_unknown_field() {
        let err = serde_json::from_str::<ExecRequest>(r#"{"script":"1+1","waitTimout":5}"#)
            .expect_err("未知字段必须被拒绝");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    /// 历史 camelCase 写法仍要能解析（alias），否则老调用方会被直接打断。
    #[test]
    fn exec_request_accepts_legacy_camel_case() {
        let req: ExecRequest = serde_json::from_str(
            r#"{"script":"1+1","waitJs":"window.ok","waitTimeout":2.5,"maxBytes":100}"#,
        )
        .expect("camelCase 应该被 alias 接受");
        assert_eq!(req.wait_js.as_deref(), Some("window.ok"));
        assert_eq!(req.wait_timeout, 2.5);
        assert_eq!(req.max_bytes, Some(100));
    }

    /// CLI 的 `batch` 实际 payload 带 `stop_on_error`（与 `run` 共用步骤文件）：
    /// `/batch` 并发执行，但 DTO 必须接受该字段，否则整条命令会 422。
    #[test]
    fn batch_request_accepts_cli_payload() {
        let req: BatchRequest =
            serde_json::from_str(r#"{"steps":[],"concurrency":4,"stop_on_error":true}"#)
                .expect("batch payload");
        assert_eq!(req.concurrency, 4);
        assert!(req.stop_on_error);
    }

    /// CLI 的 `read` / `snapshot` 实际 payload（cmd_page.rs）必须原样被接受：
    /// 加 deny 之后这里一旦漂移，`atb read` 会直接 400。
    #[test]
    fn cli_page_payloads_still_parse() {
        let read: ReadRequest = serde_json::from_str(
            r#"{"selector":null,"max_chars":20000,"inner_text_limit":null,"links":false,
                "forms":true,"include_hidden":false,"budget_ms":null,"max_nodes":null,
                "frame_id":null,"switch_tab_id":"@1","browser":null,"profile":null,
                "timeout":30.0}"#,
        )
        .expect("read payload");
        assert_eq!(read.max_chars, 20000);
        assert!(read.forms);

        let snapshot: SnapshotRequest = serde_json::from_str(
            r#"{"switch_tab_id":null,"browser":null,"profile":null,"offset":0,"limit":200,
                "details":false,"js":false,"interactive_only":false,"skip_generic":true,
                "max_nodes":0,"timeout":30.0}"#,
        )
        .expect("snapshot payload");
        assert_eq!(snapshot.skip_generic, Some(true));

        let action: ActionRequest = serde_json::from_str(
            r#"{"op":"click","target":{"ref":"e1"},"params":{},"frame_id":null,
                "switch_tab_id":"@1","browser":null,"profile":null,"timeout":30.0}"#,
        )
        .expect("action payload");
        assert_eq!(action.op, "click");
    }

    /// 响应/扩展回包方向不走这些 DTO；这里只确认请求 DTO 的默认值与缺省行为不变。
    #[test]
    fn request_defaults_are_backward_compatible() {
        let req: ExecRequest = serde_json::from_str(r#"{"script":"1"}"#).expect("最小请求体");
        assert_eq!(req.wait_timeout, 3.0);
        assert_eq!(req.wait_interval, 0.1);
        assert!(!req.no_monitor);
        let open: OpenRequest =
            serde_json::from_str(r#"{"url":"https://example.com"}"#).expect("最小 open 请求体");
        assert!(open.keep_alive);
        assert!(open.active);
        assert!(open.wait_for.is_none());
        assert!(!open.warm);
    }
}
