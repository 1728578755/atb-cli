#[allow(unused_imports)]
use super::*;
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb wait --selector '.result' --timeout 10
  atb wait --text '加载完成' --visible
  atb wait --js 'document.readyState === "complete"'"#)]
pub(crate) struct WaitArgs {
    #[arg(long)]
    pub(crate) selector: Option<String>,
    #[arg(long)]
    pub(crate) text: Option<String>,
    #[arg(long)]
    pub(crate) js: Option<String>,
    /// 只等待元素可见（配合 --selector）
    #[arg(long)]
    pub(crate) visible: bool,
    #[arg(long, default_value_t = 10.0)]
    pub(crate) timeout: f64,
    #[arg(long, default_value_t = 0.1)]
    pub(crate) interval: f64,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 在指定 iframe 内等待
    #[arg(long)]
    pub(crate) frame: Option<i64>,
    /// 等 DOM 连续静默（默认 500ms，可用 `--dom-stable 800` 指定）；适合等框架渲染完
    #[arg(long = "dom-stable", num_args = 0..=1, default_missing_value = "0")]
    pub(crate) dom_stable: Option<u64>,
    /// 等页面在途请求归零并静默（默认 500ms，可用 `--network-idle 800` 指定）
    #[arg(long = "network-idle", num_args = 0..=1, default_missing_value = "0")]
    pub(crate) network_idle: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct ChallengeArgs {
    /// status（默认，识别）| wait（等自动放行）| solve（主动求解 + 等结果）| auto（只求解一次）|
    /// click（点一次，配合 --at）| widget（只返回组件位置与可点坐标）| focus（带到前台）
    #[arg(default_value = "status")]
    pub(crate) action: String,
    /// wait/solve 的总超时（秒）；不传时由服务端按 action 取默认（wait 30s，solve 120s）
    #[arg(long)]
    pub(crate) timeout: Option<f64>,
    /// wait 的检查间隔（秒）
    #[arg(long, default_value_t = 0.25)]
    pub(crate) interval: f64,
    /// wait 期间超过这么多秒还没放行，就把标签页带到前台交给人工（0 表示不聚焦）
    #[arg(long = "focus-after", default_value_t = 8.0)]
    pub(crate) focus_after: f64,
    /// 禁止自动聚焦（默认会在 focus-after 后把标签页带到前台）
    #[arg(long = "no-focus")]
    pub(crate) no_focus: bool,
    /// 等这么多秒仍被拦就整页 reload 一次再继续等（0 = 不 reload）
    #[arg(long = "reload-after", default_value_t = 0.0)]
    pub(crate) reload_after: f64,
    /// 点击后等待页面确认的毫秒数（默认由服务端取 250）
    #[arg(long = "confirm-ms")]
    pub(crate) confirm_ms: Option<f64>,
    /// 与 action=click 配合：直接点这个页面坐标（`x,y`）。
    /// 坐标从 `challenge widget` 拿，用于自动求解失败后由智能体自己点
    #[arg(long, value_name = "X,Y")]
    pub(crate) at: Option<String>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) frame: Option<i64>,
}

#[derive(Debug, Args)]
pub(crate) struct StealthArgs {
    /// audit（默认，列出站点能读到的自动化信号）| scrub（清除本工具自身留下的痕迹）
    #[arg(default_value = "audit")]
    pub(crate) action: String,
    #[arg(long, default_value_t = 15.0)]
    pub(crate) timeout: f64,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) frame: Option<i64>,
}

#[derive(Debug, Args)]
#[command(after_help = "\
示例:
  atb open https://example.com
  atb open https://a.com https://b.com https://c.com   # 并发打开
  atb open https://a.com https://b.com --group-title demo
  atb open ./report.pdf
  atb open https://example.com --background --dry-run  # 只看计划")]
pub(crate) struct OpenArgs {
    /// URL / 本地文件；给多个时并发打开
    #[arg(required = true, num_args = 1..)]
    pub(crate) targets: Vec<String>,
    /// 后台打开：不切换当前标签页（默认是前台打开）
    #[arg(long, conflicts_with = "focus")]
    pub(crate) background: bool,
    #[arg(long)]
    pub(crate) window: bool,
    /// 打开后聚焦浏览器窗口（默认只切标签页，不抢系统焦点）
    #[arg(long)]
    pub(crate) focus: bool,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) session: Option<String>,
    #[arg(long = "group-title")]
    pub(crate) group_title: Option<String>,
    #[arg(long, default_value_t = 10.0)]
    pub(crate) timeout: f64,
    /// 打开后若落在挑战页：auto（默认，识别到就等站点放行）| wait（同 auto，超时按 --challenge-timeout）| off（不检测）
    #[arg(long, default_value = "auto", value_parser = ["auto", "wait", "off"])]
    pub(crate) challenge: String,
    /// 自动等待挑战放行的总超时（秒）；只对 auto/wait 生效
    #[arg(long = "challenge-timeout", default_value_t = 15.0)]
    pub(crate) challenge_timeout: f64,
    /// 打开后继续等页面条件满足才算成功：
    ///   networkidle      —— readyState=complete 且 1 秒内无新请求/DOM 变更
    ///   selector:<CSS>   —— 等到该选择器命中（如 --wait-for 'selector:.result'）
    ///   text:<文本>      —— 等到页面可见文本包含该内容（如 --wait-for 'text:登录成功'）
    /// 超时跟随 --timeout（readiness_timeout）；超时的目标 ok:false + error_code
    /// wait_for_timeout，不影响其它目标。不带 --wait-for 时 ready:true 只代表
    /// 「标签可操作」，不保证首屏有内容。
    #[arg(long = "wait-for", value_name = "COND")]
    pub(crate) wait_for: Option<String>,
    /// `--wait-for` 的独立预算（秒）；不传时普通页面用 --timeout，
    /// chrome:// / chrome-extension:// / about: 等特殊页面用 ATB_OPEN_SPECIAL_WAIT_SEC（默认 3s）
    #[arg(long = "wait-for-timeout", value_name = "SEC")]
    pub(crate) wait_for_timeout: Option<f64>,
    /// 打开前先对目标浏览器/profile 发一次轻量请求唤醒扩展 service worker。
    /// 旧实现冷启动时第一条 open 经常因 SW 未唤醒而慢几秒甚至超时；ping 失败
    /// 会重试一次再打开，不因 ping 失败而拒绝打开。
    #[arg(long)]
    pub(crate) warm: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CleanupArgs {
    /// 清理所有智能体打开的标签页（默认只清自己打开的）
    #[arg(long)]
    pub(crate) all_agents: bool,
    /// 只列出将要关闭的标签页，不执行
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CloseArgs {
    #[arg(long)]
    pub(crate) tab: String,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
    /// 显式确认批量关闭（目前只对 --tab group:<标题> 生效）。
    /// 旧实现直接拒绝 group:，智能体只能先 tabs --ids 再逐个 close；
    /// 又因为「按组关闭」误伤代价高，必须显式 --yes 才执行，防手滑。
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    #[arg(long, default_value_t = 100)]
    pub(crate) tail: usize,
}

/// `atb schema` 的参数：输出本身就是 JSON，`--json` 只是显式声明机器可读（保留兼容）。
#[derive(Debug, Args)]
pub(crate) struct SchemaArgs {
    /// 输出 JSON（缺省行为）；加上它语义更明确，Agent 可以固定带这个 flag
    #[arg(long)]
    pub(crate) json: bool,
}

/// `atb doctor --fix` 的参数：诊断结果之外，对可自动修复的问题动手。
#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// 对可自动修复的问题执行修复（旧 daemon 占端口 / API token 失配等）
    #[arg(long)]
    pub(crate) fix: bool,
    /// 配合 --fix：真杀占用 18767 的进程；不加时只打印要执行的命令，不动进程
    #[arg(long)]
    pub(crate) force: bool,
}

/// `atb scrape` 的参数：批量抓取全部在 CLI 侧编排，复用已有 daemon 端点（/open /exec /close）。
#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb scrape --urls urls.txt --out out.jsonl
  atb scrape --urls 'https://a.com,https://b.com' --out out.jsonl
  atb scrape --urls urls.txt --js-file grab.js --out out.jsonl --batch 6 --concurrency 6
  atb scrape --urls urls.txt --out out.jsonl --retry 2 --timeout 45"#)]
pub(crate) struct ScrapeArgs {
    /// URL 来源：本地文件路径（每行一个，行内支持逗号分隔）或逗号分隔的 URL 列表；自动去重
    #[arg(long = "urls")]
    pub(crate) urls: String,
    /// 每个 URL 上执行的 JS 文件（脚本需 return 一个可 JSON 序列化的值）；
    /// 缺省用内置脚本，返回 {title, url, text(正文前 9000 字)}
    #[arg(long = "js-file")]
    pub(crate) js_file: Option<PathBuf>,
    /// 结果输出文件（JSONL：一行一个 URL，含 {url, tab_id, ok, data, error, elapsed_ms}）
    #[arg(long)]
    pub(crate) out: PathBuf,
    /// 同时执行 JS 的标签页数上限（1-32）
    #[arg(long, default_value_t = 4)]
    pub(crate) concurrency: usize,
    /// 失败 URL 的最大重试轮数（退避 3s/5s/7s）
    #[arg(long, default_value_t = 3)]
    pub(crate) retry: usize,
    /// 每批一次性打开的 URL 数（后台打开，批处理完即关闭）
    #[arg(long, default_value_t = 4)]
    pub(crate) batch: usize,
    /// 单个 URL 打开/执行的超时（秒）
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
    /// 打开的标签页归入该 Chrome 标签组（不传则不分组）
    #[arg(long = "group-title")]
    pub(crate) group_title: Option<String>,
    /// 每次尝试都写一条记录（默认每个 URL 只写最终一条）。
    /// 诊断「第几次才成功」时用；默认模式对 Agent 更省 token，也不会有重复行。
    #[arg(long = "record-attempts")]
    pub(crate) record_attempts: bool,
}

#[derive(Debug, Args)]
pub(crate) struct InstallSkillArgs {
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SetExtensionPortArgs {
    pub(crate) port: u16,
}

#[derive(Debug, Args)]
#[command(
    after_help = "扩展 ID 获取：chrome://extensions → 右上角打开「开发者模式」→ 本扩展卡片下方的「ID」（32 位 a-p 小写字母）。\n\
                  配置写入 <用户配置目录>/ws-trust.json，daemon 每次 WebSocket 握手都会重新读取，无需重启；\
                  已经连上的扩展要重连（在 chrome://extensions 重载本扩展）才会按新规则重新校验。\n\
                  扩展 ID 列表与 token 同时配置时两个条件都要满足。"
)]
pub(crate) struct TrustExtensionArgs {
    /// 扩展 ID（32 位 a-p 小写字母）；--remove 时必填
    pub(crate) extension_id: Option<String>,
    /// 把该扩展 ID 从信任列表移除
    #[arg(long)]
    pub(crate) remove: bool,
    /// 只查看当前信任配置，不修改
    #[arg(long)]
    pub(crate) list: bool,
    /// 清空扩展白名单与 token，回到「任何扩展都能连」的兼容状态
    #[arg(long)]
    pub(crate) clear: bool,
    /// 设置 WebSocket 握手 token（扩展侧需配置同一值，握手时以 ?token= 传入）
    #[arg(long, value_name = "VALUE")]
    pub(crate) token: Option<String>,
    /// 清除握手 token（只清 token，保留扩展白名单）
    #[arg(long = "clear-token")]
    pub(crate) clear_token: bool,
}

#[derive(Debug, Args)]
pub(crate) struct LookupCommand {
    #[command(subcommand)]
    pub(crate) target: LookupTarget,
}

#[derive(Debug, Subcommand)]
pub(crate) enum LookupTarget {
    Tab(LookupTabArgs),
    Browser(LookupBrowserArgs),
    Profile(LookupProfileArgs),
}

#[derive(Debug, Args)]
pub(crate) struct LookupTabArgs {
    pub(crate) tab: String,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct LookupBrowserArgs {
    pub(crate) browser: String,
}

#[derive(Debug, Args)]
pub(crate) struct LookupProfileArgs {
    pub(crate) profile: String,
}

#[derive(Debug, Args)]
pub(crate) struct ProfileLabelCommand {
    #[command(subcommand)]
    pub(crate) action: ProfileLabelAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProfileLabelAction {
    Set(ProfileLabelSetArgs),
    Clear(ProfileLabelClearArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ProfileLabelSetArgs {
    pub(crate) label: String,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct ProfileLabelClearArgs {
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

// ---------------------------------------------------------------------------
// 2026-11-13 改进轮：目标身份 / 环境快照 / 扩展列表 / 元素量测 / 用户脚本
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb focus --tab @latest
  atb focus --tab url:example.com --no-window
  atb focus --tab current"#)]
pub(crate) struct FocusArgs {
    /// 目标标签页（默认当前 agent 的默认目标）；支持 tabs / 序号 / url: 等全部语法
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 只激活标签页，不把浏览器窗口抢到系统前台
    #[arg(long = "no-window")]
    pub(crate) no_window: bool,
    #[arg(long, default_value_t = 15.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb use @latest
  atb use 1234567890
  atb use url:example.com"#)]
pub(crate) struct UseArgs {
    /// 要固定为默认目标的标签页（tabs / @N / url: / title: 等）
    pub(crate) target: String,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb env
  atb env --pretty"#)]
pub(crate) struct EnvArgs {
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 用哪个标签页查持久注入（默认当前智能体的默认目标）
    #[arg(long)]
    pub(crate) tab: Option<String>,
    /// 只列指定数量标签页摘要（默认 10）
    #[arg(long, default_value_t = 10)]
    pub(crate) tab_limit: usize,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb extensions
  atb extensions --include-disabled"#)]
pub(crate) struct ExtensionsArgs {
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 包含已停用的扩展
    #[arg(long = "include-disabled")]
    pub(crate) include_disabled: bool,
    #[arg(long, default_value_t = 15.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb measure --selector '#chart'
  atb measure --selector 'my-app >>> .card' --styles display,position,width,height
  atb measure --selector '#app' --with-text"#)]
pub(crate) struct MeasureArgs {
    /// CSS 选择器或 `host >>> inner` shadow 选择器（不支持 @e 引用）
    #[arg(long)]
    pub(crate) selector: String,
    /// 逗号分隔的 computed style 名称（默认 display/position/width/height/zIndex）
    #[arg(long, value_name = "LIST")]
    pub(crate) styles: Option<String>,
    /// 同时返回元素可见文本（截断到 200 字符）
    #[arg(long = "with-text")]
    pub(crate) with_text: bool,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) frame: Option<i64>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb userscript ./script.user.js
  atb userscript https://example.com/app.user.js --tab @latest"#)]
pub(crate) struct UserscriptArgs {
    /// 本地 .user.js 文件或 http(s) URL
    pub(crate) source: String,
    /// 在哪个标签页里打开安装页（默认当前 agent 的默认目标）
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 只校验元数据并输出解析结果，不打开安装页
    #[arg(long = "parse-only")]
    pub(crate) parse_only: bool,
}
