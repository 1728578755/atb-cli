use super::*;

pub(crate) fn epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionSelector {
    pub(crate) tab_id: Option<String>,
    pub(crate) browser: Option<String>,
    pub(crate) profile: Option<String>,
}

impl SessionSelector {
    pub(crate) fn new(
        tab_id: Option<String>,
        browser: Option<String>,
        profile: Option<String>,
    ) -> Self {
        Self {
            tab_id,
            browser,
            profile,
        }
    }
}

/// daemon 单例锁：进程活着就持有，退出自动释放。
///
/// 为什么必须要有：Windows 上 SO_REUSEADDR 允许两个进程同时 bind 18767，
/// 一旦发生，CLI 的请求会被随机分到其中一个实例；没有扩展连接的那个会返回
/// 「没有可操作标签页」，而另一个明明正常——排查起来像是随机故障。
/// 用独立锁文件（不是 CLI 启动流程用的 daemon.lock）避免与启动锁互相等待。
pub(crate) fn daemon_singleton_lock() -> Result<Option<std::fs::File>> {
    let dir = config::user_config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon.instance.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        // 只有「锁已被别的实例持有」才是"已有 daemon"。
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        // 其余 IO 错误（网络盘 / 权限不足 / 文件系统不支持锁）不能当成"已有实例"
        // 然后 exit 0：那种情况下 daemon 根本没 bind 端口，CLI 只能等 /ping 超时。
        // 如实报错并让进程以非 0 退出，用户才看得到真实原因。
        Err(err) => Err(anyhow!(
            "daemon 单例锁加锁失败（{}）：{err}；常见原因是配置目录在网络盘、权限不足或文件系统不支持文件锁",
            path.display()
        )),
    }
}

pub async fn run_daemon() -> Result<()> {
    // 已有实例在服务时直接退出：不 bind、不抢端口、不触发扩展重连。
    let _singleton = match daemon_singleton_lock()? {
        Some(lock) => lock,
        None => {
            println!("another atb daemon is already running; exiting");
            return Ok(());
        }
    };
    let extension_port = config::load_or_create()?.extension_port;
    let addr: SocketAddr = format!("{HOST}:{API_PORT}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let api_token = Uuid::new_v4().simple().to_string();
    config::save_api_token(&api_token)?;
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
    let state = AppState {
        driver: Arc::new(RwLock::new(DriverState::default())),
        execs: Arc::new(Mutex::new(crate::protocol::ExecRegistry::default())),
        jobs: Arc::new(Mutex::new(JobStore::default())),
        session_locks: Arc::new(Mutex::new(HashMap::new())),
        page_scale: Arc::new(Mutex::new(HashMap::new())),
        ws_bind_error: Arc::new(Mutex::new(None)),
        ext_reload_sent: Arc::new(Mutex::new(HashSet::new())),
        profile_label_lock: Arc::new(Mutex::new(())),
        net_persist_lock: Arc::new(Mutex::new(HashMap::new())),
        started_at: SystemTime::now(),
        last_activity: Arc::new(Mutex::new(Instant::now())),
        inflight: Arc::new(AtomicUsize::new(0)),
        inflight_since: Arc::new(Mutex::new(None)),
        idle_shutdown_ttl: idle_shutdown_ttl_from_env(),
        ws_security_warned: Arc::new(AtomicBool::new(false)),
        ws_connections: Arc::new(std::sync::Mutex::new(HashMap::new())),
        ws_conn_seq: Arc::new(AtomicUsize::new(0)),
        shutdown: shutdown_tx,
        sessions_ready: Arc::new(Notify::new()),
        extension_port,
        api_token,
        agent_defaults: Arc::new(Mutex::new(HashMap::new())),
        agent_opened: Arc::new(Mutex::new(load_agent_opened())),
        ext_restart_seq: Arc::new(Mutex::new(HashMap::new())),
        cleanup_done: Arc::new(AtomicBool::new(false)),
        ext_warnings: Arc::new(Mutex::new(Vec::new())),
        // 幂等回放缓存与 迟到结果暂存：新字段所有构造点都必须补齐。
        idempotency: Arc::new(Mutex::new(HashMap::new())),
        late_results: Arc::new(Mutex::new(HashMap::new())),
    };

    // 读回上次落盘的 net 规则：daemon 重启后 persist 注入仍在页面/扩展侧，
    // 若不恢复内存配置，`net routes` 会读到空，表现为与实际拦截行为不一致。
    // 这里不做 fail-closed（net 规则不是安全配置），解析失败只提示并忽略。
    match config::load_net_hooks() {
        Ok(map) if !map.is_empty() => {
            state.driver.write().await.net_hooks = map;
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("net: 读取 net-hooks.json 失败，忽略持久化规则（不影响启动）: {err}");
        }
    }

    // 幂等缓存跨进程恢复。daemon 重启（空闲退出 / 升级 / 崩溃）后，60s 内
    // 同 key 同指纹的请求仍能回放首次响应，否则「响应丢失后重发」会二次执行——
    // 正是幂等键要防的场景。只恢复 Done 条目；文件缺失/损坏/无权限都不阻断启动。
    load_idempotency_cache(&state).await;

    let ws_state = state.clone();
    let ws_bind_error = state.ws_bind_error.clone();
    tokio::spawn(async move {
        if let Err(err) = run_ws_server(ws_state).await {
            eprintln!("WebSocket 服务异常: {err:?}");
            // 不要只打日志：端口被占用时 API 仍然可用，若不记录状态，
            // doctor 会显示"端口已监听"（其实是别的进程），排查会非常困难。
            *ws_bind_error.lock().await = Some(err.to_string());
        }
    });

    let idle_state = state.clone();
    tokio::spawn(async move {
        monitor_idle_shutdown(idle_state).await;
    });

    // 会话锁表的周期 GC。`forget_session` 只在断开清理能走到时删锁，
    // 被 prune / refresh 重建的会话会留下孤儿锁；5 分钟扫一次足够（daemon 本身
    // 空闲 5 分钟就会退出，所以这个任务只在真的有持续流量时才有机会跑）。
    let gc_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SESSION_LOCKS_GC_INTERVAL);
        // interval 的第一次 tick 立即完成：先消费掉，避免 daemon 刚起来就扫一次空表。
        tick.tick().await;
        loop {
            tick.tick().await;
            let removed = gc_session_locks(&gc_state).await;
            if removed > 0 {
                eprintln!("session_locks: 周期 GC 回收 {removed} 把无人持有的会话锁");
            }
        }
    });

    let app = build_api_router(state.clone());

    println!("atb rust server listening on http://{addr}");
    let cleanup_state = state.clone();
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            cleanup_on_shutdown(&cleanup_state).await;
        })
        .await;
    // Keep the last token file after shutdown. The next daemon rotates it after binding the API
    // port, avoiding an exiting daemon racing with and deleting a newer daemon's token.
    serve_result?;
    Ok(())
}

pub(crate) fn build_api_router(state: AppState) -> Router {
    // /ping 不需要 token：CLI 用它判断 daemon 是否存活。
    // 之前复用 /health，token 文件过期时会被误判成「未启动」，进而重复拉起 daemon 并卡 15 秒。
    let public = Router::new().route("/ping", get(ping));
    let protected = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/reload-trust", post(reload_trust))
        .route("/tabs", get(tabs))
        // 2026-11-13：目标身份与前台切换。
        .route("/focus", post(focus))
        .route("/use", post(use_target))
        .route("/extensions", post(extensions))
        .route("/tabtree", get(tabtree))
        .route("/lookup/tab/:tab_id", get(lookup_tab))
        .route("/lookup/browser/:browser_id", get(lookup_browser))
        .route("/lookup/profile/:profile", get(lookup_profile))
        .route("/profile-label", post(set_profile_label))
        .route("/scan", post(scan))
        .route("/exec", post(exec))
        // 迟到结果查询端点，5 分钟内可凭 exec_id 取回超时命令的真实结果。
        .route("/exec-result/:exec_id", get(exec_result))
        .route("/read", post(read_page))
        .route("/inject", post(inject))
        .route("/frames", post(frames))
        .route("/downloads", post(downloads))
        .route("/net", post(net))
        .route("/browsers", get(browsers))
        .route("/launch", post(launch_browser))
        .route("/broadcast", post(broadcast))
        .route("/find", post(find_page))
        .route("/action", post(action))
        .route("/wait", post(wait_page))
        .route("/challenge", post(challenge_page))
        .route("/stealth", post(stealth_page))
        .route("/cookies", post(cookies))
        .route("/upload", post(upload))
        .route("/open", post(open_tab))
        .route("/close", post(close))
        .route("/cleanup", post(cleanup))
        .route("/snapshot", post(snapshot))
        .route("/click", post(click))
        .route("/fill", post(fill))
        .route("/send-keys", post(send_keys))
        .route("/mouse-click", post(mouse_click))
        .route("/screenshot", post(screenshot))
        .route("/save-pdf", post(save_pdf))
        .route("/network/start", post(network_start))
        .route("/network/list", post(network_list))
        .route("/network/detail", post(network_detail))
        .route("/network/clear", post(network_clear))
        .route("/network/stop", post(network_stop))
        .route("/console/start", post(console_start))
        .route("/console/list", post(console_list))
        .route("/console/clear", post(console_clear))
        .route("/console/stop", post(console_stop))
        .route("/job/submit", post(job_submit))
        .route("/job/get/:job_id", get(job_get))
        .route("/job/list", get(job_list))
        .route("/job/cancel/:job_id", post(job_cancel))
        .route("/run", post(run_steps))
        .route("/batch", post(batch_steps))
        .route("/multi", post(multi))
        .route("/shutdown", post(shutdown))
        // POST 幂等回放：**先加 = 内层**。
        // axum 的 `Router::layer` 是「后加的在外层」，所以请求链是
        // agent_context → track_inflight → require_api_access → idempotency_replay → 路由。
        // 旧实现先加 `require_api_access`、后加本中间件，实际链变成
        // track_inflight → idempotency_replay → require_api_access：未授权请求先占位/写缓存，
        // 401 被当成可回放结果以 200 返回，CLI 的「token 失效重发一次」因此永远失败，
        // 本地进程也能用任意 key 污染缓存并落盘。
        .layer(middleware::from_fn_with_state(
            state.clone(),
            idempotency_replay,
        ))
        // token 校验：位于幂等缓存之外，401/403 直接短路，既不会占位也不会进缓存。
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_access,
        ))
        // 最外层：401/403 也算「在途」（请求离开时计数一定归零），
        // 但只有非鉴权失败才续期 `last_activity`。
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_inflight,
        ));
    public
        .merge(protected)
        // 2026-11-13：把 `select_tab` 解析到的目标身份并入响应（resolved_target /
        // browser / profile / warnings）。它必须包在业务路由外层，才能看到
        // `select_tab` 在 handler 内写入的 task-local。
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enrich_target_info,
        ))
        // 带 State 的版本：身份头只有在请求**同时**持有有效 API token 时才被承认
        // （旧报告 #6），所以这里必须能拿到 `state.api_token`。
        .layer(middleware::from_fn_with_state(state.clone(), agent_context))
        .with_state(state)
}

/// `x-agent-browser-agent` 的合法形态：1..=80 个 `[A-Za-z0-9._:-]`。
///
// 与 CLI 侧清洗规则一致。**不合法不是"清洗"而是忽略**：
/// 清洗会把 `客服`、`运维` 都塌缩成空串再落到 `default`，两个不同智能体共享
/// 同一个默认标签页，`cleanup` 互相关对方页面，且看起来一切正常。
pub(crate) fn is_valid_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 80
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
}

/// 把调用方身份放进 task-local，供 `select_tab` 等深层函数读取。
///
/// 放在最外层：即使请求在鉴权阶段被拒，也不会影响它；没有该 header 的
/// 旧客户端一律视为 `default`，行为与升级前一致。
///
/// 安全边界（旧报告 #6）：以前这个 header 被完全信任，任何本地进程都能自称
/// 别的智能体，再用 `cleanup --agent X` 关掉对方的标签。现在的规则是
/// **把身份绑定到凭证持有者**：
/// 1. header 形态不合法 → 忽略并回落 `default`；
/// 2. 只有请求同时带上了有效的 API token（与 `require_api_access` 同一套校验，
///    含 Host / Origin 白名单与恒定时间比较）才承认这个 header。
///
/// 这**不是**密码学签名，也没有 per-agent 凭证：同一个用户下的进程仍可读取
/// `~/.atb/api-token` 后冒用任意 `agent` 名（Windows 上该文件的 ACL 尤其弱）。
/// 它挡住的是"顺手伪造一个 header"这类越权，不是同机同用户的恶意进程。
pub(crate) async fn agent_context(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let declared = request
        .headers()
        .get("x-agent-browser-agent")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let agent = match declared {
        None => "default".to_string(),
        Some(value) => {
            if !is_valid_agent_id(value) {
                warn_agent_header_ignored(&format!(
                    "取值 {value:?} 不是 1-80 个 [A-Za-z0-9._:-] 字符，按 default 处理"
                ));
                "default".to_string()
            } else if validate_api_access(request.headers(), &state.api_token).is_err() {
                warn_agent_header_ignored(
                    "请求未携带有效 API token，不能声明智能体身份，按 default 处理",
                );
                "default".to_string()
            } else {
                value.to_string()
            }
        }
    };
    AGENT_ID.scope(agent, next.run(request)).await
}

/// 身份头被忽略的提示每个进程只打一次：正常 CLI 每次都带 token，打不到这里；
/// 刷屏只会把真正的排障信息淹掉。
pub(crate) fn warn_agent_header_ignored(reason: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "已忽略 x-agent-browser-agent（{reason}）。智能体身份只在请求通过 API token 校验后生效；后续同类忽略不再提示"
        );
    }
}

/// 请求离开时必定执行的减计数守卫：即便 handler panic（栈展开会 drop）或被取消，
/// 也不会漏减；否则 `inflight` 泄漏会让 daemon 永远不进入空闲自杀。
pub(crate) struct InflightGuard {
    pub(crate) inflight: Arc<AtomicUsize>,
    pub(crate) since: Arc<Mutex<Option<Instant>>>,
}

impl InflightGuard {
    /// 计数 +1 与守卫创建必须是同一步：若先递增、再 `.await` 取锁，请求在取锁期间
    // 被取消，就只留下「计数 +1、守卫从未创建」的泄漏，daemon 再也不会空闲退出。
    ///
    /// A-02 / S1-03：旧实现只在「0→1」那一刻记录起点，于是 (a) 这条请求离场后
    /// 时间戳仍滞留，下一条长请求会在 `since` 已经很旧时被提前判死；(b) 0→1 之后、
    /// 写入起点之前还有个 await 窗口，期间 monitor 看到的是上一条请求留下的旧值。
    /// 现在起点改成「**每条**请求进入时由 `track_inflight` 刷新」，acquire 不再承担
    /// 记录起点的职责（也就不再需要返回 `first`）。
    pub(crate) fn acquire(state: &AppState) -> Self {
        state.inflight.fetch_add(1, Ordering::Relaxed);
        Self {
            inflight: state.inflight.clone(),
            since: state.inflight_since.clone(),
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.inflight.fetch_sub(1, Ordering::Relaxed) == 1 {
            // 回到 0：清空起点。Drop 里不能 await，用 try_lock 快速路径；
            // 抢不到锁（monitor 正在读）就留着旧值——此时 `inflight` 已经是 0，
            // monitor 只在 `inflight > 0` 时才看起点，所以不会误判；
            // 下一条请求进入时会立刻覆盖它。
            if let Ok(mut slot) = self.since.try_lock() {
                *slot = None;
            }
        }
    }
}

pub(crate) async fn track_inflight(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // A-02 / S1-03：起点语义固定为「最近一次有请求进来的时刻」，**每条**请求都刷新，
    // 不再只在 0→1 时写。由此：
    // - 多条持续重叠的健康请求会不断续期，不会因为「最早那条请求的陈旧时间戳」
    //   在卡死阈值后被误杀（A-02）；
    // - 反过来「`inflight > 0` 且这个时刻已经超过阈值」就意味着阈值时间内
    //   一条新请求都没进来——这才是真的挂了。
    //
    // 顺序很关键：**先刷新起点、再入计数**（S1-03）。写起点要 await 拿锁，若放在
    // 「计数 +1」之后，请求恰好在这段 await 里被取消，就会留下「计数说在途、起点却还是
    // 上一条请求」的窗口——monitor 拿旧时间戳一算就把它判成卡死。放在计数之前，
    // 被取消时只是白刷新一次时间戳（这条请求没计入 inflight，monitor 不会看它），
    // 窗口彻底消失。
    *state.inflight_since.lock().await = Some(Instant::now());
    // 计数 +1 与守卫创建必须是同一步：即使后续 await 期间请求被取消，
    // 守卫也已在栈上，Drop 会把计数减回去。
    let _guard = InflightGuard::acquire(&state);
    // 必须先把响应接住再收尾，不能 early return。
    let response = next.run(request).await;
    // 续期：请求「结束」时刷新活跃时间。请求进行中由 `inflight` 挡住空闲自杀，
    // 这里保证结束后空闲计时从头算，不会刚跑完长命令就立刻自退。
    //
    // 401/403 不是业务活动，而是「鉴权失败」。旧实现无条件续期，于是
    // 一个还拿着旧 token 的进程（daemon 重启轮换 token 后的 CLI、或本地扫描脚本）
    // 每次轮询都能把空闲计时清零，daemon 永不空闲退出，扩展长期挂着「已连接」。
    // 5xx 仍然续期：那是真的跑了业务逻辑并失败了，属于活动。
    let status = response.status();
    if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
        *state.last_activity.lock().await = Instant::now();
    }
    response
}

pub(crate) async fn ping(State(state): State<AppState>) -> Json<Value> {
    let ws_bind_error = state.ws_bind_error.lock().await.clone();
    Json(json!({
        "ok": true,
        "running": true,
        "pid": std::process::id(),
        "protocol": crate::protocol::PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "ws_bind_error": ws_bind_error
    }))
}

pub(crate) async fn require_api_access(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Err((status, code, message)) = validate_api_access(request.headers(), &state.api_token) {
        return (
            status,
            Json(json!({ "ok": false, "error": message, "error_code": code })),
        )
            .into_response();
    }
    next.run(request).await
}

/// 恒定时间字符串比较：只用于 token 校验。
///
/// 防的是本地进程通过响应时间逐字节猜 token 的时间侧信道，**不是**强 token
/// 的替代品：真正的防线仍是 128-bit 随机 token + 只监听 127.0.0.1。
/// 长度不同会提前返回——长度本身不是秘密（token 长度固定），这样还能保持 O(1)。
pub(crate) fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    // `black_box` 不在这里用：diff 已经参与最终比较，编译器无法把它折叠成
    // 「发现不同就早退」的短路径（没有数据依赖分支）。
    diff == 0
}

/// 校验 HTTP API 的访问凭证：`Host` 白名单 → `Origin` 白名单 → API token。
///
/// 顺序有讲究：前两项是「这条请求是不是发给本机 daemon 的」，最后一项才是身份。
/// 任意一项失败都返回结构化 `(status, error_code, message)`，由调用方拼 JSON。
///
// 只校验 `Origin` 挡不住 **DNS rebinding**——攻击者把 `evil.example` 解析到
/// 127.0.0.1 后，浏览器发出的 `Host` 就是攻击者域名（`Origin` 也会带，但 Host 是
/// 更底层的判据，且 `Origin` 可能缺失）。现在 `Host` 的**主机名**必须是
/// `127.0.0.1` / `localhost` / `[::1]` / `::1`（大小写不敏感）；带端口时只校验
/// 端口是合法 `u16`（不要求等于 [`API_PORT`]——测试与临时实例会 bind 随机端口，
/// 而端口并不改变 DNS rebinding 的攻击面）。
///
/// `Host` 缺失时**放行**并记一条 warning：HTTP/1.1 客户端与浏览器一定带 `Host`，
/// 裸 TCP 探测（本仓库集成测试）不带；把缺失当失败只会打挂本地工具，
/// 挡不住任何真实攻击。扩展的 WS 走 `build_ws_router`，不经过这里。
pub(crate) fn validate_api_access(
    headers: &HeaderMap,
    expected_token: &str,
) -> std::result::Result<(), (StatusCode, &'static str, &'static str)> {
    match headers.get(axum::http::header::HOST) {
        Some(host) if host.to_str().map(api_host_allowed).unwrap_or(false) => {}
        Some(_) => {
            return Err((
                StatusCode::FORBIDDEN,
                "host_not_allowed",
                "Only loopback hosts (127.0.0.1 / localhost / [::1]) may access the local API",
            ));
        }
        // 缺失：放行 + 只提示一次。
        None => warn_missing_host(),
    }
    if let Some(origin) = headers.get(ORIGIN) {
        let allowed = origin
            .to_str()
            .map(|value| matches!(value, LOOPBACK_API_ORIGIN | LOCALHOST_API_ORIGIN))
            .unwrap_or(false);
        if !allowed {
            return Err((
                StatusCode::FORBIDDEN,
                "origin_not_allowed",
                "Browser origins are not allowed to access the local API",
            ));
        }
    }
    let authorized = headers
        .get(config::API_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value, expected_token));
    if !authorized {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid_api_token",
            "A valid API token is required",
        ));
    }
    Ok(())
}

// `Host` 主机名是否属于回环。
///
/// 只比主机名：`127.0.0.1:18767`、`localhost`、`[::1]:18767`、`::1` 都通过；
/// `evil.example`（DNS rebinding 的典型形态）、`127.0.0.1:abc` 一律拒绝。
fn api_host_allowed(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let (host, port) = split_host_port(value);
    let loopback = matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "::1"
    );
    if !loopback {
        return false;
    }
    // 带端口时只做语法校验：端口本身不改变攻击面，而测试/临时实例会 bind 随机端口。
    match port {
        None => true,
        Some(port) => port.parse::<u16>().is_ok(),
    }
}

/// 拆 `host[:port]`，兼容 IPv6 方括号形式；裸 IPv6（`::1`）按「无端口」处理。
fn split_host_port(value: &str) -> (&str, Option<&str>) {
    if let Some(rest) = value.strip_prefix('[') {
        return match rest.find(']') {
            // +2 = 前缀 '[' 与被找回的 ']'
            Some(end) => {
                let host = &value[..end + 2];
                (host, rest[end + 1..].strip_prefix(':'))
            }
            None => (value, None),
        };
    }
    match value.rsplit_once(':') {
        // 只有「唯一一个冒号」才是 ip:port；`::1` 这种裸 IPv6 不拆。
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (value, None),
    }
}

// 缺少 `Host` 时只提示一次。
fn warn_missing_host() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "HTTP API: 请求缺少 Host 头，已放行（仅本机回环；浏览器与 DNS rebinding 一定带 Host）；后续同类不再提示"
        );
    }
}

pub(crate) fn build_ws_router(state: AppState) -> Router {
    Router::new().route("/", get(ws_handler)).with_state(state)
}

pub(crate) async fn run_ws_server(state: AppState) -> Result<()> {
    let extension_port = state.extension_port;
    let app = build_ws_router(state);
    let addr: SocketAddr = format!("{HOST}:{extension_port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("WebSocket server running on ws://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// 空闲自杀阈值：默认 300s，`ATB_IDLE_TTL=<秒>`（旧名 AGENT_BROWSER_CLI_IDLE_TTL）可覆盖（0 或非法值回落默认）。
pub(crate) fn idle_shutdown_ttl_from_env() -> Duration {
    crate::settings::idle_shutdown_ttl_sec()
        .map(Duration::from_secs)
        .unwrap_or(IDLE_SHUTDOWN_TTL)
}

/// 在途请求「卡死」判定的下限（N3-07 / A-02）。
///
/// `inflight > 0` 会阻止空闲自杀，这本身是对的（否则长命令被腰斩）；但如果某个请求
/// 永远挂着（`download-wait --timeout 3600` 遇上扩展失联），daemon 就再也不会退出，
/// 端口和浏览器里的“已连接”浮层一直留着。
///
/// 取 3900s：单条请求的超时硬上限是 `preflight::TIMEOUT_MAX_SECONDS`（3600s），
/// 再加 300s 收尾余量。低于它会把合法的 `download-wait --timeout 3600` 跑到一半就
/// 误杀（A-02）；旧值 1800s 比单条请求允许的最长耗时还短，
/// 那条「不误杀」的保证根本立不住。
///
/// 真正的卡死判定并不只看这个下限：起点在**每条**请求进入时刷新（见 `track_inflight`），
/// 所以持续重叠的健康请求永远判不出卡死，只有「阈值时间内一条新请求都没进来」
/// 才会触发。
pub(crate) const MAX_INFLIGHT_STALL: Duration = Duration::from_secs(3900);

/// 卡死阈值 = max(2×空闲 TTL, 3900s)。
///
/// - TTL 配小（默认 300s）时用 `MAX_INFLIGHT_STALL` 兜底，别把正常的长命令误判成卡死；
/// - TTL 配大（比如 2h）时跟着放大到 4h，保证「卡死」一定晚于「正常空闲」。
pub(crate) fn max_inflight_stall(state: &AppState) -> Duration {
    state
        .idle_shutdown_ttl
        .saturating_mul(2)
        .max(MAX_INFLIGHT_STALL)
}

// 扩展断开后的宽限期。
///
/// 扩展重连靠 chrome.alarms，其硬下限 30s（Chrome 120+ 会抬到 ≥30s），
/// 所以"扩展一断就按空闲计时"等于"一断就自退"。给它一个明显大于 alarms
/// 周期的窗口，短暂断线仍能自愈。
pub(crate) const EXTENSION_RECONNECT_GRACE: Duration = Duration::from_secs(60);

// 空闲监控的「单轮结论」。
///
/// 判定从循环体里抽出来，单测才能直接驱动**生产代码**跑一轮（构造 `AppState`
/// 就够了），不必等 5s 定时器、也不必等到 300s TTL。
pub(crate) enum IdleAction {
    KeepAlive,
    /// 由调用方统一 cleanup + `shutdown.send(())` + break；日志已在判定处打完。
    Exit,
}

// 跑一轮空闲判定。`disconnected_since` 由调用方持有，跨轮保存。
///
/// 顺序即优先级：
/// 1. 在途请求（`inflight > 0`）默认阻止退出——否则长命令被腰斩；但卡死超阈值要放行，
///    否则 `download-wait --timeout 3600` 遇上扩展失联会让 daemon 永不退出；
// 2. running job 同理；
/// 3. 真正空闲（`idle_for >= ttl`）时，只要还有扩展连着就不退（重连代价更大）；
/// 4. 扩展刚断开时先等一个宽限窗口，别把短暂断线当成「没有浏览器」。
pub(crate) async fn idle_tick(
    state: &AppState,
    disconnected_since: &mut Option<Instant>,
) -> IdleAction {
    // 这里的 `since` 是「最近一次有请求进来的时刻」（A-02）：持续重叠的健康请求会
    // 一直把它刷新到现在，所以不会被「最早那条请求的陈旧时间戳」误杀。
    if state.inflight.load(Ordering::Relaxed) > 0 {
        let since = *state.inflight_since.lock().await;
        let stalled_for = match since {
            // 起点丢失（极端竞态）时保守处理：不算卡死，宁可多等一轮。
            None => Duration::ZERO,
            Some(since) => since.elapsed(),
        };
        let stall_limit = max_inflight_stall(state);
        if stalled_for < stall_limit {
            return IdleAction::KeepAlive;
        }
        eprintln!(
            "在途请求已挂起 {}s（阈值 {}s），判定为卡死，daemon 直接退出（inflight={}）",
            stalled_for.as_secs(),
            stall_limit.as_secs(),
            state.inflight.load(Ordering::Relaxed)
        );
        // 不能落入下面的 idle 判定：其它请求会不断刷新 `last_activity`，
        // 那样只会每 5s 重打一条「允许退出」却永远不退出。
        return IdleAction::Exit;
    }
    // 后台异步任务运行期间不允许退出，否则用户提交的长时间任务会被腰斩。
    let (running_jobs, oldest_running) = running_jobs_snapshot(state).await;
    if running_jobs > 0 {
        if let Some(stalled_for) = oldest_running {
            let stall_limit = max_inflight_stall(state);
            if stalled_for >= stall_limit {
                eprintln!(
                    "后台 job 已运行 {}s（阈值 {}s，running={}），判定为卡死，daemon 直接退出",
                    stalled_for.as_secs(),
                    stall_limit.as_secs(),
                    running_jobs
                );
                return IdleAction::Exit;
            }
        }
        return IdleAction::KeepAlive;
    }
    let idle_for = state.last_activity.lock().await.elapsed();
    if idle_for < state.idle_shutdown_ttl {
        return IdleAction::KeepAlive;
    }
    // 只要还有扩展连着就不退。
    //
    // 扩展的重连靠 chrome.alarms，而 alarms 的硬下限是 30s（Chrome 120+ 会把
    // periodInMinutes 抬到 ≥30s），所以 daemon 一退，下一次 CLI 冷启动最多要等
    // 近一分钟才看得见标签页 —— 用户侧表现就是「第一条命令特别慢 / 说没有可用标签页」。
    // 空闲退出的初衷是别让扩展长期挂着「已连接」状态，但那个状态本身无害，
    // 真正的代价（重连抖动、@e 引用失联、挑战求解中断）大得多。
    let connected = {
        let driver = state.driver.read().await;
        driver.sessions.values().any(|session| session.is_active())
    };
    if connected {
        // 连接期间持续续期：旧实现只是 `continue`，`last_activity`
        // 停在最后一次业务请求上，于是扩展一断，下一个 5s tick 立刻自退，
        // 没有任何重连宽限；`/health.ttl_remaining` 也会长期显示 0（谎报"马上退出"）。
        *state.last_activity.lock().await = Instant::now();
        *disconnected_since = None;
        return IdleAction::KeepAlive;
    }
    // 刚断开：先等一个宽限窗口，别把短暂断线当成"没有浏览器"。
    let since = *disconnected_since.get_or_insert_with(Instant::now);
    if since.elapsed() < EXTENSION_RECONNECT_GRACE {
        return IdleAction::KeepAlive;
    }
    eprintln!(
        "atb daemon idle for {}s（扩展已断开 {}s）, shutting down",
        idle_for.as_secs(),
        since.elapsed().as_secs()
    );
    IdleAction::Exit
}

pub(crate) async fn monitor_idle_shutdown(state: AppState) {
    let mut disconnected_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(IDLE_SHUTDOWN_CHECK_INTERVAL).await;
        match idle_tick(&state, &mut disconnected_since).await {
            IdleAction::KeepAlive => continue,
            IdleAction::Exit => {
                cleanup_on_shutdown(&state).await;
                let _ = state.shutdown.send(());
                break;
            }
        }
    }
}

/// 运行中的 job 数，以及最老一条已运行多久（没有 running job 时返回 None）。
///
/// 用 `started_at.unwrap_or(created_at)`：job 从提交到真正开跑之间 status 已经是
/// Running，但 started_at 还没写，此时按 created_at 计不会把它误判成刚启动。
///
// **不能用 `secs()`**。`secs()` 走的是 `preflight::clamp_timeout_seconds`，
/// 那是给「命令超时参数」用的护栏，上限 3600s；在这里它会把已运行时长**钳到 3600s**，
/// 而卡死阈值 `max_inflight_stall` 是 ≥3900s，于是 `stalled_for >= stall_limit`
/// 永远不成立——job 卡死分支形同虚设，daemon 永不退出。
/// 这里要的是「多老了」这个事实，用不饱和的 `try_from_secs_f64`。
pub(crate) async fn running_jobs_snapshot(state: &AppState) -> (usize, Option<Duration>) {
    let store = state.jobs.lock().await;
    let now = epoch_secs();
    let mut count = 0usize;
    let mut oldest = f64::INFINITY;
    for job in store.jobs.values().filter(|job| job.status.is_running()) {
        count += 1;
        oldest = oldest.min(job.started_at.unwrap_or(job.created_at));
    }
    let age = (count > 0 && oldest.is_finite()).then(|| {
        // NaN / 无穷不可能出现（上面已判 finite），兜底成 0 也不会 panic。
        Duration::try_from_secs_f64((now - oldest).max(0.0)).unwrap_or(Duration::ZERO)
    });
    (count, age)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 主机名必须是回环；端口只校验语法；缺 Host 放行。
    #[test]
    fn host_header_must_resolve_to_loopback() {
        for ok in [
            "127.0.0.1:18767",
            "127.0.0.1:0",
            "127.0.0.1",
            "localhost:18767",
            " LOCALHOST ",
            "[::1]:18767",
            "[::1]",
            "::1",
        ] {
            assert!(api_host_allowed(ok), "{ok} 应被接受");
        }
        for bad in [
            "evil.example",
            "evil.example:18767",
            "localhost.evil.example:18767",
            "127.0.0.1:abc",
            "127.0.0.1:99999",
            "0.0.0.0:18767",
            "example.com",
            "",
        ] {
            assert!(!api_host_allowed(bad), "{bad} 应被拒绝");
        }
    }
}
