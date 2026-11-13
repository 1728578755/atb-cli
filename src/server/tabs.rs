use super::*;

/// 收尾：关闭某个调用方（或全部）通过 `open` 打开、且仍未关闭的标签页。
///
/// 为什么按「打开记录」而不是按 URL 猜：用户自己的标签页和智能体打开的标签页
/// 往往 URL 相同，按 URL 清理会误伤。这里只动本工具创建的会话。
pub(crate) async fn cleanup(
    State(state): State<AppState>,
    Json(req): Json<CleanupRequest>,
) -> Json<Value> {
    touch(&state).await;
    let agent = req.agent.unwrap_or_else(current_agent);
    let candidates: Vec<String> = {
        let opened = state.agent_opened.lock().await;
        if req.all_agents {
            let mut all: Vec<String> = opened.values().flatten().cloned().collect();
            all.sort();
            all.dedup();
            all
        } else {
            opened.get(&agent).cloned().unwrap_or_default()
        }
    };
    // 只处理仍然活着的会话；已关闭的直接从记录里剔除。
    let (alive, stale): (Vec<String>, Vec<String>) = {
        let driver = state.driver.read().await;
        candidates.into_iter().partition(|key| {
            driver
                .sessions
                .get(key)
                .map(|session| session.is_active())
                .unwrap_or(false)
        })
    };
    if !stale.is_empty() {
        let mut opened = state.agent_opened.lock().await;
        for list in opened.values_mut() {
            list.retain(|key| !stale.contains(key));
        }
    }

    let plan: Vec<Value> = {
        let driver = state.driver.read().await;
        alive
            .iter()
            .map(|key| {
                let info = driver.sessions.get(key).map(|session| session.info.clone());
                json!({
                    "session_key": key,
                    "tab_id": info.as_ref().map(|tab| tab.tab_id.clone()).unwrap_or_default(),
                    "title": info.as_ref().map(|tab| truncate_for_tree(&tab.title, 60)).unwrap_or_default(),
                    "url": info.as_ref().map(|tab| truncate_for_tree(&tab.url, 100)).unwrap_or_default(),
                })
            })
            .collect()
    };
    if req.dry_run {
        return Json(json!({
            "ok": true,
            "dry_run": true,
            "agent": agent,
            "count": plan.len(),
            "tabs": plan,
        }));
    }

    // 并发关闭 + 整体截止时间：串行 `for` 单次最长 15s，N≥5 时总耗时
    // 就超过 CLI 的 60s 请求超时——客户端先报失败，daemon 还在一个个关，用户重试
    // 又把已经关掉的会话算成 failed。限流并发（关闭是 IO 等待，4 路够了），
    // 并对整体预算设截止；到点还没回来的条目如实标 timeout。
    const CLEANUP_CONCURRENCY: usize = 4;
    const CLEANUP_BUDGET: Duration = Duration::from_secs(45);
    let permits = Arc::new(tokio::sync::Semaphore::new(CLEANUP_CONCURRENCY));
    // 截止时间用 tokio 的 Instant：`timeout_at` 不接受 std 的时钟。
    let deadline = tokio::time::Instant::now() + CLEANUP_BUDGET;
    let mut set = tokio::task::JoinSet::new();
    // 任务 id → session_key。panic 时 `JoinError` 只带任务 id，没有返回值；
    // 没有这张表就只能把它算成「超时」，hint 文案会误导用户去等一个已经崩掉的关闭。
    let mut task_keys: HashMap<tokio::task::Id, String> = HashMap::new();
    for key in alive.iter().cloned() {
        let task_state = state.clone();
        let permits = permits.clone();
        // 身份要跟着任务走：`current_agent()` 不会随 spawn 继承。
        let task_key = key.clone();
        let handle = set.spawn(with_agent(current_agent(), async move {
            let key = task_key;
            // S1-05 同类：拿不到许可就明确失败，不要「无许可继续跑」。
            let _permit = match permits.acquire_owned().await {
                Ok(permit) => permit,
                Err(err) => {
                    return (
                        key,
                        Err(anyhow!(
                            "concurrency_closed: 并发控制器已关闭，该标签未清理，可重试: {err}"
                        )),
                    );
                }
            };
            let outcome = close_session_by_key(&task_state, &key).await;
            (key, outcome)
        }));
        task_keys.insert(handle.id(), key);
    }

    let mut closed = 0usize;
    let mut failed: Vec<Value> = Vec::new();
    let mut finished: HashSet<String> = HashSet::new();
    loop {
        let joined = match tokio::time::timeout_at(deadline, set.join_next_with_id()).await {
            Ok(Some(joined)) => joined,
            // None = 全部任务结束；Err = 预算用完。
            Ok(None) | Err(_) => break,
        };
        match joined {
            Ok((_task, (key, Ok(())))) => {
                closed += 1;
                finished.insert(key.clone());
                record_cleanup_closed(&state, &key).await;
            }
            Ok((_task, (key, Err(err)))) => {
                finished.insert(key.clone());
                failed.push(json!({ "session_key": key, "error": err.to_string() }));
            }
            // panic 是 bug，不是「还没关完」。归到 `task_panicked`，
            // 既不进超时列表（用户不该等它），也不进成功计数。
            Err(err) => {
                eprintln!("cleanup: 关闭任务 panic（task_panicked）: {err}");
                if let Some(key) = task_keys.remove(&err.id()) {
                    finished.insert(key.clone());
                    failed.push(json!({
                        "session_key": key,
                        "error": format!("task_panicked: 关闭任务异常终止，标签状态未知（{err}）"),
                        "error_code": "task_panicked",
                        // 与 `atb schema` 的 error_codes 声明对齐（cli 域）：这个码
                        // 允许在复核目标状态后重试，所以是 retryable，但这不等于
                        // 「等一会儿就会好」——hint 明确要求先复核。
                        "retryable": true,
                        // 重试前必须先看 `errors[]` 里的 session_key：同一批 cleanup 里
                        // 可能有多条 panic，直接整批重试会对已经关掉的目标重复关闭。
                        // `cleanup --dry-run` 的输出正好是「现在会被关掉的集合」，与
                        // 「刚才哪些可能已关」对齐后就能只重试剩余部分。
                        "hint": "这是 atb 的 bug：附上 daemon 日志反馈；先看 errors[] 里已失败的 session_key，不要整批重试（该标签可能已关也可能没关）；`atb cleanup --dry-run` 可列出当前仍会被关闭的目标，据此只重试剩余部分",
                    }));
                }
            }
        }
    }
    // `set` 在这里 drop，仍在跑的任务会被 abort：关闭指令可能已经发出，
    // 但结果不再等。已关闭的标签会在下一次 tabs 快照里变成 stale 并被剔除。
    drop(set);
    let timed_out: Vec<String> = alive
        .iter()
        .filter(|key| !finished.contains(*key))
        .cloned()
        .collect();
    persist_agent_opened(&state);
    let timeouts: Vec<Value> = timed_out
        .iter()
        .map(|key| json!({ "session_key": key, "status": "timeout" }))
        .collect();
    Json(json!({
        // 有失败或有超时都不算 ok：超时意味着"可能还在关"，不是成功。
        "ok": failed.is_empty() && timed_out.is_empty(),
        "agent": agent,
        "closed": closed,
        "failed": failed.len(),
        "timed_out": timed_out.len(),
        "skipped_stale": stale.len(),
        "errors": failed,
        "timeouts": timeouts,
        "hint": if timed_out.is_empty() {
            Value::Null
        } else {
            json!("超时的标签可能仍在关闭中；稍后用 cleanup --dry-run 复核，不要立刻整批重试")
        },
    }))
}

/// 关闭成功后的记账：从「本工具打开过」与各智能体默认目标里剔除该会话。
///
/// 放在关闭任务里做（而不是回到主循环统一做），并发关闭时不需要再串行化一遍。
async fn record_cleanup_closed(state: &AppState, session_key: &str) {
    {
        let mut opened = state.agent_opened.lock().await;
        for list in opened.values_mut() {
            list.retain(|item| item != session_key);
        }
    }
    clear_agent_defaults_for(state, session_key).await;
}

// 记账前置：在**发出 closeTab 之前**就把这条会话从
/// 「本工具打开过」和各 agent 的默认目标里摘掉。
///
/// `cleanup` 有整体预算，到点 `drop(set)` 会 abort 在途任务：关闭指令可能已经发出，
/// 而 `forget_session` / `record_cleanup_closed` 还没执行。旧实现把记账全放在成功
/// 回包之后，于是 abort 之后账上还留着「已关闭的标签」——下一次 cleanup 会重复关它，
/// 默认目标也继续指着它。
///
/// 现在提前记账的代价是：closeTab 失败时这条会话暂时从账上消失。它仍在快照里，
/// 下一次 `tabs_update` 会按原 key 重新登记（并把默认目标还给本 agent），
/// 也就是「最多留一个由快照收敛的会话」，不会永久丢账。
///
/// 注：`Session` 没有 `closing` 字段（`state.rs` 不在本域白名单），所以这里只做
/// 记账前置，不把会话标成 `disconnected`——那会让紧随其后的 closeTab 因为
/// `select_tab` 只认 active 会话而根本发不出去。
///
/// 返回「原先登记过这条会话的 agent 列表」，供关闭失败时精确回滚。
async fn mark_session_closing(state: &AppState, session_key: &str) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    {
        let mut opened = state.agent_opened.lock().await;
        for (agent, list) in opened.iter_mut() {
            if list.iter().any(|item| item == session_key) {
                owners.push(agent.clone());
                list.retain(|item| item != session_key);
            }
        }
    }
    clear_agent_defaults_for(state, session_key).await;
    // 全局默认 / latest 同样要摘掉，否则 /health 会指着一条正在关闭的会话。
    let mut driver = state.driver.write().await;
    if driver.default_session_key.as_deref() == Some(session_key) {
        driver.default_session_key = None;
    }
    if driver.latest_session_key.as_deref() == Some(session_key) {
        driver.latest_session_key = None;
    }
    owners
}

/// closeTab 明确失败时把归属还回去（记账前置的可回滚面）。
///
/// 不回滚会让「标签还在、但账上已经没有」——下一次 cleanup 再也清不掉它。
// 任务被 abort 走不到这里，那正是 TA 允许的残留（由快照收敛）。
async fn restore_session_ownership(state: &AppState, session_key: &str, owners: &[String]) {
    let mut opened = state.agent_opened.lock().await;
    for agent in owners {
        let list = opened.entry(agent.clone()).or_default();
        if !list.iter().any(|item| item == session_key) {
            list.push(session_key.to_string());
        }
    }
}

/// 按会话 key 关闭标签页（cleanup 用；与 `/close` 共用同一套确认逻辑）。
pub(crate) async fn close_session_by_key(state: &AppState, session_key: &str) -> Result<()> {
    let physical_tab_id = session_tab_id(state, session_key).await?;
    let physical_tab_number = require_tab_id(&physical_tab_id)?;
    // TA：先把记账落下，再发关闭指令。
    let previous_owners = mark_session_closing(state, session_key).await;
    let payload = json!({ "cmd": "closeTab", "tabId": physical_tab_number }).to_string();
    if let Err(err) = execute_page_js(
        state,
        &payload,
        SessionSelector::new(Some(session_key.to_string()), None, None),
        true,
        Duration::from_secs(15),
    )
    .await
    {
        restore_session_ownership(state, session_key, &previous_owners).await;
        return Err(err);
    }
    {
        let mut execs = state.execs.lock().await;
        for pending in execs.take_session(session_key) {
            // 与 /close 同一处理：typed tab_closed（message 保持原文，向后兼容），
            // 调用方不再靠前缀字符串判断「标签关了，别重试同一 tab_id」。
            let _ = pending.tx.send(Err(tab_closed_error(
                "tab_closed: 标签页已关闭，命令未完成",
            )));
        }
    }
    forget_session(state, session_key).await;
    Ok(())
}

pub(crate) async fn root() -> &'static str {
    "atb"
}

pub(crate) async fn health(State(state): State<AppState>) -> Json<Value> {
    let active_tabs_count = active_tabs(&state, false).await.len();
    let extension_connected = has_extension_connection(&state).await;
    let mut profile_capabilities = extension_profile_capabilities(&state).await;
    // 默认目标落在哪个会话 / 哪个浏览器。默认失效或尚未建立时如实给 null。
    let (default_session_key, default_browser_id, default_profile_id) = {
        let driver = state.driver.read().await;
        let key = driver.default_session_key.clone();
        let session = key.as_deref().and_then(|key| driver.sessions.get(key));
        (
            key,
            session.map(|session| session.browser_id.clone()),
            session.map(|session| session.profile_id.clone()),
        )
    };
    // 让调用方直接看出「默认落在哪个浏览器/profile」。只加字段，
    // 既有字段与顺序不变（`extension_freshness` 只读 version 相关字段）。
    for profile in profile_capabilities.iter_mut() {
        let is_default = default_browser_id.is_some()
            && profile.get("browser_id").and_then(Value::as_str) == default_browser_id.as_deref()
            && profile.get("profile_id").and_then(Value::as_str) == default_profile_id.as_deref();
        if let Some(object) = profile.as_object_mut() {
            object.insert("is_default".to_string(), json!(is_default));
        }
    }
    let extension_freshness = extension_freshness(&profile_capabilities);
    let ready = extension_connected && active_tabs_count > 0;
    let uptime = state
        .started_at
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let idle_for = state.last_activity.lock().await.elapsed().as_secs_f64();
    let (jobs_running, jobs_total) = {
        let store = state.jobs.lock().await;
        (
            store
                .jobs
                .values()
                .filter(|job| job.status.is_running())
                .count(),
            store.jobs.len(),
        )
    };
    let ttl = state.idle_shutdown_ttl.as_secs_f64();
    // 含本次 /health 请求本身（`track_inflight` 在进入前已 +1）。
    let inflight = state.inflight.load(Ordering::Relaxed);
    // 信任配置只做「报告」，损坏也不能让 /health 自己失败：
    // 否则用户失去唯一能看到「为什么连不上」的入口。
    let compatibility = compatibility_value(&state).await;
    // 联动：除了「白名单几条 / 要不要 token」，还必须如实报告**当前档位**
    // 与「兼容档的风险」。`atb status` 把这段原样透出，是用户唯一能看见
    // 「任何本机进程都能接管浏览器」的地方；只报数字等于默认放行但不说。
    let ws_trust_mode = crate::settings::ws_trust_mode();
    let ws_security = match load_ws_trust() {
        WsTrustState::Ok(ws_trust) => json!({
            "mode": ws_trust_mode,
            "trusted_ids": ws_trust.extension_ids.len(),
            "token_required": ws_trust.token_value().is_some(),
            "trust_config_error": Value::Null,
            "warning": if ws_trust_mode == "compat" {
                json!("当前为兼容档：任何本机进程都可能连入扩展端口并接管本浏览器；建议用 atb trust-extension --token 切到 token 档")
            } else {
                Value::Null
            },
        }),
        // 数字不伪造：此时白名单根本没读出来，0/false 只是「未知」的占位，
        // 真实原因看 `trust_config_error`（非 null 即代表握手正在被拒绝）。
        // 配置损坏时警告优先于档位风险：此时连「兼容档放行」都不是主要问题。
        WsTrustState::Corrupt(detail) => json!({
            "mode": ws_trust_mode,
            "trusted_ids": 0,
            "token_required": false,
            "trust_config_error": detail,
            "warning": format!("信任配置损坏，握手正在被拒绝：{detail}"),
        }),
    };
    let configured_extension_port = config::load_existing()
        .map(|config| config.extension_port)
        .unwrap_or(state.extension_port);
    let ws_bind_error = state.ws_bind_error.lock().await.clone();
    Json(json!({
        "ok": true,
        "running": true,
        "ready": ready,
        "ports": {
            "api": API_PORT,
            "extension": {
                "configured": configured_extension_port,
                "listening": state.extension_port,
                "matched": configured_extension_port == state.extension_port,
                "bind_error": ws_bind_error
            }
        },
        "connection": {
            "extension_connected": extension_connected,
            "active_tabs": active_tabs_count,
            // 隐式默认落在哪个会话 / 哪个浏览器；没有默认时是 null（不伪造）。
            "default_session_key": default_session_key,
            "default_browser_id": default_browser_id,
            "profiles": profile_capabilities
        },
        "extension": extension_freshness,
        // 扩展自检报错（R-19）：非空就说明它那边已经有功能降级了，
        // 比让用户盯着「没有可用标签页」猜要直接得多。
        "extension_warnings": state.ext_warnings.lock().await.clone(),
        "compatibility": compatibility,
        "protocol": crate::protocol::PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": uptime,
        "idle_for": idle_for,
        "ttl": ttl,
        "ttl_remaining": (ttl - idle_for).max(0.0),
        "inflight": inflight,
        "ws_security": ws_security,
        "jobs": {
            "running": jobs_running,
            "total": jobs_total,
            // 真正的执行并发上限（`ATB_MAX_RUNNING_JOBS` 可调）。
            // 和 `total`/`running` 一起给出，才看得出「被拒绝」是不是并发打满了。
            "max_running": crate::settings::max_running_jobs(),
        }
    }))
}

/// `POST /reload-trust`：让 `ws-trust.json` 的变更立刻作用于**已建立**的连接。
///
/// 握手阶段每次都会重读配置，但已经连上的扩展不会因为文件变化而受影响：
/// 用户跑 `trust-extension <自己的 id>` 加固后，此前连上的可疑扩展仍然能收命令。
/// 这个端点补上那一步：重读配置，把不再可信的连接置撤销位（500ms 内收到 Close）。
///
/// 规则：
/// - 白名单非空且不含该 `extension_id` → 撤销（`untrusted_id`）；
/// - 当前配置有 token → 连接的 token 指纹必须匹配，否则撤销（`token_mismatch`）；
/// - 白名单为空且无 token = 兼容模式，不踢人（否则用户刚清空白名单，所有连接会瞬间全断）；
/// - 配置损坏 → fail-closed，撤销全部（`corrupt_config`，与握手侧一致）。
///
/// 鉴权跟随其它 API：注册在 `require_api_access` 层内，需要 `X-Agent-Browser-Token`。
pub(crate) async fn reload_trust(State(state): State<AppState>) -> Json<Value> {
    let trust = load_ws_trust();
    let (revoked, checked): (Vec<Value>, usize) = {
        // 只在这一次同步加锁：遍历登记表并把撤销位置位，回调里不做任何 await。
        let conns = ws_connections_lock(&state);
        let checked = conns.len();
        let mut revoked: Vec<Value> = Vec::new();
        for (conn_id, conn) in conns.iter() {
            // 生产路径就走这条被单测覆盖的谓词，避免"测的东西"和"跑的东西"分叉。
            if !ws_trust_should_revoke(&trust, &conn.extension_id, conn.token_fingerprint) {
                continue;
            }
            // 只有真要撤销时才取一次原因（纯函数，与上面的判定同源，不会打架）。
            let reason = ws_trust_revoke_reason(&trust, &conn.extension_id, conn.token_fingerprint)
                .unwrap_or("untrusted_id");
            // swap 的返回值去重：已经撤销过的连接不再重复上报。
            if !conn.revoked.swap(true, Ordering::Relaxed) {
                revoked.push(json!({
                    "extension_id": conn.extension_id,
                    "connection_id": conn_id,
                    "reason": reason,
                }));
            }
        }
        (revoked, checked)
    };
    let error = match &trust {
        WsTrustState::Corrupt(detail) => Some(detail.clone()),
        WsTrustState::Ok(_) => None,
    };
    match error {
        Some(err) => Json(json!({
            "ok": false,
            "error": err,
            "revoked": revoked,
            "checked": checked,
        })),
        None => Json(json!({
            "ok": true,
            "revoked": revoked,
            "checked": checked,
        })),
    }
}

/// `/tabs` 查询参数的扩展包装：`TabsQuery` 在共享 DTO 里（dto.rs），本轮用
/// serde(flatten) 包一层追加过滤参数，旧字段语义与命名完全不变。
#[derive(Debug, Deserialize)]
pub(crate) struct TabsQueryExt {
    #[serde(flatten)]
    pub(crate) inner: TabsQuery,
    // 只返回本 agent 通过 `open` 打开的标签页。
    /// 旧实现 tabs 混着用户自己开的页面，智能体按列表挑清理目标会误伤。
    ///
    /// flatten 路径上的 bool 会以字符串到达（见 dto.rs::de_boolish）。
    #[serde(default, deserialize_with = "de_boolish")]
    pub(crate) mine: bool,
    // 只保留 URL 含该子串的标签页（大小写不敏感）。。
    #[serde(default, alias = "urlContains")]
    pub(crate) url_contains: Option<String>,
    // 只保留标题含该子串的标签页（大小写不敏感）。。
    #[serde(default, alias = "titleContains")]
    pub(crate) title_contains: Option<String>,
    /// `--url-contains` / `--title-contains` 的简短别名：URL 或标题任一命中即可。
    #[serde(default)]
    pub(crate) filter: Option<String>,
}

/// 大小写不敏感的子串匹配；空子串等同「不过滤」。
fn contains_ci(haystack: &str, needle: &str) -> bool {
    needle.is_empty() || haystack.to_lowercase().contains(&needle.to_lowercase())
}

// `/tabs` 的过滤谓词。
///
/// 三个条件同时给时是 **AND** 关系；`filter` 自身是「URL 或标题命中」的 OR。
/// 放在 daemon 侧、字段映射之前执行，保证 `tabs_total` / `filtered` 与最终
/// 输出基于同一份数据。
pub(crate) fn tab_matches_filters(
    tab: &TabInfo,
    url_contains: Option<&str>,
    title_contains: Option<&str>,
    filter: Option<&str>,
) -> bool {
    let url_ok = url_contains
        .map(|needle| contains_ci(&tab.url, needle))
        .unwrap_or(true);
    let title_ok = title_contains
        .map(|needle| contains_ci(&tab.title, needle))
        .unwrap_or(true);
    let filter_ok = filter
        .map(|needle| contains_ci(&tab.url, needle) || contains_ci(&tab.title, needle))
        .unwrap_or(true);
    url_ok && title_ok && filter_ok
}

/// 大列表提示：过滤后仍超过 `ATB_TABS_LARGE_WARNING` 时给一条可执行的收敛建议。
pub(crate) const TABS_LARGE_SUGGEST: &str =
    "--fields tab_id,title,url --limit 20 或 --url-contains <子串> 可显著缩小输出";

pub(crate) fn tabs_large_suggest(filtered: usize) -> Option<&'static str> {
    (filtered > crate::settings::tabs_large_warning()).then_some(TABS_LARGE_SUGGEST)
}

pub(crate) async fn tabs(
    State(state): State<AppState>,
    Query(query): Query<TabsQueryExt>,
) -> Json<Value> {
    touch(&state).await;
    // 只读命令：浏览器没开时不该白等 5 秒。
    wait_for_sessions(&state, WAIT_SESSIONS_READ).await;
    // 标签列表必须反映「刚刚发生的事」：扩展推送受 Service Worker 调度影响，
    // 可能延迟十几秒。先主动对齐一次，失败就用现有快照（不阻塞、不报错）。
    let _ = refresh_tabs_via_extension(&state, None, Duration::from_millis(1500)).await;
    // mine 过滤的依据是 agent_opened（各 agent open 过的 session_key 登记）。
    // 一次性取出来，避免在下面的过滤闭包里反复拿锁。
    let mine_keys: Option<HashSet<String>> = if query.mine {
        let opened = state.agent_opened.lock().await;
        Some(
            opened
                .get(&current_agent())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        )
    } else {
        None
    };
    match active_tabs_filtered(
        &state,
        false,
        query.inner.browser.as_deref(),
        query.inner.profile.as_deref(),
    )
    .await
    {
        Ok(tabs) => {
            // 过滤在 daemon 侧、字段映射之前完成。`tabs_total` 是过滤前
            // 数量，`filtered` 是 mine + 子串条件叠加后的数量；两者都比对同一份快照，
            // 调用方能直接看出「列表短了是因为过滤还是因为浏览器真的没标签」。
            let tabs_total = tabs.len();
            let items: Vec<Value> = tabs
                .iter()
                // mine=true 时只留本 agent open 过的；会话没有登记（用户手开的、
                // 旧版本打开的）一律不算「我的」，宁可少列不可误伤。
                .filter(|tab| {
                    mine_keys
                        .as_ref()
                        .map(|keys| keys.contains(&tab.session_key))
                        .unwrap_or(true)
                })
                .filter(|tab| {
                    tab_matches_filters(
                        tab,
                        query.url_contains.as_deref(),
                        query.title_contains.as_deref(),
                        query.filter.as_deref(),
                    )
                })
                .map(|tab| tabs_display_json(tab, query.inner.full))
                .collect();
            let filtered = items.len();
            let suggest = tabs_large_suggest(filtered);
            let mut metadata = if items.is_empty() {
                if tabs_total > 0 {
                    // 有标签、但被 mine 或子串条件全部过滤掉：不能再报「浏览器没标签」，
                    // 否则智能体会去重启浏览器而不是放宽过滤条件。
                    json!({
                        "tabs_count": 0,
                        "tabs": items,
                        "tabs_total": tabs_total,
                        "filtered": 0,
                        "warning": "没有标签页匹配过滤条件",
                        "warning_code": "no_matching_tabs",
                        "retryable": false,
                        "hint": "放宽 --url-contains / --title-contains / --filter，或去掉 --mine 后重试",
                    })
                } else if query.mine {
                    json!({
                        "tabs_count": 0,
                        "tabs": items,
                        "tabs_total": 0,
                        "filtered": 0,
                        "warning": "本 agent 没有打开过（或打开的标签页都已关闭）",
                        "hint": "确认 Chrome 已打开且扩展已连接（status）；或先 open 一个页面。扩展重连最长约 30-60 秒",
                    })
                } else {
                    // 旧写法 `&state.driver.read().await` 传的是 &RwLockReadGuard，
                    // 这里显式解引用拿 &DriverState（has_live_bridge 的实参类型）。
                    let bridge_alive = has_live_bridge(&*state.driver.read().await);
                    let (warning_code, retryable) = if bridge_alive {
                        ("no_target_tabs", false)
                    } else {
                        ("extension_disconnected", true)
                    };
                    let mut metadata = json!({
                        "tabs_count": 0,
                        "tabs": items,
                        "tabs_total": 0,
                        "filtered": 0,
                        "warning": "当前没有可操作标签页",
                        "warning_code": warning_code,
                        "retryable": retryable,
                        "hint": "确认 Chrome 已打开且扩展已连接（status）；或先 open 一个页面。扩展重连最长约 30-60 秒",
                    });
                    if warning_code == "extension_disconnected" {
                        metadata["retry_after_ms"] = json!(3000);
                    }
                    metadata
                }
            } else {
                let mut metadata = json!({
                    "tabs_count": items.len(),
                    "tabs": items,
                    "tabs_total": tabs_total,
                    "filtered": filtered,
                });
                // 只增字段：大列表给一条收敛建议。daemon 不知道 CLI 侧有没有传
                // `--fields` / `--limit`（那是纯客户端参数），由 CLI 在裁剪时摘掉。
                if let Some(suggest) = suggest {
                    metadata["suggest"] = json!(suggest);
                }
                metadata
            };
            if query.mine {
                // 回显过滤条件：智能体看到 mine:true 才知道列表不是全量。
                if let Some(object) = metadata.as_object_mut() {
                    object.insert("mine".to_string(), json!(true));
                }
            }
            Json(json!({ "ok": true, "result": { "status": "success", "metadata": metadata } }))
        }
        // 旧实现手写纯文本 JSON：active_tabs_filtered 的歧义错误 typed 化后，
        // 这里 downcast 输出稳定码 target_ambiguous，文案不变。
        Err(err) => Json(session_error_json(&err)),
    }
}

/// 展示层截断：数据模型里保留完整 URL，输出按需缩短。
pub(crate) fn tabs_display_json(tab: &TabInfo, full: bool) -> Value {
    let mut value = tabs_display_json_inner(tab, full);
    if let Some(object) = value.as_object_mut() {
        if let Some(group) = &tab.group_title {
            object.insert("group".to_string(), json!(group));
        }
        if tab.active {
            object.insert("active".to_string(), json!(true));
        }
    }
    value
}

pub(crate) fn tabs_display_json_inner(tab: &TabInfo, full: bool) -> Value {
    let url = if full {
        tab.url.clone()
    } else {
        truncate_for_tree(&tab.url, 80)
    };
    json!({
        "id": tab.id,
        "tab_id": tab.tab_id,
        "window": tab.window_id,
        "browser_id": tab.browser_id,
        "browser_label": tab.browser_label,
        "profile_id": tab.profile_id,
        "profile_label": tab.profile_label,
        "session_key": tab.session_key,
        "url": url,
        "title": tab.title,
        "type": tab.tab_type,
        "connected_at": tab.connected_at,
    })
}

pub(crate) async fn tabtree(
    State(state): State<AppState>,
    Query(query): Query<TabtreeQuery>,
) -> Json<Value> {
    touch(&state).await;
    // 只读命令：浏览器没开时用短等待，别再白等 5 秒。
    wait_for_sessions(&state, WAIT_SESSIONS_READ).await;
    // TA：与 `tabs` 一样先对齐一次快照。旧实现直接读内存表，扩展推送延迟
    // （SW 被节流时可达十几秒）会让 tabtree 显示一棵过期的树；刷新失败就用现有
    // 快照（不阻塞、不报错）。
    let _ = refresh_tabs_via_extension(&state, None, Duration::from_millis(1500)).await;
    // 与 `tabs` / 选择路径保持同一判据：`--browser Edge` 同时命中
    // `Edge` 与 `Edge-Beta` 时报歧义，而不是把两个浏览器并成一棵树。
    if let Some(browser) = query.browser.as_deref() {
        let driver = state.driver.read().await;
        let matched: Vec<&Session> = driver
            .sessions
            .values()
            .filter(|s| s.is_active() && browser_selector_matches(s, browser))
            .filter(|s| {
                query
                    .profile
                    .as_deref()
                    .map(|profile| {
                        s.profile_id == profile || s.profile_label.as_deref() == Some(profile)
                    })
                    .unwrap_or(true)
            })
            .collect();
        let candidates = ambiguous_browser_candidates(&matched);
        if candidates.len() > 1 {
            return Json(json!({
                "ok": false,
                "error": browser_ambiguity_message(browser, &candidates),
                // 旧实现纯文本：补稳定码（改过滤条件即可解）与提示。
                "error_code": "target_ambiguous",
                "retryable": false,
                "hint": "写更精确的 browser 标签或完整 browser_id；browsers 命令可列出全部实例",
            }));
        }
    }
    let items: Vec<SessionTreeItem> = {
        let driver = state.driver.read().await;
        driver
            .sessions
            .values()
            .filter(|s| s.is_active())
            .filter(|s| {
                query
                    .browser
                    .as_deref()
                    .map(|browser| browser_selector_matches(s, browser))
                    .unwrap_or(true)
            })
            .filter(|s| {
                query
                    .profile
                    .as_deref()
                    .map(|profile| {
                        s.profile_id == profile || s.profile_label.as_deref() == Some(profile)
                    })
                    .unwrap_or(true)
            })
            .filter(|s| {
                query
                    .tab
                    .as_deref()
                    .map(|tab| s.tab_id == tab || s.session_key == tab)
                    .unwrap_or(true)
            })
            .map(|s| SessionTreeItem {
                browser_id: s.browser_id.clone(),
                profile_id: s.profile_id.clone(),
                profile_label: s.profile_label.clone(),
                tab_id: s.tab_id.clone(),
                session_key: s.session_key.clone(),
                title: s.info.title.clone(),
                url: s.info.url.clone(),
            })
            .collect()
    };

    let mut browser_map: HashMap<String, HashMap<String, Vec<SessionTreeItem>>> = HashMap::new();
    for item in items {
        browser_map
            .entry(item.browser_id.clone())
            .or_default()
            .entry(item.profile_id.clone())
            .or_default()
            .push(item);
    }

    let mut browsers: Vec<Value> = browser_map
        .into_iter()
        .map(|(browser_id, profiles)| {
            let mut profile_nodes: Vec<Value> = profiles
                .into_iter()
                .map(|(profile_id, mut sessions)| {
                    // TA：与 `tabs` / `--tab @N` 共用同一判据（数值序，tab id 是
                    // Chrome 递增整数）。旧实现按字符串排序，`10` 会排在 `9` 前面，
                    // 于是 tabtree 的序号和 tabs 的序号指向不同标签。
                    sessions.sort_by(|a, b| {
                        tab_numeric_order(&a.tab_id)
                            .cmp(&tab_numeric_order(&b.tab_id))
                            .then_with(|| a.tab_id.cmp(&b.tab_id))
                    });
                    let profile_label = sessions.iter().find_map(|s| s.profile_label.clone());
                    let tabs: Vec<Value> = sessions
                        .iter()
                        .map(|s| tabtree_tab_json(s, query.full))
                        .collect();
                    json!({
                        "profile_id": profile_id,
                        "profile_label": profile_label,
                        "tabs_count": tabs.len(),
                        "tabs": tabs,
                    })
                })
                .collect();
            profile_nodes.sort_by(|a, b| {
                a.get("profile_label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("profile_label").and_then(Value::as_str).unwrap_or(""))
                    .then_with(|| {
                        a.get("profile_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .cmp(b.get("profile_id").and_then(Value::as_str).unwrap_or(""))
                    })
            });
            let tabs_count: usize = profile_nodes
                .iter()
                .map(|p| p.get("tabs_count").and_then(Value::as_u64).unwrap_or(0) as usize)
                .sum();
            json!({
                "browser_id": browser_id,
                "profiles_count": profile_nodes.len(),
                "tabs_count": tabs_count,
                "profiles": profile_nodes,
            })
        })
        .collect();
    browsers.sort_by(|a, b| {
        a.get("browser_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("browser_id").and_then(Value::as_str).unwrap_or(""))
    });
    let profiles_count: usize = browsers
        .iter()
        .map(|b| b.get("profiles_count").and_then(Value::as_u64).unwrap_or(0) as usize)
        .sum();
    let tabs_count: usize = browsers
        .iter()
        .map(|b| b.get("tabs_count").and_then(Value::as_u64).unwrap_or(0) as usize)
        .sum();
    Json(json!({
        "ok": true,
        "result": {
            "status": "success",
            "compact": !query.full,
            "filters": {
                "tab": query.tab,
                "browser": query.browser,
                "profile": query.profile,
            },
            "browsers_count": browsers.len(),
            "profiles_count": profiles_count,
            "tabs_count": tabs_count,
            "browsers": browsers,
        }
    }))
}

pub(crate) fn tabtree_tab_json(item: &SessionTreeItem, full: bool) -> Value {
    if full {
        json!({
            "tab_id": item.tab_id,
            "session_key": item.session_key,
            "title": item.title,
            "url": item.url,
        })
    } else {
        json!({
            "tab_id": item.tab_id,
            "title": item.title,
            "url": truncate_for_tree(&item.url, 120),
        })
    }
}

pub(crate) fn truncate_for_tree(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    format!("{}...", input.chars().take(max_chars).collect::<String>())
}

pub(crate) async fn lookup_tab(
    State(state): State<AppState>,
    AxumPath(tab_id): AxumPath<String>,
    Query(query): Query<LookupTabQuery>,
) -> Json<Value> {
    touch(&state).await;
    // 只读命令：浏览器没开时用短等待，别再白等 5 秒。
    wait_for_sessions(&state, WAIT_SESSIONS_READ).await;
    let driver = state.driver.read().await;
    let matches: Vec<&Session> = driver
        .sessions
        .values()
        .filter(|s| s.is_active())
        .filter(|s| s.tab_id == tab_id || s.session_key == tab_id)
        .filter(|s| {
            query
                .browser
                .as_deref()
                .map(|browser| s.browser_id == browser)
                .unwrap_or(true)
        })
        .filter(|s| {
            query
                .profile
                .as_deref()
                .map(|profile| {
                    s.profile_id == profile || s.profile_label.as_deref() == Some(profile)
                })
                .unwrap_or(true)
        })
        .collect();
    if matches.is_empty() {
        // 旧实现只说「tab xxx 未连接」：分不清 id 写错 / 标签已关 / 整个浏览器下线。
        // 按全表（含已断开）细分，给稳定码与重试策略。
        let stale = driver
            .sessions
            .values()
            .find(|s| s.tab_id == tab_id || s.session_key == tab_id);
        return Json(match stale {
            // 会话活着但没进 matches：被 --browser/--profile 过滤掉了，目标属于另一个 browser。
            Some(session) if session.is_active() => session_error_json(&target_select_error(
                "session_mismatch",
                format!("tab {tab_id} 未在当前 --browser/--profile 过滤范围内"),
                "该标签属于另一个 browser/profile：去掉过滤条件，或改用它所属的 browser",
            )),
            Some(session) => {
                let dead = dead_tab_count_of_browser(&driver, &session.browser_id);
                session_error_json(&target_select_error(
                    "session_gone",
                    format!(
                        "tab {tab_id} 未连接：其所属浏览器实例已下线（扩展重载或浏览器重启），名下 {dead} 个标签失效"
                    ),
                    "不要复用旧 tab_id：重新 open 目标页面，或先 tabs 拿新列表",
                ))
            }
            _ if !has_live_bridge(&driver) => session_error_json(&bridge_disconnected_error(
                format!("tab {tab_id} 未连接（扩展桥未连接，浏览器可能未打开）"),
            )),
            _ => session_error_json(&target_select_error(
                "target_not_found",
                format!("tab {tab_id} 未连接"),
                "id 写错或标签刚被关闭：tabs --ids 可看当前完整列表",
            )),
        });
    }
    if matches.len() > 1 {
        let sessions: Vec<Value> = matches.iter().map(|s| session_lookup_json(s)).collect();
        return Json(json!({
            "ok": false,
            "error": format!("tab {tab_id} 存在歧义，匹配到 {} 个会话，请补 --profile 或 --browser", sessions.len()),
            // 旧实现没有稳定码：CLI 只能靠「存在歧义」子串猜。补 target_ambiguous
            //（改命令类）与 matches 归属字段，智能体可直接挑 session_key。
            "error_code": "target_ambiguous",
            "retryable": false,
            "hint": "从 matches 里挑准确的 session_key，或补 --profile / --browser",
            "matches": sessions,
        }));
    }
    Json(json!({ "ok": true, "result": session_lookup_json(matches[0]) }))
}

pub(crate) async fn lookup_browser(
    State(state): State<AppState>,
    AxumPath(browser_id): AxumPath<String>,
) -> Json<Value> {
    touch(&state).await;
    // 只读命令：浏览器没开时用短等待，别再白等 5 秒。
    wait_for_sessions(&state, WAIT_SESSIONS_READ).await;
    let driver = state.driver.read().await;
    let sessions: Vec<&Session> = driver
        .sessions
        .values()
        .filter(|s| s.is_active() && s.browser_id == browser_id)
        .collect();
    if sessions.is_empty() {
        // 旧实现只说「browser xxx 未连接」：分不清 id 写错还是桥断。
        // 桥断开是唯一可重试病因，单独给码 + retry_after_ms。
        return Json(if has_live_bridge(&driver) {
            session_error_json(&target_select_error(
                "target_not_found",
                format!("browser {browser_id} 未连接"),
                "browser_id 写错或该浏览器已关闭：browsers 命令可列出当前实例",
            ))
        } else {
            session_error_json(&bridge_disconnected_error(format!(
                "browser {browser_id} 未连接（扩展桥未连接，浏览器可能未打开）"
            )))
        });
    }
    // 按 profile 归并并查歧义：旧实现直接取 `sessions[0]` 的 profile，
    // 却用**全部** profile 的会话数当 `tabs_count`，同一浏览器开多个 profile 时
    // 返回的 profile_id 与实际统计对不上，也不像 `lookup_profile` 那样报歧义。
    let profile_ids: HashSet<String> = sessions.iter().map(|s| s.profile_id.clone()).collect();
    if profile_ids.len() > 1 {
        let mut ids: Vec<&str> = profile_ids.iter().map(String::as_str).collect();
        ids.sort_unstable();
        return Json(json!({
            "ok": false,
            "error": format!(
                "browser {browser_id} 下存在多个 profile（{}），请改用 lookup profile <profile_id|label>",
                ids.join(", ")
            ),
            "profile_ids": ids,
            "matches": sessions.iter().map(|s| session_lookup_json(s)).collect::<Vec<_>>(),
        }));
    }
    let first = sessions[0];
    Json(json!({
        "ok": true,
        "result": {
            "browser_id": browser_id,
            "profile_id": first.profile_id,
            "profile_label": first.profile_label,
            "profiles_count": 1,
            "tabs_count": sessions.len(),
            "tabs": sessions.iter().map(|s| session_lookup_json(s)).collect::<Vec<_>>(),
        }
    }))
}

pub(crate) async fn lookup_profile(
    State(state): State<AppState>,
    AxumPath(profile): AxumPath<String>,
) -> Json<Value> {
    touch(&state).await;
    // 只读命令：浏览器没开时用短等待，别再白等 5 秒。
    wait_for_sessions(&state, WAIT_SESSIONS_READ).await;
    let driver = state.driver.read().await;
    let sessions: Vec<&Session> = driver
        .sessions
        .values()
        .filter(|s| {
            s.is_active()
                && (s.profile_id == profile || s.profile_label.as_deref() == Some(profile.as_str()))
        })
        .collect();
    if sessions.is_empty() {
        // 同 lookup_browser：细分「profile 写错」（改命令）与「桥断开」（可重试）。
        return Json(if has_live_bridge(&driver) {
            session_error_json(&target_select_error(
                "target_not_found",
                format!("profile {profile} 未连接"),
                "profile id/label 写错或该 profile 没有打开的标签页：tabs --ids 可看归属",
            ))
        } else {
            session_error_json(&bridge_disconnected_error(format!(
                "profile {profile} 未连接（扩展桥未连接，浏览器可能未打开）"
            )))
        });
    }
    let profile_ids: HashSet<String> = sessions.iter().map(|s| s.profile_id.clone()).collect();
    if profile_ids.len() > 1 {
        let matches: Vec<Value> = sessions.iter().map(|s| session_lookup_json(s)).collect();
        return Json(json!({
            "ok": false,
            "error": format!("profile {profile:?} 存在歧义，匹配到多个 profile"),
            "error_code": "target_ambiguous",
            "retryable": false,
            "hint": "改用完整 profile_id，或先给 profile 起唯一 label（profile-label set）",
            "matches": matches,
        }));
    }
    let first = sessions[0];
    let mut browser_ids: Vec<String> = sessions.iter().map(|s| s.browser_id.clone()).collect();
    browser_ids.sort();
    browser_ids.dedup();
    Json(json!({
        "ok": true,
        "result": {
            "profile_id": first.profile_id,
            "profile_label": first.profile_label,
            "browser_ids": browser_ids,
            "browser_count": browser_ids.len(),
            "tabs_count": sessions.len(),
            "tabs": sessions.iter().map(|s| session_lookup_json(s)).collect::<Vec<_>>(),
        }
    }))
}

pub(crate) fn session_lookup_json(session: &Session) -> Value {
    json!({
        "browser_id": session.browser_id,
        "profile_id": session.profile_id,
        "profile_label": session.profile_label,
        "tab_id": session.tab_id,
        "session_key": session.session_key,
        "title": session.info.title,
        "url": session.info.url,
    })
}

pub(crate) async fn set_profile_label(
    State(state): State<AppState>,
    Json(req): Json<ProfileLabelRequest>,
) -> Json<Value> {
    touch(&state).await;
    let result = update_profile_label(&state, req).await;
    // 旧实现纯文本透传：typed 错误（session_gone 等）的结构化字段在这里被剥掉。
    Json(match result {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => session_error_json(&err),
    })
}

/// cleanup / close 这类"必须落到具体标签"的路径不允许空 tab_id。
///
/// **`parse_tab_id("")` 返回 `Err`（`session_gone` 语义），不再返回 `Ok(0)`**：
/// 只有「这条命令根本不需要 tabId」的路径（`exec` 等）才在更上层跳过调用，
/// 一旦走到这里，空串就说明会话记录已经坏了。旧注释写成「空串返回 Ok(0)」，
// 与实现相反，照着注释改代码会把 `tabId:0` 发给扩展，错误归因也被带偏。
/// 关闭标签时必须落到具体 tabId，所以在入口再拦一次并给 typed `session_gone`。
pub(crate) fn require_tab_id(raw: &str) -> Result<i64> {
    if raw.trim().is_empty() {
        // 旧实现裸 anyhow：这是「会话记录已失效」（tabId 丢了）而非参数写错，
        // typed 化为 session_gone，避免调用方把重试打在同一个坏会话上。
        // message 保持原文，向后兼容。
        return Err(target_select_error(
            "session_gone",
            "会话的 tabId 为空（tabId 必填），请 refresh 或重新 open",
            "会话记录已失效（扩展重载或标签关闭）：重新 open，或执行 tabs 后换目标",
        ));
    }
    parse_tab_id(raw)
}

pub(crate) fn normalize_profile_label(label: Option<String>) -> Result<Option<String>> {
    let Some(label) = label else {
        return Ok(None);
    };
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > 40 {
        return Err(anyhow!("profile label 长度不能超过 40 个字符"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(anyhow!("profile label 只能包含英文、数字、-、_、."));
    }
    Ok(Some(trimmed.to_string()))
}

pub(crate) async fn update_profile_label(
    state: &AppState,
    req: ProfileLabelRequest,
) -> Result<Value> {
    let label = normalize_profile_label(req.label)?;
    // 串行化「查重 → 通知扩展 → 写回内存」全过程：查重和写入之间隔着一次最长
    // 10s 的扩展往返，只靠读写锁两个并发请求都能通过查重，最后同一个 label 挂到
    // 两个 profile 上，之后按 label 选 profile 就开始歧义。
    let _label_guard = state.profile_label_lock.lock().await;
    let session_key = select_tab(
        state,
        SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
    )
    .await?;
    let (profile_id, browser_id, tab_id, sender, previous_label) = {
        let driver = state.driver.read().await;
        let session = driver
            .sessions
            .get(&session_key)
            .filter(|s| s.is_active())
            .ok_or_else(|| {
                // select_tab 之后瞬间失效（快照把会话标断开）：归类 session_gone，
                // 不要让调用方误以为 label 参数写错。
                target_select_error(
                    "session_gone",
                    format!("会话ID {session_key} 未连接"),
                    "会话在命令执行间隙失效（扩展重载或标签关闭）：重新 open 或用 tabs 另选目标",
                )
            })?;
        // 查重必须在**副作用之前**：旧实现先让扩展落盘并广播，再在写内存时
        // 二次查重；冲突时扩展侧已经挂上同名 label，两个 profile 共享一个 label，
        // 之后 `--profile <label>` 恒报歧义，而且没有任何命令能改回去。
        // `profile_label_lock` 已经把 daemon 侧并发请求串起来，这一次查重就是权威判定。
        if let Some(label) = label.as_deref() {
            let duplicate = driver.sessions.values().find(|s| {
                s.is_active()
                    && s.profile_id != session.profile_id
                    && s.profile_label.as_deref() == Some(label)
            });
            if let Some(duplicate) = duplicate {
                return Err(anyhow!(
                    "profile label {label:?} 已被 profile {}（浏览器 {}）使用",
                    duplicate.profile_id,
                    duplicate.browser_id
                ));
            }
        }
        (
            session.profile_id.clone(),
            session.browser_id.clone(),
            session.tab_id.clone(),
            session.sender.clone(),
            session.profile_label.clone(),
        )
    };

    send_profile_label_to_extension(state, &session_key, &tab_id, &sender, label.as_deref())
        .await?;

    let mut driver = state.driver.write().await;
    // 写入锁内再确认一次：串行锁之外仍可能被扩展的 tabs_update 带回同名 label
    // （例如另一个浏览器进程侧改过）。真撞上时不能只报错——扩展侧已经落盘了，
    // 必须回滚，否则扩展继续用新 label、daemon 拒绝写入，两边永久分叉。
    if let Some(label) = label.as_deref() {
        let conflict = driver
            .sessions
            .values()
            .find(|session| {
                session.is_active()
                    && session.profile_id != profile_id
                    && session.profile_label.as_deref() == Some(label)
            })
            .map(|session| (session.browser_id.clone(), session.profile_id.clone()));
        if let Some((other_browser, other_profile)) = conflict {
            drop(driver);
            let rollback = send_profile_label_to_extension(
                state,
                &session_key,
                &tab_id,
                &sender,
                previous_label.as_deref(),
            )
            .await;
            return Err(anyhow!(match rollback {
                Ok(()) => format!(
                    "profile label {label:?} 已被 profile {other_profile}（浏览器 {other_browser}）占用，已把扩展侧 label 回滚为原值；请换一个 label"
                ),
                Err(err) => format!(
                    "profile label {label:?} 已被 profile {other_profile}（浏览器 {other_browser}）占用，且回滚扩展侧 label 失败（{err}）；请重跑 profile-label set/clear 修正"
                ),
            }));
        }
    }
    for session in driver.sessions.values_mut() {
        if session.profile_id == profile_id && session.browser_id == browser_id {
            session.profile_label = label.clone();
            session.info.profile_label = label.clone();
        }
    }
    Ok(json!({
        "status": "success",
        "profile_id": profile_id,
        "browser_id": browser_id,
        "profile_label": label,
    }))
}

/// 把 label 写给扩展（`setProfileLabel`）并等它确认。
///
/// 抽出来是因为「应用」与「冲突回滚」要发同一类命令：回滚复用同一套
/// 注册/失败回滚逻辑，否则异常路径又会漏掉 `execs` 条目、让条目等到超时。
async fn send_profile_label_to_extension(
    state: &AppState,
    session_key: &str,
    tab_id: &str,
    sender: &mpsc::Sender<String>,
    label: Option<&str>,
) -> Result<()> {
    let exec_id = Uuid::new_v4().to_string();
    let payload = json!({
        "id": exec_id,
        "code": json!({ "cmd": "setProfileLabel", "label": label }).to_string(),
        "tabId": require_tab_id(tab_id)?
    })
    .to_string();
    let (tx, rx) = oneshot::channel();
    // 顺序必须是「注册 → send → send 失败时 take 回滚」：
    // send 之后扩展可能在下一个事件循环就回包，而回包由另一条任务处理；
    // 若那时还没注册，`take` 取不到，结果直接丢，这条命令只能等到超时。
    // 旧写法把 `?` 挂在 send 上，失败时登记留在表里，tx 永不发结果、条目永不清理。
    state.execs.lock().await.register(
        &exec_id,
        session_key,
        crate::protocol::PendingExec {
            delivered_at: None,
            tx,
        },
    );
    // 队列满 / 连接已断都要翻译成结构化错误（`ws_backpressure` /
    // `ws_disconnected`），而不是像旧实现那样只回一句「连接已断开」。
    if let Err(err) = send_downstream(sender, payload).await {
        state.execs.lock().await.take(&exec_id);
        return Err(err);
    }
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(_)) => Err(anyhow!("执行结果通道已关闭")),
        Err(_) => {
            state.execs.lock().await.take(&exec_id);
            Err(anyhow!("设置 profile label 超时"))
        }
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    fn sample_tab(url: &str, title: &str) -> TabInfo {
        TabInfo {
            id: "id".to_string(),
            window_id: Some(1),
            tab_id: "7".to_string(),
            browser_id: "browser".to_string(),
            profile_id: "profile".to_string(),
            profile_label: None,
            browser_label: None,
            session_key: "browser:profile:7".to_string(),
            url: url.to_string(),
            title: title.to_string(),
            tab_type: "ext_ws".to_string(),
            group_title: None,
            active: false,
            connected_at: None,
        }
    }

    // TA：`parse_tab_id("")` 的契约是「空串 = 会话已失效」，返回 `Err`。
    /// 这条断言把历史上「注释说 Ok(0)、实现返回 Err」的分歧固化下来：
    /// 只允许在更上层（`exec` 等不需要 tabId 的路径）跳过调用，不允许返回值骗人。
    #[test]
    fn empty_tab_id_is_an_error_not_zero() {
        assert!(parse_tab_id("").is_err());
        assert!(parse_tab_id("   ").is_err());
        assert_eq!(parse_tab_id("12").expect("12"), 12);
        assert!(parse_tab_id("abc").is_err());
        // 关闭类入口必须给出 typed 的 session_gone。
        let err = require_tab_id("").expect_err("empty tab id must fail");
        let typed = err
            .downcast_ref::<SessionSelectError>()
            .expect("require_tab_id 应给 typed 错误");
        assert_eq!(typed.code, "session_gone");
    }

    // url / title 子串过滤大小写不敏感；`filter` 命中任一即可。
    #[test]
    fn tabs_filters_are_case_insensitive() {
        let tab = sample_tab("https://Example.COM/Docs", "Atb Docs");
        assert!(tab_matches_filters(&tab, Some("example.com"), None, None));
        assert!(tab_matches_filters(&tab, Some("EXAMPLE"), None, None));
        assert!(tab_matches_filters(&tab, None, Some("atb"), None));
        assert!(tab_matches_filters(&tab, None, None, Some("docs")));
        // filter 是 URL 或标题任一命中。
        assert!(tab_matches_filters(&tab, None, None, Some("atb docs")));
        assert!(!tab_matches_filters(&tab, None, None, Some("missing")));
        // 空子串等同不过滤。
        assert!(tab_matches_filters(&tab, Some(""), Some(""), Some("")));
    }

    /// 三个条件同时给时是 AND：任一不满足就过滤掉。
    #[test]
    fn tabs_filters_combine_with_and() {
        let tab = sample_tab("https://example.com/docs", "Documentation");
        assert!(tab_matches_filters(
            &tab,
            Some("example"),
            Some("doc"),
            Some("docs")
        ));
        assert!(!tab_matches_filters(
            &tab,
            Some("example"),
            Some("nomatch"),
            None
        ));
        assert!(!tab_matches_filters(
            &tab,
            Some("nomatch"),
            Some("doc"),
            None
        ));
    }

    /// 大列表提示只在超过阈值时出现；阈值本身来自 settings 访问器。
    #[test]
    fn tabs_large_suggest_only_over_threshold() {
        let threshold = crate::settings::tabs_large_warning();
        assert!(tabs_large_suggest(threshold).is_none());
        assert_eq!(tabs_large_suggest(threshold + 1), Some(TABS_LARGE_SUGGEST));
    }

    /// GET Query 用 snake_case；camelCase 作为 alias 继续兼容，flatten 不吞字段。
    #[test]
    fn tabs_query_accepts_snake_case_and_camel_alias() {
        let snake: TabsQueryExt = serde_json::from_value(json!({
            "url_contains": "example",
            "title_contains": "docs",
            "filter": "atb",
            "mine": true,
            "browser": "Chrome",
            "full": true,
        }))
        .expect("snake_case query 应可反序列化");
        assert_eq!(snake.url_contains.as_deref(), Some("example"));
        assert_eq!(snake.title_contains.as_deref(), Some("docs"));
        assert_eq!(snake.filter.as_deref(), Some("atb"));
        assert!(snake.mine);
        assert_eq!(snake.inner.browser.as_deref(), Some("Chrome"));
        assert!(snake.inner.full);

        let camel: TabsQueryExt = serde_json::from_value(json!({
            "urlContains": "example",
            "titleContains": "docs",
        }))
        .expect("camelCase alias 应可反序列化");
        assert_eq!(camel.url_contains.as_deref(), Some("example"));
        assert_eq!(camel.title_contains.as_deref(), Some("docs"));
        assert!(camel.filter.is_none());
    }
}
