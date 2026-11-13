use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

/// `atb scrape`：批量抓取。
///
/// 全部在 CLI 侧编排，复用已有 daemon 端点（/open /exec /close），不改 daemon：
/// 1. 读 URL 列表（文件或逗号分隔，去重保序）；
/// 2. 分批后台打开（`--batch` 个一批，daemon 内部并发）；
/// 3. 每批对每个标签页执行 JS（`--concurrency` 个线程并发）；
/// 4. 收集结果逐行写 out.jsonl（一行一个 URL）；
/// 5. 批次结束关闭本批标签页；
/// 6. 失败 URL 进入下一轮，退避 3s/5s/7s，最多 `--retry` 轮；
/// 7. 结束打印摘要。
///
/// JS 缺省时用内置脚本取 `{title, url, text(正文前 9000 字)}`，
/// 9000 字的上限是为了单行结果不撑爆 Agent 上下文。
// `String.prototype.slice` 按 UTF-16 code unit 截断，9000 处正好落在代理对
/// 中间时会切出半个 emoji（JSON 里变成替换字符）。用扩展运算符按 code point 切。
const DEFAULT_SCRAPE_SCRIPT: &str =
    "return {title:document.title,url:location.href,text:[...document.body.innerText].slice(0,9000).join('')}";

/// 失败重试的退避序列（秒）：第 1/2/3 轮前分别等 3s/5s/7s，超出后沿用 7s。
const RETRY_BACKOFF_SECS: [u64; 3] = [3, 5, 7];

/// `--batch` / `--concurrency` 的合法上限。
///
/// 0 会让 `chunks(0)` panic；过大的值等于让 daemon 一次性开出几百个标签页（浏览器
// 直接卡死）。两者都是调用方写错了参数，必须在本地以退出码 2 拒绝。
pub(crate) const MAX_PARALLEL: usize = 32;

/// 读 URL 列表：`--urls` 是存在的文件就按行读（行内支持逗号分隔），
/// 否则按逗号分隔的列表解析；统一去重保序，空项丢弃。
///
// 不存在的文件路径以前会被静默当成「逗号分隔的 URL 列表」，于是一个
/// 拼错的 `urls.txt` 只会得到「没有解析到任何 URL」或更糟——把路径字符串当成
/// 一个 URL 去打开。现在只要输入像路径（有扩展名或路径分隔符、且不含 scheme），
/// `!is_file()` 就直接报 `file_not_found`（exit 2）。
fn read_url_list(raw: &str) -> Result<Vec<String>> {
    let trimmed = raw.trim();
    let path = Path::new(trimmed);
    let has_scheme = trimmed.contains("://")
        || trimmed.to_ascii_lowercase().starts_with("data:")
        || trimmed.to_ascii_lowercase().starts_with("blob:")
        || trimmed.to_ascii_lowercase().starts_with("file:");
    let looks_like_path = !has_scheme
        && (trimmed.contains('/') || trimmed.contains('\\') || path.extension().is_some());
    if !path.is_file() && looks_like_path {
        return Err(usage_failure(
            "file_not_found",
            format!("--urls 指向的文件不存在: {trimmed}"),
            "确认路径存在（相对路径按当前工作目录解析）；也可以直接传逗号分隔的 URL 列表，例如 --urls 'https://a.com,https://b.com'",
        ));
    }
    let text = if path.is_file() {
        fs::read_to_string(path)
            .with_context(|| format!("读取 URL 文件失败: {}", path.display()))?
    } else {
        raw.to_string()
    };
    let mut urls: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in text.lines() {
        for item in line.split(',') {
            let url = item.trim().to_string();
            if url.is_empty() {
                continue;
            }
            // 去重按原样字符串比较：同一 URL 两种写法（带不带尾斜杠）算两条，
            // 避免自作聪明归一化后把用户有意分开的目标合并。
            if seen.insert(url.clone()) {
                urls.push(url);
            }
        }
    }
    Ok(urls)
}

/// 结果是否「未知」：请求已发出但没拿到结果（可能已经执行）。
///
/// 三种来源都要认：结构化 `outcome_unknown:true`、`error_code:"outcome_unknown"`、
/// daemon 侧以 `outcome_unknown:` 开头的错误文案（见 `server/action.rs`）。
// 未知结果**绝不能重试**：重放可能造成二次点击/输入。
fn is_outcome_unknown(value: &Value) -> bool {
    if value
        .get("outcome_unknown")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    if value.get("error_code").and_then(Value::as_str) == Some("outcome_unknown") {
        return true;
    }
    value
        .get("error")
        .and_then(Value::as_str)
        .map(|text| text.trim_start().starts_with("outcome_unknown"))
        .unwrap_or(false)
}

/// 本批已打开的标签页：任何退出路径（`?` 提前返回、panic 之外的早退）都要关掉，
// 否则 scrape 会给用户留一堆后台页。
struct OpenTabGuard {
    tabs: Vec<String>,
    timeout: f64,
}

impl OpenTabGuard {
    fn new(timeout: f64) -> Self {
        Self {
            tabs: Vec::new(),
            timeout,
        }
    }

    fn track(&mut self, tab_id: &str) {
        if !tab_id.is_empty() {
            self.tabs.push(tab_id.to_string());
        }
    }

    /// 关闭当前记录的所有标签页，返回失败数（已清空列表，不会重复关）。
    ///
    // 用 `mem::take` 把 id 列表整体取走再逐条关——这样无论主流程显式调用
    /// 还是 panic 时 `Drop` 兜底调用，都不会对同一批 id 二次 `/close`。
    fn close_all(&mut self) -> usize {
        let tabs = std::mem::take(&mut self.tabs);
        let mut failed = 0usize;
        for tab in tabs {
            let ok = request(
                "POST",
                "/close",
                Some(json!({ "tab_id": tab })),
                self.timeout.max(5.0),
            )
            .map(|value| value.get("ok").and_then(Value::as_bool).unwrap_or(false))
            .unwrap_or(false);
            if !ok {
                failed += 1;
            }
        }
        failed
    }
}

impl Drop for OpenTabGuard {
    fn drop(&mut self) {
        if !self.tabs.is_empty() {
            let _ = self.close_all();
        }
    }
}

/// 单个 URL 的最终结果：默认只在全部轮次结束后写一条，`--record-attempts` 才逐次写。
#[derive(Clone)]
struct UrlResult {
    tab_id: Value,
    ok: bool,
    data: Value,
    error: Value,
    elapsed_ms: u64,
    attempts: usize,
    outcome_unknown: bool,
}

impl UrlResult {
    fn new() -> Self {
        Self {
            tab_id: Value::Null,
            ok: false,
            data: Value::Null,
            error: Value::Null,
            elapsed_ms: 0,
            attempts: 0,
            outcome_unknown: false,
        }
    }

    fn to_json(&self, url: &str) -> Value {
        let mut value = json!({
            "url": url,
            "tab_id": self.tab_id.clone(),
            "ok": self.ok,
            "data": self.data.clone(),
            "error": self.error.clone(),
            "elapsed_ms": self.elapsed_ms,
            "attempts": self.attempts,
            // 结果未知在每一行里都是显式字段，而不是只靠顶层汇总。
            "outcome_unknown": self.outcome_unknown,
        });
        if self.outcome_unknown {
            // 与 daemon / http 层同一形状：error_code 让下游 `is_outcome_unknown`
            // 与退出码映射都能命中。
            value["error_code"] = json!("outcome_unknown");
        }
        value
    }
}

/// 对单个标签页执行脚本：返回 (/exec 原始响应, 本条耗时毫秒)。
/// 耗时在线程内实测，并发时各条互不干扰；放在独立函数里是为了
/// 配合 `--concurrency` 的线程并发（request/Client 都是线程安全的）。
fn exec_on_tab(tab_id: &str, script: &str, timeout: f64) -> Result<(Value, u64)> {
    let payload = json!({
        "script": script,
        "switch_tab_id": tab_id,
        "no_monitor": true,
        "timeout": timeout,
    });
    let started = Instant::now();
    // 客户端超时比服务端多留 10s 余量：让 daemon 的结构化超时错误先回来，
    // 而不是拿到 reqwest 的裸 "operation timed out"（与 cmd_exec 的口径一致）。
    let value = request("POST", "/exec", Some(payload), timeout + 10.0)?;
    // serde_json 默认不收 u128，统一转 u64 毫秒。
    Ok((value, started.elapsed().as_millis() as u64))
}

/// 一批标签页的并发执行：按 `--concurrency` 分块，块内线程并发，块间串行。
/// 返回 (url, tab_id, exec 响应, 耗时毫秒) 列表，顺序与输入一致。
fn exec_batch(
    tabs: &[(String, String)],
    script: &str,
    concurrency: usize,
    timeout: f64,
) -> Vec<(String, String, Value, u64)> {
    let mut results = Vec::with_capacity(tabs.len());
    for chunk in tabs.chunks(concurrency.max(1)) {
        // std::thread::scope：借用安全，不需要 'static，也就不用 Arc 克隆脚本字符串。
        let chunk_results: Vec<(String, String, Value, u64)> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(url, tab_id)| {
                    scope.spawn(move || {
                        let (value, elapsed_ms) = exec_on_tab(tab_id, script, timeout)
                            .unwrap_or_else(|err| {
                                // `/exec` 的传输层 Err 以前标成 unexpected_error，
                                // 绕过了 outcome_unknown 通道 → 上层按「可重试」再跑一次脚本，
                                // 可能造成二次点击/提交。这里统一成结果未知形状。
                                (
                                    json!({
                                        "ok": false,
                                        "outcome_unknown": true,
                                        "outcome": "unknown",
                                        "error_code": "outcome_unknown",
                                        "error": format!(
                                            "exec 请求失败，但脚本可能已执行（结果未知）: {err}"
                                        ),
                                        "hint": "不要自动重试动作类脚本；先用 tabs 复查页面状态",
                                    }),
                                    0,
                                )
                            });
                        (url.clone(), tab_id.clone(), value, elapsed_ms)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        (
                            String::new(),
                            String::new(),
                            json!({
                                "ok": false,
                                "outcome_unknown": true,
                                "outcome": "unknown",
                                "error_code": "outcome_unknown",
                                "error": "执行线程异常退出（脚本可能已执行，结果未知）",
                            }),
                            0,
                        )
                    })
                })
                .collect()
        });
        results.extend(chunk_results);
    }
    results
}

/// 从 `/open` 响应里取 (请求 URL, tab_id) 与非重试原因。
///
// 打开目标在成功/失败两侧共用的形状；单独起别名，避免 clippy 的 type_complexity
// （三层嵌套元组写在签名里确实难读）。
type OpenedTab = (String, String);

// 把 `/open` 回包里的一个条目映射回**请求 URL**。
///
/// 回包里的 `url` 是**实际页面 URL**（重定向后），与 `--urls` 的请求串不同。
/// 以前优先拿它当匹配键，`https://example.com` 被重定向到 `https://example.com/`
/// 后就会被判成「没打开」而反复重试。现在严格按 `target`/`input`（请求串）取键；
/// 其次按 `index` 对齐请求顺序；`url` 只作最后兜底。
fn requested_url_for(item: &Value, actual: &str, requested: &[String]) -> Option<String> {
    for key in ["target", "input"] {
        if let Some(value) = item
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    // 多目标回包带 index，直接对齐输入顺序（最可靠）。
    if let Some(index) = item.get("index").and_then(Value::as_u64) {
        if let Some(url) = requested.get(index as usize) {
            return Some(url.clone());
        }
    }
    // 单目标：请求只有一个 URL，回包条目天然对应它。
    if requested.len() == 1 {
        return Some(requested[0].clone());
    }
    if !actual.is_empty() {
        // 重定向后的地址也可能与某个请求串前缀匹配，按容错匹配兜底。
        if let Some(url) = requested
            .iter()
            .find(|url| url.as_str() == actual || url_matches(actual, url))
        {
            return Some(url.to_string());
        }
        // 实在对不上时保留实际 URL 作展示，不丢结果。
        return Some(actual.to_string());
    }
    None
}

// 解析一批 `/open` 回包。返回（：只列真实存在的 3 个返回值；整批未知由
/// 调用处用 `is_outcome_unknown(response)` 判定，不属于本函数）：
/// - `opened`：打开成功、可以执行脚本的标签页（键是**请求 URL**）；
/// - `failed`：打开失败但**可重试**的请求 URL → 错误文案；
/// - `unknown`：结果未知的请求 URL（请求可能已到达 daemon，重放会重复开页）→ 不重试。
fn opened_tabs(
    response: &Value,
    requested: &[String],
) -> (Vec<OpenedTab>, Vec<OpenedTab>, Vec<String>) {
    let mut opened: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // `tabs[]` 是 daemon 逐目标的结果，带 `target`（请求串）与 `index`；`targets[]`
    // 只有实际 `url`（重定向后），只能兜底。两条都读，但优先能对齐请求串的那份。
    let items = response
        .get("tabs")
        .and_then(Value::as_array)
        .or_else(|| response.get("targets").and_then(Value::as_array))
        .or_else(|| response.get("results").and_then(Value::as_array));
    for tab in items.into_iter().flatten() {
        let actual = tab
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let Some(url) = requested_url_for(tab, &actual, requested) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let ok = tab.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let tab_id = tab
            .get("tab_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if ok && !tab_id.is_empty() {
            opened.push((url, tab_id.to_string()));
        } else if is_outcome_unknown(tab) {
            unknown.push(url);
        } else {
            let error = tab
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("open 失败")
                .to_string();
            failed.push((url, error));
        }
    }
    (opened, failed, unknown)
}

// 打开 scrape 输出文件，按 `ATB_IO_OVERWRITE` 策略处理已存在文件。
///
/// 旧实现 `File::create` 无条件截断：`--urls` 写错一次就把上次的抓取结果清空，
/// 且没有任何确认/备份。现在：
/// - `backup`（默认）：旧文件先改名 `<name>.<ts>.bak`，再新建；
/// - `force`：直接截断覆盖（旧行为，脚本化场景）；
/// - `ask`：存在即报 `output_exists`（exit 2），要用户显式处理；
/// - `append`：追加写。
fn open_scrape_output(path: &Path) -> Result<fs::File> {
    let mode = crate::settings::io_overwrite();
    let exists = path.exists();
    if exists && mode == "ask" {
        return Err(usage_failure(
            "output_exists",
            format!("输出文件已存在: {}", path.display()),
            "换一个 --out 路径，或设 ATB_IO_OVERWRITE=backup / force 后重试",
        ));
    }
    if exists && mode == "backup" {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("out.jsonl");
        let backup = path.with_file_name(format!("{name}.{stamp}.bak"));
        fs::rename(path, &backup).with_context(|| {
            format!(
                "备份旧输出文件失败: {} -> {}",
                path.display(),
                backup.display()
            )
        })?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if mode == "append" {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(path)
        .with_context(|| format!("创建输出文件失败: {}", path.display()))
}

pub(crate) fn cmd_scrape(args: ScrapeArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("scrape", is_async)?;
    // 参数护栏：0 会让 chunks(0) panic、重试死循环；>32 会把浏览器开爆。
    if args.batch == 0 || args.concurrency == 0 {
        return Err(usage_err!("--batch 和 --concurrency 必须至少为 1"));
    }
    if args.batch > MAX_PARALLEL || args.concurrency > MAX_PARALLEL {
        return Err(usage_err!(
            "--batch 和 --concurrency 必须在 1~{MAX_PARALLEL} 之间（收到 batch={} concurrency={}）",
            args.batch,
            args.concurrency
        ));
    }
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 3600.0 {
        return Err(usage_err!("--timeout 必须在 0~3600 秒之间"));
    }
    let urls = read_url_list(&args.urls)?;
    if urls.is_empty() {
        return Err(usage_err!(
            "没有解析到任何 URL：--urls 请给文件路径（每行一个）或逗号分隔的 URL 列表"
        ));
    }
    let script = match &args.js_file {
        Some(file) => fs::read_to_string(file)
            .with_context(|| format!("读取 JS 文件失败: {}", file.display()))?,
        None => DEFAULT_SCRAPE_SCRIPT.to_string(),
    };
    let out_path = absolutize_path(&args.out.display().to_string());
    // `scrape --dry-run` 以前会创建输出文件、拉起 daemon、真开页。
    // 现在只解析 URL 列表与参数：不建文件、不连 daemon、不开页。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "scrape",
                "total": urls.len(),
                "would_open": urls.len(),
                "would_exec": urls.len(),
                "batch": args.batch,
                "concurrency": args.concurrency,
                "retry": args.retry,
                "timeout": args.timeout,
                "out": out_path,
                "note": "去掉 --dry-run 才会创建输出文件、打开标签页并执行脚本",
            },
        }));
        return Ok(());
    }
    // scrape 是页面操作类命令：daemon 不在就自动拉起（与 open/exec 一致）。
    ensure_server()?;
    let mut writer = open_scrape_output(Path::new(&out_path))?;

    let total = urls.len();
    let mut pending: Vec<String> = urls.clone();
    // 每个 URL 的最终结果（默认仅在结束时写一条；--record-attempts 才逐次写）。
    let mut results: std::collections::HashMap<String, UrlResult> =
        std::collections::HashMap::new();
    // retried 统计的是「参加重试的 URL 次数」：一条 URL 重试 2 轮计 2 次。
    let mut retried = 0usize;
    let mut close_failed = 0usize;
    // 已经落盘的 URL（默认模式只写最终记录；成功的当场写，保住 Ctrl+C 前的成果）。
    let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut guard = OpenTabGuard::new(args.timeout);
    // 轮次：第 0 轮是首次执行，1..=retry 是重试轮。
    for round in 0..=args.retry {
        if round > 0 {
            if pending.is_empty() {
                break;
            }
            // 退避：3s/5s/7s，超出序列后沿用最后一档。
            let backoff = RETRY_BACKOFF_SECS[(round - 1).min(RETRY_BACKOFF_SECS.len() - 1)];
            sleep(Duration::from_secs(backoff));
            retried += pending.len();
        }
        let mut next_round: Vec<String> = Vec::new();
        for batch in pending.chunks(args.batch) {
            let targets: Vec<String> = batch.to_vec();
            // 后台打开：不打扰用户当前标签页；keep_alive 让标签页就绪后再返回。
            let mut payload = json!({
                "targets": targets,
                "active": false,
                "keep_alive": true,
                // 严格化后字段名必须与 OpenRequest 对齐：以前写 `timeout`，
                // daemon 静默忽略（页面就绪预算一直用默认值）；现在未知字段会被
                // 422 拒绝，必须用 `readiness_timeout`。
                "readiness_timeout": args.timeout,
            });
            if let Some(group) = &args.group_title {
                payload["group_title"] = json!(group);
            }
            let response = request(
                "POST",
                "/open",
                Some(payload),
                http_budget(args.timeout + 10.0),
            )
            .unwrap_or_else(|err| {
                // 传输层 Err 以前标成 unexpected_error（可重试），会让整批
                // URL 进下一轮——首次可能已经打开了标签页，重试就是重复开页 + 泄漏。
                // 统一成 outcome_unknown，交给 `batch_unknown` 分支逐条记录、不重试。
                json!({
                    "ok": false,
                    "outcome_unknown": true,
                    "outcome": "unknown",
                    "error_code": "outcome_unknown",
                    "error": format!("open 请求失败，但标签页可能已打开（结果未知）: {err}"),
                    "hint": "不要自动重试；先用 atb tabs 复查是否已打开",
                    "tabs": []
                })
            });
            let batch_unknown = is_outcome_unknown(&response);
            let (opened, failed, unknown) = opened_tabs(&response, batch);
            // 整个 /open 结果未知：不知道哪些页已经打开，一律不重试，逐条记一条未知记录。
            if batch_unknown {
                for url in batch {
                    let mut entry = UrlResult::new();
                    entry.attempts = results.get(url).map(|old| old.attempts).unwrap_or(0) + 1;
                    entry.outcome_unknown = true;
                    entry.error = json!(
                        "open 结果未知（请求可能已到达 daemon），未自动重试；请用 tabs 复查是否已打开"
                    );
                    results.insert(url.clone(), entry);
                }
                // 仍把明确打开的标签页记进 guard，避免遗留后台页。
                for (_, tab_id) in &opened {
                    guard.track(tab_id);
                }
                close_failed += guard.close_all();
                continue;
            }
            for (_, tab_id) in &opened {
                guard.track(tab_id);
            }
            // 打开失败的目标：可重试的进下一轮；结果未知的不重试。
            let opened_urls: std::collections::HashSet<&str> =
                opened.iter().map(|(url, _)| url.as_str()).collect();
            let unknown_set: std::collections::HashSet<&str> =
                unknown.iter().map(String::as_str).collect();
            let failed_map: std::collections::HashMap<&str, &str> = failed
                .iter()
                .map(|(url, error)| (url.as_str(), error.as_str()))
                .collect();
            for url in batch {
                if opened_urls.contains(url.as_str()) {
                    continue;
                }
                if unknown_set.contains(url.as_str()) {
                    // 不重试：写一条未知记录即可。
                    let mut entry = results.get(url).cloned().unwrap_or_else(UrlResult::new);
                    entry.attempts += 1;
                    entry.outcome_unknown = true;
                    entry.error = json!(
                        "open 结果未知（请求可能已到达 daemon），未自动重试；请用 tabs 复查是否已打开"
                    );
                    results.insert(url.clone(), entry);
                    continue;
                }
                next_round.push(url.clone());
                // 记住最后一次打开失败的原因：最终仍失败时要有可读的记录。
                let mut entry = results.get(url).cloned().unwrap_or_else(UrlResult::new);
                entry.attempts += 1;
                if !entry.ok {
                    entry.error = json!(failed_map
                        .get(url.as_str())
                        .copied()
                        .unwrap_or("open 未返回该目标（可能被 daemon 拒绝）"));
                }
                results.insert(url.clone(), entry);
            }
            if opened.is_empty() {
                continue;
            }
            for (url, tab_id, value, elapsed_ms) in
                exec_batch(&opened, &script, args.concurrency, args.timeout)
            {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let unknown = is_outcome_unknown(&value);
                // daemon 的 `/exec` 把脚本返回值放在 `result.js_return`；旧版本
                // 用 `result.value`。只读后者会让每个成功行都是 `data: null`
                // （实测 scrape 全绿但抓不到任何内容）。
                let data = value
                    .pointer("/result/js_return")
                    .or_else(|| value.pointer("/result/value"))
                    .or_else(|| value.pointer("/result/result"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let error = if ok {
                    Value::Null
                } else {
                    value
                        .get("error")
                        .cloned()
                        .unwrap_or_else(|| json!("exec 失败"))
                };
                let mut entry = results.get(&url).cloned().unwrap_or_else(UrlResult::new);
                entry.attempts += 1;
                entry.tab_id = json!(tab_id);
                entry.elapsed_ms = elapsed_ms;
                if ok {
                    entry.ok = true;
                    entry.data = data;
                    entry.error = Value::Null;
                } else if !entry.ok {
                    entry.error = error;
                }
                if unknown {
                    entry.outcome_unknown = true;
                    if !ok {
                        entry.error = json!(
                            "exec 结果未知（请求可能已执行），未自动重试；重复执行可能产生二次操作"
                        );
                    }
                }
                // `--record-attempts`：逐次落盘，便于诊断「第几次才成功」。
                if args.record_attempts {
                    writeln!(writer, "{}", entry.to_json(&url))
                        .with_context(|| format!("写出结果失败: {}", out_path))?;
                    writer.flush().ok();
                    written.insert(url.clone());
                }
                let unknown_now = unknown && !ok;
                // 成功的 URL 已经是最终结果（不会再重试）：立刻落盘，进程被中断时
                // 也不丢已抓到的数据。
                if ok && !args.record_attempts {
                    writeln!(writer, "{}", entry.to_json(&url))
                        .with_context(|| format!("写出结果失败: {}", out_path))?;
                    writer.flush().ok();
                    written.insert(url.clone());
                }
                results.insert(url.clone(), entry);
                // 结果未知不重试；其余失败进下一轮。
                if !ok && !unknown_now {
                    next_round.push(url);
                }
            }
            // 批处理完关闭本批标签页：scrape 不给用户留一堆后台页。
            close_failed += guard.close_all();
        }
        pending = next_round;
    }
    // 主流程显式清理（正常路径每批已关，这里兜底收尾）。`close_all` 用
    // `mem::take` 取走 id 列表，Drop 里的 panic 兜底不会二次关闭同一批标签页。
    close_failed += guard.close_all();
    // 默认模式：每个 URL 只写一条最终记录（失败也只一条）。
    let mut success = 0usize;
    let mut failed = 0usize;
    let mut unknown_count = 0usize;
    if !args.record_attempts {
        for url in &urls {
            let entry = results.get(url).cloned().unwrap_or_else(|| {
                let mut missing = UrlResult::new();
                missing.error = json!("未执行（参数或调度异常）");
                missing
            });
            if entry.ok {
                success += 1;
            } else {
                failed += 1;
                if entry.outcome_unknown {
                    unknown_count += 1;
                }
            }
            // 成功记录已在执行时落盘，这里只补最终失败/未知记录。
            if written.contains(url) {
                continue;
            }
            writeln!(writer, "{}", entry.to_json(url))
                .with_context(|| format!("写出结果失败: {}", out_path))?;
            writer.flush().ok();
        }
    } else {
        for url in &urls {
            let entry = results.get(url).cloned();
            match entry.as_ref() {
                Some(item) if item.ok => success += 1,
                Some(item) => {
                    failed += 1;
                    if item.outcome_unknown {
                        unknown_count += 1;
                    }
                }
                None => failed += 1,
            }
            // `--record-attempts` 以前只写执行过 /exec 的 URL，打开阶段就
            // 失败的 URL 一行都没有，调用方甚至不知道它被尝试过。收尾时对没有
            // 任何输出行的 URL 补一条最终记录。
            if written.contains(url) {
                continue;
            }
            let row = entry.unwrap_or_else(|| {
                let mut missing = UrlResult::new();
                missing.error = json!("未执行（参数或调度异常）");
                missing
            });
            writeln!(writer, "{}", row.to_json(url))
                .with_context(|| format!("写出结果失败: {}", out_path))?;
            writer.flush().ok();
            written.insert(url.clone());
        }
    }
    // 标签关闭失败不该让整批抓取报失败——`ok` 只看抓取结果，
    // `close_failed` 单独作为警告字段（warnings + hint），调用方自行决定是否清理。
    let ok = failed == 0;
    let mut warnings: Vec<String> = Vec::new();
    if close_failed > 0 {
        warnings.push(format!("close_failed:{close_failed}"));
    }
    let mut summary = json!({
        "ok": ok,
        "total": total,
        "success": success,
        "failed": failed,
        "retried": retried,
        "close_failed": close_failed,
        "warnings": warnings,
        "outcome_unknown": unknown_count,
        "out": out_path,
        "hint": if failed > 0 {
            if unknown_count > 0 {
                json!("有 URL 的结果未知（请求可能已执行），已停止对它们的重试；先 atb tabs 复查或人工核对输出，再决定是否重跑")
            } else {
                json!("失败的 URL 没有可用数据行（打开失败的没有行，执行失败的行 ok:false）；失败原因见 atb logs，可单独重跑这批 URL")
            }
        } else if close_failed > 0 {
            json!("抓取结果全部成功，但有标签页关闭失败，可能残留后台页；用 atb tabs 查看后用 close 清理")
        } else {
            Value::Null
        },
    });
    if unknown_count > 0 {
        // print_json 按 error_code 映射退出码：outcome_unknown → 4（结果未知）。
        summary["error_code"] = json!("outcome_unknown");
    }
    print_json(summary);
    Ok(())
}
