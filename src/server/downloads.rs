use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

/// 每个调用方（agent）最近若干次 `download` 启动的下载 id。
///
/// 旧实现每个 agent 只存**一个** id：同一 agent 连续发起 A、B 两个下载时，
/// A 的 id 被 B 覆盖，`download-wait`（不带 `--id`）就去等 B 的文件；
/// 两个未显式带 `--agent` 的 CLI 进程又都会落到 `default`，互相覆盖。
/// 现在按 agent 存一个 `VecDeque`（深度可配，默认 8），wait 命中「本 agent 最近
/// 启动且仍在列表里」的那一条，并在回包里用 `claimed_from` 说明命中来源。
fn download_registry_depth() -> usize {
    crate::settings::download_registry_depth()
}

/// 每个 agent 最近下载的 (id, 时间) 队列。抽成别名只为满足 clippy 的类型复杂度告警。
type DownloadRegistry = HashMap<String, VecDeque<(i64, Instant)>>;

fn last_download_registry() -> &'static Mutex<DownloadRegistry> {
    static REGISTRY: OnceLock<Mutex<DownloadRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_last_download(id: i64) {
    let depth = download_registry_depth();
    let mut registry = last_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entries = registry.entry(current_agent()).or_default();
    // 同一个 id 重复登记时不重复入队，只刷新时间戳。
    entries.retain(|(seen, _)| *seen != id);
    entries.push_back((id, Instant::now()));
    while entries.len() > depth {
        entries.pop_front();
    }
}

/// 本 agent 最近启动过的下载 id，**新的在前**；超过认领窗口的条目顺手清掉。
fn recent_download_ids_of_current_agent() -> Vec<i64> {
    let ttl = Duration::from_secs(crate::settings::download_claim_ttl_sec());
    let agent = current_agent();
    let mut registry = last_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(entries) = registry.get_mut(&agent) else {
        return Vec::new();
    };
    entries.retain(|(_, at)| at.elapsed() < ttl);
    // 先收集再决定是否删条目：避免在 `entries` 的借用还活着时动 registry。
    let ids: Vec<i64> = entries.iter().rev().map(|(id, _)| *id).collect();
    if ids.is_empty() {
        registry.remove(&agent);
    }
    ids
}

// ---------------------------------------------------------------- 文件名净化

/// Windows 保留设备名：`CON` / `PRN` / `AUX` / `NUL` / `COM1..9` / `LPT1..9`。
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 文件名保留的最大字符数（不含路径）。
const MAX_DOWNLOAD_NAME_CHARS: usize = 180;

/// 净化下载文件名。
///
/// 修的是「daemon 侧原样透传」：`--filename ../../etc/passwd` 会一路拼到
/// `${dir}/${fileName}`，只剩 Chrome 兜底。这里在到达扩展之前就取 basename、
/// 拒绝空名与上跳、把危险字符与 Windows 保留名换掉，并截断超长名。
///
/// 返回 `(实际下发的文件名, 告警列表)`。`ATB_IO_SANITIZE_FILENAME=compat` 时
/// **只告警不改名**（兼容脚本化场景），但空名仍然直接报错（没有可用的落盘名）。
pub(crate) fn sanitize_download_name(raw: &str) -> Result<(String, Vec<String>)> {
    let compat = crate::settings::io_sanitize_filename() == "compat";
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "download: --filename 不能为空（未指定时由扩展按 URL 末段或 Content-Disposition 决定）"
        ));
    }
    let mut warnings: Vec<String> = Vec::new();

    // 1) 任何一段是 `..` 就是路径上跳：直接拒绝，不做「取 basename 后放行」。
    if trimmed
        .split(['/', '\\'])
        .any(|segment| segment == ".." || segment == ".")
    {
        return Err(anyhow!(
            "download: --filename 不能包含路径上跳段（收到 {raw:?}）；这里只接受纯文件名，目录请用 --dir"
        ));
    }

    // 2) 取 basename：Windows/Linux 分隔符都算。
    let basename = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if basename != trimmed {
        warnings.push(format!(
            "filename 含路径段，只保留 basename（{trimmed:?} → {basename:?}）；目录请用 --dir"
        ));
    }
    let name = basename.trim();

    // 3) 危险字符与保留名。
    let mut cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if (ch as u32) < 0x20 => '_',
            ch => ch,
        })
        .collect();
    // Windows 上结尾的点和空格会被系统悄悄吃掉，导致「回包名 ≠ 落盘名」。
    let trailing = cleaned.trim_end_matches([' ', '.']).len();
    if trailing != cleaned.len() {
        cleaned.truncate(trailing);
        warnings.push("filename 结尾的空白/点会被 Windows 丢弃，已去除".to_string());
    }
    if cleaned.is_empty() {
        return Err(anyhow!(
            "download: --filename 净化后为空（收到 {raw:?}），请换一个普通文件名"
        ));
    }
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    if WINDOWS_RESERVED_NAMES.contains(&stem.as_str()) {
        cleaned = format!("_{cleaned}");
        warnings.push(format!(
            "filename 与 Windows 保留设备名冲突（{stem}），已加前缀 _ 避免写盘失败"
        ));
    }

    // 4) 超长截断，尽量保留扩展名（Chrome 靠它决定落盘类型）。
    if cleaned.chars().count() > MAX_DOWNLOAD_NAME_CHARS {
        let original_len = cleaned.chars().count();
        // 先算出新名字再赋值：直接 `cleaned = format!("{stem}{ext}")` 会同时
        // 借用 stem/ext（借自 cleaned）与写入 cleaned，无法通过借用检查。
        let shortened = {
            let (stem, ext) = match cleaned.rfind('.') {
                Some(pos) if pos > 0 && cleaned.len() - pos <= 17 => {
                    (&cleaned[..pos], &cleaned[pos..])
                }
                _ => (cleaned.as_str(), ""),
            };
            let budget = MAX_DOWNLOAD_NAME_CHARS
                .saturating_sub(ext.chars().count())
                .max(1);
            format!("{}{}", stem.chars().take(budget).collect::<String>(), ext)
        };
        warnings.push(format!(
            "filename 超过 {MAX_DOWNLOAD_NAME_CHARS} 字符，已截断（原长 {original_len}）"
        ));
        cleaned = shortened;
    }

    if cleaned == name && warnings.is_empty() {
        return Ok((cleaned, warnings));
    }
    if compat {
        // compat：只告警，仍按用户原样下发（旧脚本依赖「我传什么就是什么」）。
        if cleaned != name || !warnings.is_empty() {
            warnings.push(format!(
                "ATB_IO_SANITIZE_FILENAME=compat：已按原名 {name:?} 下发（净化后的建议值 {cleaned:?}）"
            ));
        }
        return Ok((name.to_string(), warnings));
    }
    if cleaned != name {
        warnings.insert(0, format!("filename 已净化：{name:?} → {cleaned:?}"));
    }
    Ok((cleaned, warnings))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct DownloadsRequest {
    /// list | start | wait | cancel | erase
    pub(crate) action: Option<String>,
    pub(crate) id: Option<i64>,
    pub(crate) url: Option<String>,
    pub(crate) filename: Option<String>,
    pub(crate) dir: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) limit: Option<usize>,
    /// wait 的超时（秒）与轮询间隔（秒）。
    pub(crate) wait_timeout: Option<f64>,
    pub(crate) interval: Option<f64>,
    /// wait 超时时是否取消这条下载。缺省读
    /// `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT`（默认 false，避免杀掉用户仍想要的文件）。
    #[serde(default, alias = "cancelOnTimeout")]
    pub(crate) cancel_on_timeout: Option<bool>,
    /// 取消后再抹掉下载历史条目（需 `cancel_on_timeout` 生效）；缺省读
    /// `ATB_DOWNLOAD_CANCEL_ERASE`（默认 false）。
    #[serde(default, alias = "cancelErase")]
    pub(crate) cancel_erase: Option<bool>,
    pub(crate) switch_tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

pub(crate) async fn downloads(
    State(state): State<AppState>,
    Json(req): Json<DownloadsRequest>,
) -> Json<Value> {
    touch(&state).await;
    // wait 的超时 / interrupted 是业务失败，外层 ok 必须跟随内层。
    Json(json_result_with_inner_ok(
        downloads_value(&state, req).await,
    ))
}

pub(crate) async fn downloads_value(state: &AppState, req: DownloadsRequest) -> Result<Value> {
    let action = req
        .action
        .clone()
        .unwrap_or_else(|| "list".to_string())
        .to_lowercase();
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;

    if action != "wait" {
        let mut payload = json!({ "action": action });
        if let Some(id) = req.id {
            payload["id"] = json!(id);
        }
        if let Some(url) = req.url {
            payload["url"] = json!(url);
        }
        // 文件名在 daemon 侧就净化：
        // 旧实现原样拼进 `${dir}/${fileName}` 交给扩展，只剩 Chrome 兜底，
        // `--filename ../../x` 会写到下载目录之外。
        let mut filename_note: Option<Value> = None;
        if let Some(filename) = req.filename {
            if action == "start" {
                let (applied, warnings) = sanitize_download_name(&filename)?;
                if applied != filename || !warnings.is_empty() {
                    filename_note = Some(json!({
                        "requested": filename,
                        "applied": applied.clone(),
                        "changed": applied != filename,
                        "warnings": warnings,
                    }));
                }
                payload["filename"] = json!(applied);
            } else {
                payload["filename"] = json!(filename);
            }
        }
        if let Some(dir) = req.dir {
            // 目录允许是绝对路径（下载目录本来就是绝对路径），但不允许上跳：
            // `${dir}/../..` 一样能把文件写到任意位置。
            if dir.split(['/', '\\']).any(|segment| segment == "..") {
                return Err(anyhow!(
                    "download: --dir 不能包含 `..`（收到 {dir:?}）；需要指定下载目录请直接给绝对路径"
                ));
            }
            payload["dir"] = json!(dir);
        }
        if let Some(state_filter) = req.state {
            payload["state"] = json!(state_filter);
        }
        if let Some(limit) = req.limit {
            payload["limit"] = json!(limit.clamp(1, 200));
        }
        let value = extension_control(
            state,
            &session_key,
            "downloads",
            payload,
            DEFAULT_EXTENSION_CONTROL_TIMEOUT,
        )
        .await?;
        let mut data = value.get("data").cloned().unwrap_or(value);
        if let Some(note) = filename_note {
            if let Some(object) = data.as_object_mut() {
                object.insert("filename_sanitized".to_string(), note);
            }
        }
        // 记住本次 start 的 id，供同一 agent 的 `download-wait` 精确认领。
        if action == "start" {
            let id = data
                .get("item")
                .and_then(|item| item.get("id"))
                .or_else(|| data.get("id"))
                .and_then(Value::as_i64);
            if let Some(id) = id {
                remember_last_download(id);
            }
        }
        return Ok(data);
    }

    // wait：在 daemon 侧轮询，避免扩展 Service Worker 在长下载期间被回收。
    let timeout = req.wait_timeout.unwrap_or(120.0).clamp(1.0, 3600.0);
    let interval = req.interval.unwrap_or(0.5).clamp(0.05, 5.0);
    // 列表条数可配：固定 50 会让「较早的 id」落在窗口外，只能一路等到超时。
    let list_limit = crate::settings::download_wait_limit();
    // 超时处理：默认只报超时，不取消——文件可能还在下，杀掉更难恢复。
    let cancel_on_timeout = req
        .cancel_on_timeout
        .unwrap_or_else(crate::settings::download_cancel_on_timeout);
    let cancel_erase = req
        .cancel_erase
        .unwrap_or_else(crate::settings::download_cancel_erase);
    let deadline = Instant::now() + secs(timeout);
    let mut polls: u64 = 0;
    let mut last: Value = Value::Null;

    // 认领顺序：
    // 1) `--id` 精确匹配（唯一可信的归属证明）；
    // 2) 本 agent 最近 N 次 `download` 启动的 id（VecDeque，新的优先）；
    // 3) 循环内再退回「最新一条仍在进行中」→「wait 期间新出现的记录」→「认领窗口内完成的记录」。
    // 每一步都在回包里如实写进 `claimed_from`，调用方能区分「精确命中」与「推断」。
    let explicit_id = req.id;
    let auto_ids = if explicit_id.is_none() {
        recent_download_ids_of_current_agent()
    } else {
        Vec::new()
    };
    if explicit_id.is_none() && auto_ids.is_empty() && !crate::settings::download_auto_claim() {
        return Ok(json!({
            "ok": false,
            "matched": false,
            "state": "id_required",
            "error": "download_id_required: 未指定 --id，且自动认领已关闭（ATB_DOWNLOAD_AUTO_CLAIM=0）",
            "error_code": "download_id_required",
            "hint": "用 `atb download-wait --id <id>`（id 见 `atb downloads`），\
                     或设置 ATB_DOWNLOAD_AUTO_CLAIM=1 恢复自动认领",
            "polls": 0
        }));
    }
    // 只有一个候选 id 时才把它作为过滤条件下发给扩展：传单个 id 会把其它候选挡在外面。
    let query_id = explicit_id.or_else(|| (auto_ids.len() == 1).then(|| auto_ids[0]));
    // 不带 `--id` 时，「等最近一条」需要一个时间基线。
    //
    // 旧实现直接取 `items.first()`：只要列表里还有一条**历史的** complete 记录，
    // 第一次轮询就命中并返回 `ok:true`——等待类命令的假成功比超时更难发现，
    // 调用方会拿着一份旧文件当成本次下载的产物。
    // 现在的规则：进循环时记下列表里的最大 id，之后只认「id 比基线大」的记录；
    // 唯一例外是**最新一条仍在进行中**（`download` 紧接着 `download-wait` 的常见路径）。
    //
    // 上面这条规则修掉了假成功，却把主流路径变成了假超时：
    // `download && download-wait` 里目标条目在 wait 首轮就已存在（id == 基线），
    // 一旦完成就被 `id > 基线` 排除，兜底过滤器又把 complete/interrupted 排除，
    // 于是一路空转到 120s 超时。补两条通道：
    // 1) **认领**：看到「最新一条仍在进行中」就记下它的 id，之后按 id 跟踪到终态，
    //    不再依赖「它是否还比基线新」；
    // 2) **认领窗口**：首轮没有在跑条目时，接受「最新一条已终态且刚开始不久」的记录，
    //    覆盖 `download` 很快结束、wait 才启动的场景。窗口取
    //    `ATB_DOWNLOAD_CLAIM_TTL_SEC`（30–900s，默认 600）。
    let mut baseline_max_id: Option<i64> = None;
    let mut claimed_id: Option<i64> = None;
    let mut claimed_from: Option<&'static str> = None;
    // 超时要取消的目标：最后观察到的条目 id（含认领到的在跑条目）。
    let mut observed_id: Option<i64> = None;
    let claim_window_ms = (crate::settings::download_claim_ttl_sec() as i64).saturating_mul(1000);
    let wait_started_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    loop {
        polls += 1;
        // 每轮控制调用与 sleep 都必须吃「剩余预算」，否则声明 1s 的等待
        // 可能因为一次扩展往返就拖到 30s 以上（deadline 只在调用后检查）。
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(download_wait_timeout_payload(
                state,
                &session_key,
                timeout,
                polls,
                &last,
                observed_id,
                cancel_on_timeout,
                cancel_erase,
            )
            .await);
        }
        let call_timeout = remaining
            .min(DEFAULT_EXTENSION_CONTROL_TIMEOUT)
            .max(Duration::from_millis(50));
        let value = match extension_control(
            state,
            &session_key,
            "downloads",
            json!({ "action": "list", "limit": list_limit, "id": query_id }),
            call_timeout,
        )
        .await
        {
            Ok(value) => value,
            // 单轮超时不算整体失败：预算还没用完就继续等下一轮；扩展真断了由
            // 后续轮次连续失败 + deadline 兜底，避免把一次抖动误报成下载失败。
            Err(err) if remaining > Duration::from_millis(100) => {
                last = json!({ "error": humanize_extension_error(&err.to_string()) });
                tokio::time::sleep(remaining.min(secs(interval))).await;
                continue;
            }
            Err(err) => return Err(err),
        };
        let data = value.get("data").cloned().unwrap_or(value);
        let items = data
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let max_id = items
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_i64))
            .max();
        if baseline_max_id.is_none() {
            baseline_max_id = max_id;
        }
        let latest = items
            .iter()
            .max_by_key(|item| item.get("id").and_then(Value::as_i64).unwrap_or(0));
        let mut matched_recently = false;
        // 1) 精确 id 或本 agent 最近启动的 id（新的优先）。
        let pinned = if let Some(id) = explicit_id {
            items
                .iter()
                .find(|item| item.get("id").and_then(Value::as_i64) == Some(id))
                .cloned()
                .map(|item| (item, "explicit"))
        } else if !auto_ids.is_empty() {
            items
                .iter()
                .filter(|item| {
                    item.get("id")
                        .and_then(Value::as_i64)
                        .map(|id| auto_ids.contains(&id))
                        .unwrap_or(false)
                })
                .max_by_key(|item| item.get("id").and_then(Value::as_i64).unwrap_or(0))
                .cloned()
                .map(|item| (item, "recent"))
        } else {
            None
        };
        let item = if let Some((item, from)) = pinned {
            claimed_from = Some(from);
            Some(item)
        } else if let Some(claimed) = claimed_id {
            // 认领后只按 id 跟踪：列表被 limit 截断或扩展重启时这一轮可能读不到，
            // 继续等下一轮即可（不要退回去重新认领，否则目标会被换掉）。
            let found = items
                .iter()
                .find(|item| item.get("id").and_then(Value::as_i64) == Some(claimed))
                .cloned();
            if found.is_some() {
                claimed_from = Some("inflight");
            }
            found
        } else {
            let newer = items
                .iter()
                .filter(
                    |item| match (item.get("id").and_then(Value::as_i64), baseline_max_id) {
                        (Some(id), Some(base)) => id > base,
                        _ => false,
                    },
                )
                .max_by_key(|item| item.get("id").and_then(Value::as_i64).unwrap_or(0));
            if let Some(item) = newer {
                // 原有通道：wait 期间新出现的记录（id 比基线大）。
                claimed_from = Some("recent");
                Some(item.clone())
            } else if let Some(item) = latest
                .filter(|item| download_is_running(download_state_of(item)))
                .cloned()
            {
                // 没有更新的记录：最新一条还在跑就认它。
                claimed_id = item.get("id").and_then(Value::as_i64);
                claimed_from = Some("inflight");
                Some(item)
            } else {
                // 认领窗口：wait 启动前不久就开始的终态条目算「刚完成」，
                // 起步时间读不到（扩展没带 startTimeMs / startTime）时跳过该判据，
                // 宁可继续等到超时，也不拿历史记录冒充本次产物。
                let recent = latest.cloned().filter(|item| {
                    download_is_terminal(download_state_of(item))
                        && download_start_ms(item)
                            .map(|start| start >= wait_started_ms.saturating_sub(claim_window_ms))
                            .unwrap_or(false)
                });
                matched_recently = recent.is_some();
                if matched_recently {
                    claimed_from = Some("recent");
                }
                recent
            }
        };
        if let Some(item) = item {
            let download_state = download_state_of(&item).to_string();
            if let Some(id) = item.get("id").and_then(Value::as_i64) {
                observed_id = Some(id);
            }
            if download_is_terminal(&download_state) {
                let mut out = json!({
                    "ok": download_state == "complete",
                    "matched": download_state == "complete",
                    "state": download_state,
                    "item": item,
                    "polls": polls,
                    // 命中来源：explicit（--id）/ recent（本 agent 最近启动或刚完成）/ inflight（认领到在跑条目）
                    "claimed_from": claimed_from.unwrap_or("recent"),
                    "waited_ms": (polls.saturating_sub(1) as f64 * interval * 1000.0).round() as u64
                });
                // 只有走「认领窗口」才有这个字段：调用方据此知道命中的是
                // wait 之前就已完成的那条，而不是实时观察到的完成。
                if matched_recently {
                    out["matched_recently"] = json!(true);
                }
                return Ok(out);
            }
            last = item;
        }
        if Instant::now() >= deadline {
            return Ok(download_wait_timeout_payload(
                state,
                &session_key,
                timeout,
                polls,
                &last,
                observed_id,
                cancel_on_timeout,
                cancel_erase,
            )
            .await);
        }
        // sleep 同样不能越过 deadline：`interval` 默认 0.5s，但用户可以给 5s。
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(secs(interval).min(remaining)).await;
    }
}

/// 组装 `download-wait` 的超时回包，必要时先取消这条下载。
///
/// 旧实现超时只报一句 `download_wait_timeout`：临时文件留在磁盘里、
/// 下载还在后台跑，调用方既拿不到产物也不知道该不该清理。`cancel_on_timeout`
/// 打开后在返回前尽力取消（可选 erase 掉历史条目），并把结果写进 `cancelled`。
#[allow(clippy::too_many_arguments)]
async fn download_wait_timeout_payload(
    state: &AppState,
    session_key: &str,
    timeout: f64,
    polls: u64,
    last: &Value,
    target: Option<i64>,
    cancel_on_timeout: bool,
    cancel_erase: bool,
) -> Value {
    let mut cancelled = false;
    let mut cancel_error: Option<String> = None;
    if cancel_on_timeout {
        match target {
            Some(id) => {
                match extension_control(
                    state,
                    session_key,
                    "downloads",
                    json!({ "action": "cancel", "id": id }),
                    Duration::from_secs(10),
                )
                .await
                {
                    Ok(_) => {
                        cancelled = true;
                        if cancel_erase {
                            // erase 单独一次调用：扩展的 cancel 分支只认 id，不处理 erase 字段。
                            if let Err(err) = extension_control(
                                state,
                                session_key,
                                "downloads",
                                json!({ "action": "erase", "id": id }),
                                Duration::from_secs(10),
                            )
                            .await
                            {
                                cancel_error = Some(humanize_extension_error(&err.to_string()));
                            }
                        }
                    }
                    Err(err) => cancel_error = Some(humanize_extension_error(&err.to_string())),
                }
            }
            None => cancel_error = Some("没有可取消的目标 id（本轮未认领到任何下载）".to_string()),
        }
    }
    let mut out = json!({
        "ok": false,
        "matched": false,
        "state": "timeout",
        "error": format!("download_wait_timeout: {timeout}s 内未完成"),
        "error_code": "download_wait_timeout",
        "hint": "下载可能仍在后台进行：用 `atb downloads` 查看，或加大 --timeout；\
                 需要超时即取消时设置 ATB_DOWNLOAD_CANCEL_ON_TIMEOUT=1",
        "item": last,
        "polls": polls,
        "cancelled": cancelled,
        "cancel_requested": cancel_on_timeout,
    });
    if let Some(err) = cancel_error {
        out["cancel_error"] = json!(err);
    }
    out
}

// ---------------------------------------------------------------- wait 判据辅助

fn download_state_of(item: &Value) -> &str {
    item.get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// `complete` / `interrupted` 是下载的终态，其余（in_progress / paused 等）都还在跑。
fn download_is_terminal(state: &str) -> bool {
    matches!(state, "complete" | "interrupted")
}

/// 「仍在进行中」：state 缺失的空串不算——没读到状态就认领，只会空等到超时。
fn download_is_running(state: &str) -> bool {
    !state.is_empty() && !download_is_terminal(state)
}

/// 条目开始时间（epoch 毫秒）。
///
/// 优先读毫秒字段 `startTimeMs`（扩展可能要补），缺失时退回 ISO8601 字符串：
/// 扩展的 `summarizeDownload` 目前给的是 `start_time: item.startTime`。
/// 全都读不到时返回 `None`——调用方会把「近期完成」这条判据跳过，
/// 绝不用 0 或当前时间兜底（那等于把所有历史记录都当成刚完成）。
fn download_start_ms(item: &Value) -> Option<i64> {
    for key in ["startTimeMs", "startTime", "start_time"] {
        match item.get(key) {
            Some(Value::Number(number)) => {
                if let Some(value) = number.as_i64() {
                    // 秒级时间戳也能用（1e10 ms 约等于 2286 年，足够区分）。
                    return Some(if value < 10_000_000_000 {
                        value.saturating_mul(1000)
                    } else {
                        value
                    });
                }
            }
            Some(Value::String(text)) => {
                if let Some(value) = parse_iso8601_ms(text) {
                    return Some(value);
                }
            }
            _ => {}
        }
    }
    None
}

/// 解析 `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`，返回 epoch 毫秒。
///
/// 只覆盖浏览器时间戳的实际形态（Chrome 的 `downloads.startTime` 是 UTC ISO8601），
/// 不引时间库：解析失败返回 `None`，由调用方跳过判据。
fn parse_iso8601_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let part =
        |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse::<i64>().ok() };
    if text.len() < 19 {
        return None;
    }
    let year = part(0..4)?;
    let month = part(5..7)?;
    let day = part(8..10)?;
    let hour = part(11..13)?;
    let minute = part(14..16)?;
    let second = part(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = text.get(19..)?;
    let mut millis: i64 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return None;
        }
        let mut frac = digits.clone();
        frac.truncate(3);
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac.parse::<i64>().ok()?;
        rest = fraction.get(digits.len()..)?;
    }
    let offset_seconds = match rest {
        "" | "Z" | "z" => 0,
        other => {
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = other.get(1..)?;
            let (offset_hour, offset_minute) = match body.split_once(':') {
                Some((hh, mm)) => (hh.parse::<i64>().ok()?, mm.parse::<i64>().ok()?),
                None if body.len() == 4 => (
                    body.get(0..2)?.parse::<i64>().ok()?,
                    body.get(2..4)?.parse::<i64>().ok()?,
                ),
                None => return None,
            };
            sign * (offset_hour * 3600 + offset_minute * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    Some(seconds.saturating_mul(1000) + millis)
}

/// 公历日期 → 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------- 多浏览器编排
