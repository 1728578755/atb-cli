use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Subcommand)]
pub(crate) enum NetworkAction {
    Start(TabOnlyArgs),
    List(NetworkListArgs),
    Detail(NetworkDetailArgs),
    Clear(TabOnlyArgs),
    Stop(TabOnlyArgs),
}

#[derive(Debug, Args)]
pub(crate) struct NetworkCommand {
    #[command(subcommand)]
    pub(crate) action: NetworkAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConsoleAction {
    Start(TabOnlyArgs),
    List(ConsoleListArgs),
    Clear(TabOnlyArgs),
    Stop(TabOnlyArgs),
    /// 一次性取回页面已产生的 console / error / unhandledrejection（含加载期报错）
    History(ConsoleHistoryArgs),
    /// 轮询式跟随输出（默认 10 秒，可 --duration 加长）
    Tail(ConsoleTailArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ConsoleCommand {
    #[command(subcommand)]
    pub(crate) action: ConsoleAction,
}

#[derive(Debug, Args)]
pub(crate) struct JobCommand {
    #[command(subcommand)]
    pub(crate) action: JobAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum JobAction {
    /// 列出最近提交的后台任务
    List(JobListArgs),
    /// 查询单个后台任务的状态与结果
    Get(JobGetArgs),
    /// 阻塞等待任务结束（带超时，超时后仍可再次查询）
    Wait(JobWaitArgs),
    /// 请求取消任务（协作式：等待类任务立即停止）
    Cancel(JobCancelArgs),
    /// 流式跟踪任务进度（JSONL，一行一个状态快照，直到结束或超时）
    Tail(JobTailArgs),
    /// 取回 exec 超时后暂存的迟到结果（exec_id 见超时错误信息）
    Result(JobResultArgs),
}

#[derive(Debug, Args)]
pub(crate) struct JobTailArgs {
    pub(crate) id: String,
    /// 轮询间隔（秒）
    #[arg(long, default_value_t = 0.5)]
    pub(crate) interval: f64,
    /// 最长跟踪时间（秒）；0 表示直到任务结束
    #[arg(long, default_value_t = 600.0)]
    pub(crate) timeout: f64,
    /// 每个快照输出为紧凑 JSON（默认 true；--human 时输出人读行）
    #[arg(long)]
    pub(crate) human: bool,
}

#[derive(Debug, Args)]
pub(crate) struct JobResultArgs {
    /// exec 超时错误里给出的 exec_id
    pub(crate) id: String,
    /// 等待结果出现的最长秒数（默认 5；长任务可加大）
    #[arg(long, default_value_t = 5.0)]
    pub(crate) wait: f64,
    #[arg(long, default_value_t = 0.5)]
    pub(crate) interval: f64,
}

#[derive(Debug, Args)]
pub(crate) struct ConsoleHistoryArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 采样窗口（毫秒）；默认 ATB_CONSOLE_HISTORY_SETTLE_MS（300）
    #[arg(long = "settle-ms")]
    pub(crate) settle_ms: Option<u64>,
    /// 只要这些类型（逗号分隔：error,warn,log,info,debug,rejection）
    #[arg(long, value_name = "TYPES")]
    pub(crate) types: Option<String>,
    /// 采样前先清空当前缓冲
    #[arg(long)]
    pub(crate) clear: bool,
    #[arg(long, default_value_t = 200)]
    pub(crate) limit: usize,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct ConsoleTailArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 跟随总时长（秒）
    #[arg(long, default_value_t = 10.0)]
    pub(crate) duration: f64,
    /// 轮询间隔（秒）
    #[arg(long, default_value_t = 0.5)]
    pub(crate) interval: f64,
    #[arg(long, value_name = "TYPES")]
    pub(crate) types: Option<String>,
    #[arg(long, default_value_t = 200)]
    pub(crate) limit: usize,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct JobListArgs {
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: usize,
}

#[derive(Debug, Args)]
pub(crate) struct JobGetArgs {
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct JobWaitArgs {
    pub(crate) id: String,
    #[arg(long, default_value_t = 300.0)]
    pub(crate) timeout: f64,
    #[arg(long, default_value_t = 0.3)]
    pub(crate) interval: f64,
}

#[derive(Debug, Args)]
pub(crate) struct JobCancelArgs {
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// 步骤文件：{"steps":[{"op":"exec","args":{...}}, ...]}，也接受顶层数组
    pub(crate) file: PathBuf,
    /// 某一步失败后继续执行后续步骤（默认遇错即停）
    #[arg(long)]
    pub(crate) continue_on_error: bool,
}

#[derive(Debug, Args)]
pub(crate) struct TabOnlyArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct NetworkListArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) filter: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub(crate) limit: usize,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct NetworkDetailArgs {
    pub(crate) request_id: String,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct ConsoleListArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) level: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub(crate) limit: usize,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct BatchArgs {
    /// 步骤文件：{"steps":[{"op":"read","args":{...}}, ...]}，也接受顶层数组
    pub(crate) file: PathBuf,
    /// 并发度：同时在多个标签页上执行的上限（1-32）
    #[arg(long, default_value_t = 4)]
    pub(crate) concurrency: usize,
}

#[derive(Debug, Args)]
#[command(after_help = "\
示例:
  atb read                                  # 正文+表单+计数
  atb read --links --max-chars 4000
  atb read --tab all                         # 多标签并发读取
  atb read --tab url:docs --selector article")]
pub(crate) struct ReadArgs {
    /// 只读取该 CSS 选择器范围内的内容
    #[arg(long)]
    pub(crate) selector: Option<String>,
    /// 正文字符上限（默认 20000）
    #[arg(long = "max-chars", default_value_t = 20000)]
    pub(crate) max_chars: usize,
    /// 页面抽取的节点数上限（**不是字符数**）：节点数不超过该值时用 innerText 精确抽取，
    /// 超过则退化为近似可见文本（textMode=visible-approx，更快但可能少内容），页面侧默认 30000。
    /// 传 0 等同未指定（沿用默认 30000）；正数由 daemon clamp 到 1000~500000。
    /// 控制返回字符量请用 --max-chars（daemon clamp 1000~400000）。
    #[arg(long = "inner-text-limit")]
    pub(crate) inner_text_limit: Option<usize>,
    /// 附带链接列表
    #[arg(long)]
    pub(crate) links: bool,
    /// 不返回表单结构
    #[arg(long = "no-forms")]
    pub(crate) no_forms: bool,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
    /// 连隐藏内容一起返回（textContent 语义）
    #[arg(long = "include-hidden")]
    pub(crate) include_hidden: bool,
    /// 页面内文本抽取时间预算（毫秒，默认 2000）
    #[arg(long = "budget-ms")]
    pub(crate) budget_ms: Option<u64>,
    /// 最多处理多少个节点（默认 60000）
    #[arg(long = "max-nodes")]
    pub(crate) max_nodes: Option<usize>,
    /// 在指定 iframe 内读取（先用 frames 查 frame_id）
    #[arg(long)]
    pub(crate) frame: Option<i64>,
}

#[derive(Debug, Args)]
#[command(after_help = r#"
示例:
  atb find 'a[href]' --limit 20
  atb find --text '登录'
  atb find --xpath '//button[@type="submit"]'"#)]
pub(crate) struct FindArgs {
    /// CSS 选择器
    pub(crate) selector: Option<String>,
    /// 可见文本（包含匹配）
    #[arg(long)]
    pub(crate) text: Option<String>,
    /// XPath
    #[arg(long)]
    pub(crate) xpath: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: usize,
    /// 文本精确匹配（默认包含匹配）
    #[arg(long)]
    pub(crate) exact: bool,
    /// 包含不可见元素
    #[arg(long = "include-hidden")]
    pub(crate) include_hidden: bool,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 20.0)]
    pub(crate) timeout: f64,
    /// 在指定 iframe 内查找
    #[arg(long)]
    pub(crate) frame: Option<i64>,
}

#[derive(Debug, Args)]
pub(crate) struct ActionArgs {
    /// CSS 选择器；按可见文本定位请先 `find --text` 拿选择器（这里不接受 text= 语法）
    pub(crate) target: Option<String>,
    /// 命中多个元素时按索引取（从 0 开始）
    #[arg(long)]
    pub(crate) index: Option<i64>,
    /// select 的选项值或文本
    #[arg(long)]
    pub(crate) value: Option<String>,
    #[arg(long)]
    pub(crate) label: Option<String>,
    /// scroll 的纵向位移
    #[arg(long)]
    pub(crate) dy: Option<f64>,
    /// scroll 到 top / bottom
    #[arg(long)]
    pub(crate) to: Option<String>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 15.0)]
    pub(crate) timeout: f64,
    /// 在指定 iframe 内执行
    #[arg(long)]
    pub(crate) frame: Option<i64>,
}

#[derive(Debug, Args)]
pub(crate) struct InjectArgs {
    /// 脚本内容；也可用 --file 读文件（--list / --remove 时省略）
    pub(crate) script: Option<String>,
    /// 脚本文件；`-` 读 stdin
    #[arg(long)]
    pub(crate) file: Option<PathBuf>,
    /// 按油猴脚本环境执行：注入最小 GM shim（GM_getValue/GM_setValue/
    /// GM_registerMenuCommand/unsafeWindow 等），并从 `==UserScript==` 元数据取名字
    #[arg(long)]
    pub(crate) userscript: bool,
    /// GM_* 存储命名空间（默认取脚本 @name 或文件名）
    #[arg(long = "gm-namespace", value_name = "NAME")]
    pub(crate) gm_namespace: Option<String>,
    /// 注入前先删除匹配的元素（可多次），避免旧宿主节点残留
    #[arg(long = "cleanup-selector", value_name = "CSS")]
    pub(crate) cleanup_selectors: Vec<String>,
    /// 注入前执行的清理脚本文件（例如删除旧全局变量、旧宿主节点）
    #[arg(long = "cleanup-file", value_name = "FILE")]
    pub(crate) cleanup_file: Option<PathBuf>,
    /// 命名持久注入：每次页面导航后自动重放。默认 2 小时后过期；
    /// 永久驻留必须同时给 --persist-forever --i-know-this-is-long-lived
    #[arg(long, conflicts_with_all = ["remove", "list"])]
    pub(crate) persist: Option<String>,
    /// 持久注入寿命（秒）；不传时用 ATB_INJECT_PERSIST_DEFAULT_TTL_SEC（默认 7200）
    #[arg(long = "ttl", value_name = "SEC")]
    pub(crate) ttl: Option<u64>,
    /// 持久注入永不过期；必须同时给 --i-know-this-is-long-lived
    #[arg(long = "persist-forever", requires = "i_know_this_is_long_lived")]
    pub(crate) persist_forever: bool,
    /// 确认知道持久注入会与页面/其它用户脚本长期共存
    #[arg(long = "i-know-this-is-long-lived")]
    pub(crate) i_know_this_is_long_lived: bool,
    /// 移除已持久化的注入
    #[arg(long, conflicts_with = "list")]
    pub(crate) remove: Option<String>,
    /// 列出已持久化的注入
    #[arg(long)]
    pub(crate) list: bool,
    /// 执行环境 main / isolated（大小写不敏感）
    ///
    /// 与 `exec --world` 同口径：自由字符串会被扩展静默落成 MAIN，用户以为切到了
    // isolated。
    #[arg(
        long,
        default_value = "main",
        value_parser = ["main", "isolated"],
        ignore_case = true
    )]
    pub(crate) world: String,
    /// 只在匹配的 URL 上重放（可多次），默认 <all_urls>
    #[arg(long = "match")]
    pub(crate) matches: Vec<String>,
    /// 注入到指定 iframe
    #[arg(long)]
    pub(crate) frame: Option<i64>,
    /// 注入到所有 frame
    #[arg(long = "all-frames")]
    pub(crate) all_frames: bool,
    /// 跳过本地 JS 语法预检，直接交给浏览器执行
    #[arg(long = "no-preflight")]
    pub(crate) no_preflight: bool,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct FramesArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 20.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct BroadcastArgs {
    /// 要广播的操作：exec / read / find / scan / net / downloads / click ...
    pub(crate) op: String,
    /// exec 的脚本（也可用 --file）
    #[arg(long)]
    pub(crate) script: Option<String>,
    #[arg(long)]
    pub(crate) file: Option<PathBuf>,
    /// 其余参数的 JSON 对象，例如 '{"max_chars":2000}'
    #[arg(long = "args")]
    pub(crate) args_json: Option<String>,
    /// 只在这些 browser 上执行（可重复，支持 id 前缀）
    #[arg(long = "browser")]
    pub(crate) browsers: Vec<String>,
    /// 只在这些 profile 上执行（可重复，id 或 label）
    #[arg(long = "profile")]
    pub(crate) profiles: Vec<String>,
    #[arg(long)]
    pub(crate) concurrency: Option<usize>,
    #[arg(long, default_value_t = 180.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct DownloadsArgs {
    #[arg(long)]
    pub(crate) limit: Option<usize>,
    /// in_progress / complete / interrupted
    #[arg(long)]
    pub(crate) state: Option<String>,
    #[arg(long)]
    pub(crate) id: Option<i64>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 20.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct DownloadArgs {
    pub(crate) url: String,
    #[arg(long)]
    pub(crate) filename: Option<String>,
    /// 下载到 Chrome 默认下载目录下的子目录
    #[arg(long)]
    pub(crate) dir: Option<String>,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct DownloadWaitArgs {
    /// 等待指定下载；省略则等待最近一条
    #[arg(long)]
    pub(crate) id: Option<i64>,
    /// 等待超时（秒）
    #[arg(long = "timeout", default_value_t = 120.0)]
    pub(crate) wait_timeout: f64,
    #[arg(long, default_value_t = 0.5)]
    pub(crate) interval: f64,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct NetCommand {
    #[command(subcommand)]
    pub(crate) action: NetAction,
}

#[derive(Debug, Subcommand)]
pub(crate) enum NetAction {
    /// 在页面上下文发请求（带 cookie / 同源策略 / 页面 CSP 视角）
    Fetch(NetFetchArgs),
    /// 包装 fetch / XHR，开始记录（可同时 mock）
    Hook(NetHookArgs),
    /// 读取拦截记录
    Log(NetLogArgs),
    /// 添加 mock 规则（命中后返回伪造响应，也可 abort）
    Mock(NetMockArgs),
    /// 列出 mock 规则
    Routes(NetTargetArgs),
    /// 删除 mock 规则
    Unroute(NetUnrouteArgs),
    /// 还原 fetch / XHR 并清空状态
    Reset(NetTargetArgs),
}

#[derive(Debug, Args)]
pub(crate) struct NetTargetArgs {
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// 在指定 iframe 内执行
    #[arg(long)]
    pub(crate) frame: Option<i64>,
    #[arg(long, default_value_t = 30.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct NetFetchArgs {
    pub(crate) url: String,
    #[arg(long, default_value = "GET")]
    pub(crate) method: String,
    /// 请求头，格式 'Name: value'（可重复）
    #[arg(long = "header", short = 'H')]
    pub(crate) headers: Vec<String>,
    /// 请求体：以 { 或 [ 开头按 JSON，否则按纯文本
    #[arg(long)]
    pub(crate) body: Option<String>,
    /// 返回形式：text（默认）或 base64
    #[arg(long = "response-type", default_value = "text")]
    pub(crate) response_type: String,
    /// 跨站请求的凭据策略：include（默认，带 cookie）/ omit（不带）/ same-origin
    #[arg(long, default_value = "include")]
    pub(crate) credentials: String,
    /// 不自动解析 JSON 响应，按原文返回（响应不是合法 JSON 时会退回原文，可省一次解析）
    #[arg(long = "no-parse-json")]
    pub(crate) no_parse_json: bool,
    /// 响应体上限（字符，默认 200000）
    #[arg(long = "max-chars", default_value_t = 200000)]
    pub(crate) max_chars: usize,
    #[arg(long)]
    pub(crate) tab: Option<String>,
    #[arg(long)]
    pub(crate) browser: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) frame: Option<i64>,
    #[arg(long, default_value_t = 60.0)]
    pub(crate) timeout: f64,
}

#[derive(Debug, Args)]
pub(crate) struct NetHookArgs {
    /// 记录响应体预览（会增加开销）。
    ///
    /// `Option<bool>` 而不是 `bool`：不带这个 flag 时**不下发**该字段，服务端才会沿用
    /// 已持久化的值。旧写法默认 false 且每次下发，第二次 `net hook --persist` 会把
    /// 之前存的 captureBodies 静默压回 false（CLI 侧另一半）。
    #[arg(long = "capture-bodies", num_args = 0..=1, default_missing_value = "true")]
    pub(crate) capture_bodies: Option<bool>,
    /// 持久化：页面导航后自动重装 hook（可配合 --match 限定 URL）
    #[arg(long)]
    pub(crate) persist: bool,
    /// 持久化的 URL 匹配模式（可重复），默认 <all_urls>
    #[arg(long = "match")]
    pub(crate) matches: Vec<String>,
    /// 日志环形缓冲条数（10~5000）。不带时不覆盖已持久化的值。
    #[arg(long = "max-log")]
    pub(crate) max_log: Option<usize>,
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
pub(crate) struct NetLogArgs {
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: usize,
    /// 按 URL 片段过滤
    #[arg(long = "match")]
    pub(crate) matches: Option<String>,
    /// 读取后清空
    #[arg(long)]
    pub(crate) clear: bool,
    /// 记录被规则放行（pass）的请求；默认只记命中规则的，排查漏 mock 时用
    #[arg(long)]
    pub(crate) debug_pass: bool,
    /// 日志来源：page 只看页面侧（fetch/XHR）、cdp 只看扩展侧、both 合并两者（默认）
    #[arg(long, value_parser = ["page", "cdp", "both"])]
    pub(crate) engine: Option<String>,
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
pub(crate) struct NetMockArgs {
    /// URL 片段或正则
    pub(crate) pattern: String,
    /// 伪造响应的状态码（不填时：只改请求/响应就不伪造，纯 mock 默认 200）
    #[arg(long)]
    pub(crate) status: Option<u16>,
    /// 响应体：以 { 或 [ 开头按 JSON，否则按纯文本
    #[arg(long)]
    pub(crate) body: Option<String>,
    #[arg(long)]
    pub(crate) method: Option<String>,
    /// 响应头，格式 'Name: value'（可重复）
    #[arg(long = "header", short = 'H')]
    pub(crate) headers: Vec<String>,
    /// 直接让请求失败（模拟网络错误）
    #[arg(long)]
    pub(crate) abort: bool,
    /// 追加/覆盖请求头，格式 'Name: value'（可重复）
    #[arg(long = "request-header")]
    pub(crate) request_headers: Vec<String>,
    /// 替换请求体（以 { 或 [ 开头按 JSON）
    #[arg(long = "request-body")]
    pub(crate) request_body: Option<String>,
    /// 放行真实请求但改写响应状态码
    #[arg(long = "response-status")]
    pub(crate) response_status: Option<u16>,
    /// 放行真实请求但改写响应体
    #[arg(long = "response-body")]
    pub(crate) response_body: Option<String>,
    /// 拦截引擎：page（默认，包装页面 fetch/XHR）/ cdp（扩展侧 Fetch 域，
    /// 能改 responseType=blob 的原始二进制体，也能拦 img/script 这类请求）
    #[arg(long, default_value = "page")]
    pub(crate) engine: String,
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
pub(crate) struct NetUnrouteArgs {
    pub(crate) pattern: String,
    /// 拦截引擎：page（默认，删页面内规则）/ cdp（删扩展侧 Fetch 域的规则）
    #[arg(long, default_value = "page")]
    pub(crate) engine: String,
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
