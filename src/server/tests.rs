use super::*;
use axum::http::HeaderValue;

// 测试用下行队列容量：给足空间，只有专门测背压的
/// 用例才会用小容量把队列填满。
const TEST_WS_QUEUE: usize = 64;

fn test_state() -> AppState {
    // 大多数用例不关心退出信号，丢弃接收端即可（`send` 会返回 Err，无人读）。
    test_state_with_shutdown().0
}

fn test_state_with_shutdown() -> (AppState, mpsc::UnboundedReceiver<()>) {
    let (shutdown, shutdown_rx) = mpsc::unbounded_channel();
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
        idle_shutdown_ttl: IDLE_SHUTDOWN_TTL,
        ws_security_warned: Arc::new(AtomicBool::new(false)),
        ws_connections: Arc::new(std::sync::Mutex::new(HashMap::new())),
        ws_conn_seq: Arc::new(AtomicUsize::new(0)),
        shutdown,
        sessions_ready: Arc::new(Notify::new()),
        extension_port: config::DEFAULT_EXTENSION_PORT,
        api_token: "test-token".to_string(),
        agent_defaults: Arc::new(Mutex::new(HashMap::new())),
        agent_opened: Arc::new(Mutex::new(HashMap::new())),
        ext_restart_seq: Arc::new(Mutex::new(HashMap::new())),
        cleanup_done: Arc::new(AtomicBool::new(false)),
        ext_warnings: Arc::new(Mutex::new(Vec::new())),
        // 新增状态字段：与生产构造点保持同步。
        idempotency: Arc::new(Mutex::new(HashMap::new())),
        late_results: Arc::new(Mutex::new(HashMap::new())),
    };
    (state, shutdown_rx)
}

fn insert_test_session(
    driver: &mut DriverState,
    browser_id: &str,
    profile_id: &str,
    tab_id: &str,
) -> mpsc::Receiver<String> {
    let session_key = crate::protocol::make_session_key(browser_id, profile_id, tab_id);
    let (sender, receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
    driver.sessions.insert(
        session_key.clone(),
        Session {
            session_key: session_key.clone(),
            tab_id: tab_id.to_string(),
            browser_id: browser_id.to_string(),
            profile_id: profile_id.to_string(),
            profile_label: None,
            extension_version: Some("2.1".to_string()),
            extension_protocol: crate::protocol::EXTENSION_PROTOCOL_VERSION,
            file_scheme_access: Some(true),
            info: TabInfo {
                id: tab_id.to_string(),
                window_id: None,
                tab_id: tab_id.to_string(),
                browser_id: browser_id.to_string(),
                profile_id: profile_id.to_string(),
                profile_label: None,
                browser_label: None,
                session_key,
                url: "https://example.com".to_string(),
                title: "test".to_string(),
                group_title: None,
                active: true,
                tab_type: "ext_ws".to_string(),
                connected_at: None,
            },
            sender,
            disconnected_at: None,
            conn_id: 0,
        },
    );
    receiver
}

#[test]
fn api_access_requires_token_and_rejects_foreign_origins() {
    let mut headers = HeaderMap::new();
    headers.insert(
        config::API_TOKEN_HEADER,
        HeaderValue::from_static("test-token"),
    );
    assert!(validate_api_access(&headers, "test-token").is_ok());

    headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
    let error = validate_api_access(&headers, "test-token").unwrap_err();
    assert_eq!(error.0, StatusCode::FORBIDDEN);
    assert_eq!(error.1, "origin_not_allowed");

    headers.insert(ORIGIN, HeaderValue::from_static(LOOPBACK_API_ORIGIN));
    assert!(validate_api_access(&headers, "test-token").is_ok());
    headers.remove(config::API_TOKEN_HEADER);
    let error = validate_api_access(&headers, "test-token").unwrap_err();
    assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    assert_eq!(error.1, "invalid_api_token");
}

#[test]
fn websocket_origin_requires_a_valid_chrome_extension_id() {
    let mut headers = HeaderMap::new();
    assert!(!is_allowed_extension_origin(&headers));

    headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
    assert!(!is_allowed_extension_origin(&headers));

    headers.insert(
        ORIGIN,
        HeaderValue::from_static("chrome-extension://abcdefghijklmnopabcdefghijklmnop"),
    );
    assert!(is_allowed_extension_origin(&headers));

    headers.insert(
        ORIGIN,
        HeaderValue::from_static("chrome-extension://abcdefghijklmnopabcdefghijklmnop.example.com"),
    );
    assert!(!is_allowed_extension_origin(&headers));
}

#[test]
fn ws_trust_enforces_identity_only_when_configured() {
    let empty = WsTrust::default();
    assert!(!empty.enforces_identity(), "未配置 = 兼容模式");
    assert_eq!(empty.token_value(), None);

    let blank_token = WsTrust {
        extension_ids: Vec::new(),
        token: Some("   ".to_string()),
    };
    assert!(!blank_token.enforces_identity(), "空白 token 等同于未配置");

    let full = WsTrust {
        extension_ids: vec!["abcdefghijklmnopabcdefghijklmnop".to_string()],
        token: Some(" secret ".to_string()),
    };
    assert!(full.enforces_identity());
    assert_eq!(full.token_value(), Some("secret"));
}

/// N3-02 / §4.4：坏 JSON 与**空内容**都必须变成 `Corrupt`，绝不能退化成「未配置」
/// （= 任意扩展可连）。
#[test]
fn parse_ws_trust_accepts_valid_json() {
    match parse_ws_trust(r#"{"extension_ids":["abc"],"token":"t"}"#) {
        WsTrustState::Ok(trust) => {
            assert_eq!(trust.extension_ids, vec!["abc".to_string()]);
            assert_eq!(trust.token_value(), Some("t"));
            assert!(trust.enforces_identity());
        }
        WsTrustState::Corrupt(err) => panic!("合法 JSON 不该判成损坏: {err}"),
    }
    // 空对象是合法的「什么都没配」。
    match parse_ws_trust("{}") {
        WsTrustState::Ok(trust) => assert!(!trust.enforces_identity()),
        WsTrustState::Corrupt(err) => panic!("空对象不该判成损坏: {err}"),
    }
}

/// §4.4：存在但为空 ≠ 未配置。清空文件以前会静默回到「任意扩展可连」，
/// 现在必须 fail-closed，并在文案里告诉用户"删文件才回兼容模式"。
#[test]
fn parse_ws_trust_treats_empty_as_corrupt() {
    for raw in ["", "   ", "\n\t\r\n"] {
        match parse_ws_trust(raw) {
            WsTrustState::Corrupt(err) => {
                assert!(
                    err.contains("删除该文件"),
                    "文案要指出回到兼容模式的做法，实际: {err}"
                );
            }
            WsTrustState::Ok(trust) => panic!(
                "空文件必须 fail-closed，不能退化成兼容模式（白名单 {} 条）",
                trust.extension_ids.len()
            ),
        }
    }
}

#[test]
fn parse_ws_trust_rejects_corrupt_json() {
    // 尾逗号：用户手改配置最常见的错误，以前会让整套白名单静默失效。
    for raw in [
        r#"{"extension_ids": ["abc",], "token": null}"#,
        r#"{"extension_ids": "not-an-array"}"#,
        "not json at all",
    ] {
        match parse_ws_trust(raw) {
            WsTrustState::Corrupt(err) => {
                assert!(!err.trim().is_empty(), "损坏时必须带得上错误信息: {raw}")
            }
            WsTrustState::Ok(trust) => panic!(
                "坏 JSON 必须 fail-closed，不能解析成兼容模式: {raw} -> {:?}",
                trust.extension_ids
            ),
        }
    }
}

/// N3-04：`/reload-trust` 的判定核心——只踢掉白名单外的人，兼容模式不踢。
#[test]
fn reload_trust_revokes_only_untrusted_extensions() {
    let trusted = WsTrustState::Ok(WsTrust {
        extension_ids: vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()],
        token: None,
    });
    assert!(
        !ws_trust_should_revoke(&trusted, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", None),
        "id 比对与握手一致：忽略大小写与空白"
    );
    assert!(ws_trust_should_revoke(
        &trusted,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        None
    ));

    // 兼容模式（白名单为空、无 token）不能踢人，否则清空白名单会瞬间断掉所有连接。
    // 连接上残留的旧指纹也不能让人被误踢：降级到兼容模式不该断谁。
    let compat = WsTrustState::Ok(WsTrust::default());
    assert!(!ws_trust_should_revoke(
        &compat,
        "cccccccccccccccccccccccccccccccc",
        None
    ));
    assert!(!ws_trust_should_revoke(
        &compat,
        "cccccccccccccccccccccccccccccccc",
        Some(token_fingerprint("old-secret"))
    ));

    // 配置损坏 = fail-closed：全部撤销，且原因排在白名单之前。
    let corrupt = WsTrustState::Corrupt("trailing comma".to_string());
    assert!(ws_trust_should_revoke(
        &corrupt,
        "dddddddddddddddddddddddddddddddd",
        Some(token_fingerprint("whatever"))
    ));

    // `reason` 的实际取值（CLI 与用户都靠它理解为什么被踢）。
    assert_eq!(
        ws_trust_revoke_reason(&corrupt, "dddddddddddddddddddddddddddddddd", None),
        Some("corrupt_config")
    );
    assert_eq!(
        ws_trust_revoke_reason(&trusted, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", None),
        Some("untrusted_id")
    );
    assert_eq!(
        ws_trust_revoke_reason(&trusted, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", None),
        None
    );
}

/// §4.1：token-only 配置（白名单空 + token 有值）时，指纹对不上的已建连接必须撤销。
///
/// 这是以前"加了 token 却锁不死"的缺口：两条断言分别覆盖"连接带的是别的 token"
/// 与"连接根本没带 token"（配置收紧前连上的）。
#[test]
fn reload_trust_revokes_token_mismatch_under_token_only_config() {
    let token_only = WsTrustState::Ok(WsTrust {
        extension_ids: Vec::new(),
        token: Some("secret".to_string()),
    });
    let id = "cccccccccccccccccccccccccccccccc";

    assert_eq!(
        ws_trust_revoke_reason(&token_only, id, Some(token_fingerprint("other"))),
        Some("token_mismatch"),
        "白名单放行不代表可信：token-only 配置下指纹不符要撤销"
    );
    assert_eq!(
        ws_trust_revoke_reason(&token_only, id, None),
        Some("token_mismatch"),
        "这条连接是在没配 token 时上来的，配置收紧后应当断开"
    );
}

/// §4.1 的反向断言：指纹匹配就不该踢——否则加 token 会把自己也踹下线。
#[test]
fn reload_trust_keeps_token_matching_connection() {
    let token_only = WsTrustState::Ok(WsTrust {
        extension_ids: Vec::new(),
        // 带空白：配置侧读过 `token_value()` 时会 trim，两侧必须对齐。
        token: Some(" secret ".to_string()),
    });
    let id = "cccccccccccccccccccccccccccccccc";

    assert_eq!(
        ws_trust_revoke_reason(&token_only, id, Some(token_fingerprint("secret"))),
        None,
        "同一 token 的连接不能被自己的配置踢掉"
    );
    // 相对断言：不写死哈希值，只确认指纹的相等语义（`DefaultHasher` 跨版本不保证）。
    assert_eq!(
        token_fingerprint("secret"),
        token_fingerprint("secret"),
        "同一字符串两次哈希必须相等"
    );
    assert_ne!(
        token_fingerprint("secret"),
        token_fingerprint("secret2"),
        "不同字符串不该撞上"
    );
}

/// 注销必须靠 Drop guard：这条测试守住「登记表会泄漏」这个最容易漏的点。
#[test]
fn ws_conn_guard_unregisters_on_drop() {
    let state = test_state();
    let revoked = Arc::new(AtomicBool::new(false));
    let (sender, _receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
    ws_connections_lock(&state).insert(
        7,
        WsConn {
            extension_id: "x".to_string(),
            token_fingerprint: None,
            revoked,
            session_registry: Arc::new(std::sync::Mutex::new(HashSet::new())),
            sender,
        },
    );
    assert_eq!(ws_connections_lock(&state).len(), 1);
    {
        let _guard = WsConnGuard {
            state: state.clone(),
            conn_id: 7,
        };
    }
    assert_eq!(
        ws_connections_lock(&state).len(),
        0,
        "Drop 必须注销连接，否则登记表只增不减"
    );
}

/// N3-06：challenge 的外层 `ok` 必须等于业务结论，且 `result` 内部结构不动。
#[test]
fn challenge_envelope_mirrors_inner_ok() {
    // 超时：内层 ok=false → 外层也必须 false，并带出 error_code。
    let timeout = json_result_with_inner_ok(Ok(json!({
        "ok": false,
        "cleared": false,
        "passed": false,
        "error_code": "challenge_timeout",
        "error": "challenge 在 30s 内未清除",
    })));
    assert_eq!(timeout["ok"], json!(false));
    assert_eq!(timeout["error_code"], json!("challenge_timeout"));
    assert_eq!(timeout["result"]["ok"], json!(false), "内层结构不能改");
    assert_eq!(timeout["result"]["cleared"], json!(false));
    assert_eq!(timeout["result"]["passed"], json!(false));

    // 成功：外层仍是 true。
    let cleared = json_result_with_inner_ok(Ok(json!({
        "ok": true,
        "cleared": true,
    })));
    assert_eq!(cleared["ok"], json!(true));
    assert_eq!(cleared["result"]["cleared"], json!(true));

    // status/detect 分支没有 ok 字段 = 识别本身成功，不能误判成失败。
    let status = json_result_with_inner_ok(Ok(json!({ "action": "status" })));
    assert_eq!(status["ok"], json!(true));
    assert_eq!(status["result"]["action"], json!("status"));

    // 真正的错误路径保持原样。
    let failed = json_result_with_inner_ok(Err(anyhow!("boom")));
    assert_eq!(failed["ok"], json!(false));
    assert!(failed.get("result").is_none());
}

/// N3-07：卡死阈值必须同时满足「不误伤正常长命令」与「晚于正常空闲」。
#[test]
fn inflight_stall_threshold_has_a_floor() {
    let mut state = test_state();
    state.idle_shutdown_ttl = IDLE_SHUTDOWN_TTL;
    assert_eq!(max_inflight_stall(&state), MAX_INFLIGHT_STALL);
    assert!(MAX_INFLIGHT_STALL >= Duration::from_secs(1800));
    // TTL 配得很大时跟着放大，保证「卡死」不会早于「正常空闲」。
    state.idle_shutdown_ttl = Duration::from_secs(7200);
    assert_eq!(max_inflight_stall(&state), Duration::from_secs(14400));
}

/// open 的挑战探测必须「正常页面零成本、可疑页面才付一次往返」。
#[test]
fn should_probe_challenge_only_for_suspicious_pages() {
    // 正常页面：标题能看出来，直接跳过。
    assert!(!should_probe_challenge(
        "https://example.com",
        "Example Domain"
    ));
    assert!(!should_probe_challenge(
        "https://a.com",
        "百度一下，你就知道"
    ));
    // CF 拦截页的稳定标题。
    assert!(should_probe_challenge("https://a.com", "Just a moment..."));
    assert!(should_probe_challenge(
        "https://a.com",
        "Attention Required! | Cloudflare"
    ));
    assert!(should_probe_challenge("https://a.com", "请稍候…"));
    // 标题为空：挑战页在响应头到达前就是空标题，值得探一次。
    assert!(should_probe_challenge("https://a.com", ""));
    assert!(should_probe_challenge("https://a.com", "   "));
    // 非 http(s) 一律不探：本地文件 / about:blank 不会挂挑战页。
    assert!(!should_probe_challenge(
        "file:///C:/a.html",
        "Just a moment..."
    ));
    assert!(!should_probe_challenge("about:blank", ""));
}

#[test]
fn handshake_query_token_is_parsed_and_decoded() {
    assert_eq!(
        query_param(Some("token=abc"), "token").as_deref(),
        Some("abc")
    );
    assert_eq!(
        query_param(Some("a=1&token=x%20y"), "token").as_deref(),
        Some("x y")
    );
    assert_eq!(query_param(Some("token="), "token").as_deref(), Some(""));
    assert_eq!(query_param(None, "token"), None);
    assert_eq!(query_param(Some("token=abc"), "other"), None);
    // query 语义里裸 `+` 不是空格；空格只能来自 `%20`。
    assert_eq!(
        query_param(Some("token=a+b"), "token").as_deref(),
        Some("a+b")
    );
    assert_eq!(
        query_param(Some("token=a%2Bb"), "token").as_deref(),
        Some("a+b")
    );
}

/// 两段式变更观察的「线上契约」：`tests/mock_extension.mjs` 靠
/// `__abcMonitor__` 与 `status: 'watching'` 这两处字面量识别脚本并回包，
// 改动这两个哨兵必须同步改 mock，否则 的守卫会红得莫名其妙。
#[test]
fn monitor_scripts_keep_their_wire_contract() {
    let start = monitor_start_script("monitor-1");
    assert!(
        start.contains("__abcMonitor__"),
        "mock 靠这个字面量识别脚本"
    );
    assert!(start.contains("status: 'watching'"), "成功后必须回这个状态");
    assert!(start.contains("status: 'unsupported'"), "装不上时的状态");
    assert!(start.contains("\"monitor-1\""), "观察令牌要真的注进脚本");

    let stop = monitor_stop_script("monitor-1");
    assert!(stop.contains("__abcMonitor__"));
    assert!(stop.contains("status: 'ok', watch: state"));
    assert!(stop.contains("status: 'navigated'"));
    assert!(stop.contains("status: 'superseded'"));
    assert!(stop.contains("window[KEY] = null"), "收数后要清掉槽位");
    assert!(stop.contains("\"monitor-1\""), "令牌不匹配时判 superseded");
}

/// ：`track_inflight` 必须是最外层（401 也计入在途、请求结束后
/// 计数归零），但 401/403 属于「鉴权失败」**不是业务活动**，不能续期
/// `last_activity`——否则拿着旧 token 的进程每次轮询都能把空闲计时清零。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_api_request_is_tracked_and_released() {
    let state = test_state();
    let probe = state.clone();
    if let Some(past) = Instant::now().checked_sub(Duration::from_secs(60)) {
        *state.last_activity.lock().await = past;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, build_api_router(state))
            .await
            .unwrap();
    });

    let status_line = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(
                format!("GET /tabs HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        buf.lines().next().unwrap_or_default().to_string()
    })
    .await
    .unwrap();

    assert!(
        status_line.contains("401"),
        "缺少 token 必须被拒绝，实际: {status_line}"
    );
    let mut inflight = usize::MAX;
    for _ in 0..100 {
        inflight = probe.inflight.load(Ordering::Relaxed);
        if inflight == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        inflight, 0,
        "请求结束后 inflight 必须归零，否则 daemon 永不自退"
    );
    assert!(
        probe.last_activity.lock().await.elapsed() >= Duration::from_secs(55),
        "401 是鉴权失败不是业务活动：不能续期 last_activity"
    );
    server.abort();
}

// 未授权请求必须被鉴权层短路，**既不能占位也不能写幂等缓存**。
///
/// 旧层序是 `idempotency_replay` 在 `require_api_access` 外层：无 token 的 POST
/// 先占位、401 被当成可回放结果缓存，随后带正确 token 的同 key 请求直接回放
/// 那个 401（CLI 的「token 失效重发一次」因此永远失败），本地进程还能用任意
/// key 污染缓存并落盘。
async fn core01_probe_handler(State(state): State<AppState>) -> Json<Value> {
    // 可观察副作用：只有 handler 真的执行过才会出现这个 key。
    state
        .late_results
        .lock()
        .await
        .insert("core01-probe".to_string(), (json!(1), Instant::now()));
    Json(json!({ "ok": true }))
}

/// 与 `build_api_router` 相同的三层顺序：幂等在内、鉴权居中、inflight 最外。
fn core01_probe_router(state: AppState) -> Router {
    Router::new()
        .route("/probe", post(core01_probe_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            idempotency_replay,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_access,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_inflight,
        ))
        .with_state(state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_post_does_not_pollute_idempotency_cache() {
    // 幂等中间件默认会把成功响应的元数据追加到真实 `~/.atb/idempotency.jsonl`；
    // 测试只关心内存表，关掉落盘避免污染用户目录。
    std::env::set_var("ATB_IDEMPOTENCY_PERSIST", "off");

    let state = test_state();
    let probe = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, core01_probe_router(state))
            .await
            .unwrap();
    });

    let send = move |with_token: bool| {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://{addr}/probe");
        let mut request = client
            .post(&url)
            .header("x-atb-idempotency-key", "core01-key")
            .header("content-type", "application/json")
            .body(r#"{"probe":1}"#);
        if with_token {
            request = request.header(config::API_TOKEN_HEADER, "test-token");
        }
        let response = request.send().unwrap();
        let status = response.status();
        let body = response.json::<Value>().unwrap_or(Value::Null);
        (status, body)
    };

    // 第一次：无 token → 401，且不占位、不执行、不写缓存。
    let (first_status, _) = tokio::task::spawn_blocking(move || send(false))
        .await
        .unwrap();
    assert_eq!(first_status, StatusCode::UNAUTHORIZED);
    assert!(
        probe.idempotency.lock().await.is_empty(),
        "401 不能占用幂等槽/写缓存"
    );
    assert!(
        !probe.late_results.lock().await.contains_key("core01-probe"),
        "401 不能执行 handler"
    );

    // 第二次：带正确 token、同一个 key 与 body → 必须真正执行（不是回放 401）。
    let (second_status, second_body) = tokio::task::spawn_blocking(move || send(true))
        .await
        .unwrap();
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(
        second_body["ok"],
        json!(true),
        "带 token 的同 key 请求必须真正执行，实际回包: {second_body}"
    );
    assert!(
        probe.late_results.lock().await.contains_key("core01-probe"),
        "handler 必须被执行一次"
    );
    assert_eq!(
        probe.idempotency.lock().await.len(),
        1,
        "只有成功响应才允许进入幂等缓存"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_router_rejects_missing_and_non_extension_origins() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, build_ws_router(test_state()))
            .await
            .unwrap();
    });

    let status_lines = tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader, Write};

        let handshake = |origin: Option<&str>| {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let origin_header = origin
                .map(|value| format!("Origin: {value}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "GET / HTTP/1.1\r\n\
                 Host: {addr}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 {origin_header}\r\n"
            );
            stream.write_all(request.as_bytes()).unwrap();
            stream.flush().unwrap();
            let mut status_line = String::new();
            BufReader::new(stream).read_line(&mut status_line).unwrap();
            status_line
        };

        (
            handshake(None),
            handshake(Some("https://evil.example")),
            handshake(Some("chrome-extension://abcdefghijklmnopabcdefghijklmnop")),
        )
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;

    assert!(status_lines.0.starts_with("HTTP/1.1 403 Forbidden"));
    assert!(status_lines.1.starts_with("HTTP/1.1 403 Forbidden"));
    assert!(status_lines
        .2
        .starts_with("HTTP/1.1 101 Switching Protocols"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_router_enforces_token_and_origin_checks() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, build_api_router(test_state()))
            .await
            .unwrap();
    });

    let responses = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://{addr}/health");
        let send = |token: Option<&str>, origin: Option<&str>| {
            let mut request = client.get(&url);
            if let Some(token) = token {
                request = request.header(config::API_TOKEN_HEADER, token);
            }
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let response = request.send().unwrap();
            let status = response.status();
            let body = response.json::<Value>().unwrap();
            (status, body)
        };
        (
            send(None, None),
            send(Some("wrong-token"), None),
            send(Some("test-token"), Some("https://evil.example")),
            send(Some("test-token"), None),
        )
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;

    assert_eq!(responses.0 .0, StatusCode::UNAUTHORIZED);
    assert_eq!(responses.0 .1["error_code"], "invalid_api_token");
    assert_eq!(responses.1 .0, StatusCode::UNAUTHORIZED);
    assert_eq!(responses.1 .1["error_code"], "invalid_api_token");
    assert_eq!(responses.2 .0, StatusCode::FORBIDDEN);
    assert_eq!(responses.2 .1["error_code"], "origin_not_allowed");
    assert_eq!(responses.3 .0, StatusCode::OK);
    assert_eq!(responses.3 .1["running"], true);
}

/// N3-04：`/reload-trust` 必须落在 API 鉴权层内，且回包报告检查了多少条连接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_trust_requires_token_and_reports_checked() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = test_state();
    // 预置一条已登记连接：用来验证 `checked` 真的在数登记表，而不是写死 0。
    ws_connections_lock(&state).insert(1, {
        let (sender, _receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
        WsConn {
            extension_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            token_fingerprint: None,
            revoked: Arc::new(AtomicBool::new(false)),
            session_registry: Arc::new(std::sync::Mutex::new(HashSet::new())),
            sender,
        }
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, build_api_router(state))
            .await
            .unwrap();
    });

    let (unauth, authed) = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://{addr}/reload-trust");
        let unauth = client.post(&url).send().unwrap().status();
        let response = client
            .post(&url)
            .header(config::API_TOKEN_HEADER, "test-token")
            .send()
            .unwrap();
        let status = response.status();
        let body = response.json::<Value>().unwrap();
        (unauth, (status, body))
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;

    assert_eq!(unauth, StatusCode::UNAUTHORIZED, "管理端点不能绕过 token");
    assert_eq!(authed.0, StatusCode::OK);
    // 回包形态固定：ok/checked 必须在；配置是否损坏取决于本机文件，不做断言。
    assert!(authed.1.get("ok").is_some(), "回包: {}", authed.1);
    assert_eq!(authed.1["checked"], json!(1), "回包: {}", authed.1);
}

#[tokio::test]
async fn open_result_uses_executor_scope_when_browser_tab_ids_overlap() {
    let state = test_state();
    {
        let mut driver = state.driver.write().await;
        let _receiver_a = insert_test_session(&mut driver, "browser-a", "profile-a", "7");
        let _receiver_b = insert_test_session(&mut driver, "browser-b", "profile-b", "42");
    }

    let result = normalize_open_result(
        &state,
        json!({
            "js_return": { "id": 42 },
            "tab_id": "7",
            "session_key": "browser-a:profile-a:7"
        }),
    )
    .await;
    assert_eq!(
        result.get("opened_session_key").and_then(Value::as_str),
        Some("browser-a:profile-a:42")
    );
}

#[tokio::test]
async fn open_result_refuses_ambiguous_tab_id_without_executor_scope() {
    let state = test_state();
    {
        let mut driver = state.driver.write().await;
        let _receiver_a = insert_test_session(&mut driver, "browser-a", "profile-a", "42");
        let _receiver_b = insert_test_session(&mut driver, "browser-b", "profile-b", "42");
    }

    let result = normalize_open_result(&state, json!({ "js_return": { "id": 42 } })).await;
    assert!(result.get("opened_session_key").is_some_and(Value::is_null));
}

#[tokio::test]
async fn page_execution_reports_the_session_that_received_the_command() {
    let state = test_state();
    let (mut receiver_a, _receiver_b) = {
        let mut driver = state.driver.write().await;
        let receiver_a = insert_test_session(&mut driver, "browser-a", "profile-a", "7");
        let receiver_b = insert_test_session(&mut driver, "browser-b", "profile-b", "8");
        driver.default_session_key = Some("browser-a:profile-a:7".to_string());
        (receiver_a, receiver_b)
    };

    let exec_state = state.clone();
    let execution = tokio::spawn(async move {
        execute_page_js(
            &exec_state,
            "return 1",
            SessionSelector::new(Some("browser-a:profile-a:7".to_string()), None, None),
            true,
            Duration::from_secs(15),
        )
        .await
    });
    let payload = tokio::time::timeout(Duration::from_secs(1), receiver_a.recv())
        .await
        .expect("command should be dispatched")
        .expect("browser-a receiver should remain connected");
    let exec_id = serde_json::from_str::<Value>(&payload)
        .unwrap()
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    state.driver.write().await.default_session_key = Some("browser-b:profile-b:8".to_string());
    let (sender, _receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
    handle_ws_message(
        WsIncoming::Result {
            id: exec_id,
            result: json!(1),
            new_tabs: None,
        },
        &state,
        sender,
        0,
        &std::sync::Mutex::new(HashSet::new()),
    )
    .await;

    let result = execution.await.unwrap().unwrap();
    assert_eq!(
        result.get("session_key").and_then(Value::as_str),
        Some("browser-a:profile-a:7")
    );
    assert_eq!(result.get("tab_id").and_then(Value::as_str), Some("7"));
}

#[test]
fn pending_default_session_does_not_match_another_browser_tab() {
    let mut driver = DriverState {
        preferred_default_session_key: Some("browser-a:profile-a:42".to_string()),
        // key 与时间戳必须同生共死，只有 key 的记忆现在视为无效。
        preferred_default_session_at: Some(epoch_secs()),
        ..DriverState::default()
    };
    assert!(!should_set_default_session(
        &driver,
        "browser-b:profile-b:42"
    ));
    assert!(should_set_default_session(
        &driver,
        "browser-a:profile-a:42"
    ));
    driver.default_session_key = Some("browser-a:profile-a:7".to_string());
    assert!(should_set_default_session(
        &driver,
        "browser-a:profile-a:42"
    ));
}

#[test]
fn request_target_accepts_compatible_fields() {
    assert_eq!(
        resolve_request_target(Some("./demo.html"), None).unwrap(),
        "./demo.html"
    );
    assert_eq!(
        resolve_request_target(None, Some("https://example.com")).unwrap(),
        "https://example.com"
    );
    assert_eq!(
        resolve_request_target(Some("same"), Some("same")).unwrap(),
        "same"
    );
}

#[test]
fn request_target_rejects_missing_and_conflicting_fields() {
    assert_eq!(
        resolve_request_target(None, None).unwrap_err().code,
        "missing_open_target"
    );
    assert_eq!(
        resolve_request_target(Some("a"), Some("b"))
            .unwrap_err()
            .code,
        "ambiguous_open_target"
    );
}

#[test]
fn extension_versions_compare_numerically() {
    assert!(version_at_least("2.1", "2.1"));
    assert!(version_at_least("2.10", "2.1"));
    assert!(version_at_least("2.1.1", "2.1"));
    assert!(!version_at_least("2.0", "2.1"));
    assert!(!version_at_least("unknown", "2.1"));
}

#[test]
fn extension_freshness_flags_outdated_extension() {
    let bundled = bundled_extension_version();
    assert!(version_at_least(bundled, "2.2"));

    let stale = extension_freshness(&[json!({ "extension_version": "0.0.1" })]);
    assert_eq!(stale.get("stale").and_then(Value::as_bool), Some(true));
    assert!(stale.get("hint").and_then(Value::as_str).is_some());

    let fresh = extension_freshness(&[json!({ "extension_version": bundled })]);
    assert_eq!(fresh.get("stale").and_then(Value::as_bool), Some(false));
    assert_eq!(fresh.get("hint"), Some(&Value::Null));
}

/// 在途执行表拆出来后，结果回传、断连、会话清理都必须只靠这张表完成；
/// 漏清会泄漏 oneshot 发送端，漏取会让命令等到超时。
#[test]
fn exec_registry_tracks_and_clears_entries() {
    use crate::protocol::{ExecRegistry, PendingExec};

    let mut registry = ExecRegistry::default();
    let (tx_a, _rx_a) = oneshot::channel();
    let (tx_b, _rx_b) = oneshot::channel();
    let (tx_c, _rx_c) = oneshot::channel();
    registry.register(
        "e1",
        "s1",
        PendingExec {
            delivered_at: None,
            tx: tx_a,
        },
    );
    registry.register(
        "e2",
        "s1",
        PendingExec {
            delivered_at: None,
            tx: tx_b,
        },
    );
    registry.register(
        "e3",
        "s2",
        PendingExec {
            delivered_at: None,
            tx: tx_c,
        },
    );
    assert_eq!(registry.pending.len(), 3);
    assert_eq!(registry.active.len(), 3);

    registry.acked.insert("e1".to_string());
    assert!(registry.take("e1").is_some());
    assert!(registry.acked.is_empty(), "take 必须顺手清掉 acked");
    assert!(!registry.active.contains_key("e1"));
    assert!(registry.take("e1").is_none(), "重复 take 不能 panic");

    let orphaned = registry.take_session("s1");
    assert_eq!(orphaned.len(), 1, "s1 只剩 e2");
    assert_eq!(registry.pending.len(), 1);

    registry.retain_sessions(&["s1".to_string()]);
    assert!(
        registry.pending.is_empty(),
        "s2 不在存活列表里，e3 应被清掉"
    );
    assert!(registry.active.is_empty());
}

/// C-05：淘汰必须按「完成时间升序」，而不是 HashMap 的迭代顺序。
/// 旧实现 `values().take(20)` 会随机删掉刚完成、正等 `job wait` 的新任务。
#[test]
fn job_store_evicts_oldest_finished_jobs() {
    fn record(id: &str, status: JobStatus, created_at: f64, finished_at: Option<f64>) -> JobRecord {
        JobRecord {
            id: id.to_string(),
            op: "exec".to_string(),
            status,
            created_at,
            started_at: Some(created_at),
            finished_at,
            elapsed_ms: 0,
            last_heartbeat_ms: 0,
            result: None,
            error: None,
            session_key: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    let mut store = JobStore::default();
    for index in 0..120u32 {
        let id = format!("job-{index:03}");
        store.jobs.insert(
            id.clone(),
            record(
                &id,
                JobStatus::Succeeded,
                f64::from(index),
                Some(f64::from(index)),
            ),
        );
    }
    store.evict_for_insert();
    assert_eq!(
        store.jobs.len(),
        100,
        "超过软上限时必须淘汰一批（120 - 20）"
    );
    for index in 0..20u32 {
        assert!(
            !store.jobs.contains_key(&format!("job-{index:03}")),
            "完成时间最旧的 job-{index:03} 必须先被淘汰"
        );
    }
    // 刚完成的最新任务必须留下：调用方可能正在 `job wait` 它。
    assert!(store.jobs.contains_key("job-119"));
}

/// C-05 的另一半：全是 Running 时表也必须有硬上限，且淘汰最旧的。
#[test]
fn job_store_stays_bounded_when_all_jobs_are_running() {
    let mut store = JobStore::default();
    for index in 0..JOB_STORE_HARD_LIMIT {
        let id = format!("run-{index:04}");
        store.jobs.insert(
            id.clone(),
            JobRecord {
                id: id.clone(),
                op: "wait".to_string(),
                status: JobStatus::Running,
                created_at: index as f64,
                started_at: Some(index as f64),
                finished_at: None,
                elapsed_ms: 0,
                last_heartbeat_ms: 0,
                result: None,
                error: None,
                session_key: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    store.evict_for_insert();
    assert_eq!(
        store.jobs.len(),
        JOB_STORE_HARD_LIMIT - 1,
        "硬上限触发时腾出一个位置给新任务"
    );
    assert!(
        !store.jobs.contains_key("run-0000"),
        "最旧的 running 先淘汰"
    );
    assert!(store.jobs.contains_key("run-0499"), "最新的 running 保留");
}

/// 极端嵌套的树（1000 层）不能让递归爆栈：三个递归函数都要在深度上限处停住，
/// 且结果仍然可序列化（能原样吐给 CLI）。
#[test]
fn deep_tree_recursion_is_bounded() {
    // 逐层手工包装：`json!` 每层都会把内层重新序列化一遍，1000 层会把
    // 构造阶段自己递归爆栈（那是宏的问题，不是被测函数的问题）。
    fn nested(depth: usize) -> Value {
        let mut node = json!({ "ref": "@e1", "role": "button" });
        for _ in 0..depth {
            let mut obj = serde_json::Map::new();
            obj.insert("role".to_string(), json!("generic"));
            obj.insert("children".to_string(), Value::Array(vec![node]));
            node = Value::Object(obj);
        }
        node
    }

    // 每次重新构造，不 clone 原始深树：`Value::clone()` 对 1000 层也是递归的，
    // 在 debug 构建下（帧很大）测试自己就会爆栈，测不到被测函数。
    let pruned = prune_tree_keep_refs(vec![nested(1000)]);
    let folded = fold_tree(vec![nested(1000)], 10_000);

    let refs: HashMap<String, ElementRef> = HashMap::new();
    let with_details = attach_details_to_tree(vec![nested(1000)], &refs);

    // 深度上限生效：三条递归路径都必须停住，而不是靠「序列化没报错」证明没崩。
    fn depth_of(value: &Value) -> usize {
        match value.get("children").and_then(Value::as_array) {
            Some(children) => 1 + children.iter().map(depth_of).max().unwrap_or(0),
            None => 1,
        }
    }
    fn node_count(nodes: &[Value]) -> usize {
        nodes
            .iter()
            .map(|node| {
                1 + node
                    .get("children")
                    .and_then(Value::as_array)
                    .map(|children| node_count(children))
                    .unwrap_or(0)
            })
            .sum()
    }

    let pruned_depth = pruned.iter().map(depth_of).max().unwrap_or(0);
    assert!(
        pruned_depth <= MAX_TREE_DEPTH + 1,
        "prune 必须截断到 {MAX_TREE_DEPTH} 层，实际 {pruned_depth}"
    );
    // 1000 层链上唯一的 ref 在深度上限之外：prune 会整支丢弃（不是 panic），
    // 所以这里断言的是「不超上限」而不是「一定有节点」。
    assert!(
        node_count(&pruned) <= MAX_TREE_DEPTH + 1,
        "prune 截断后节点数也必须受限，实际 {}",
        node_count(&pruned)
    );
    // 对照：ref 在深度上限以内时必须完整保留这条祖先链（10 层 + 叶子 = 11 个节点）。
    let shallow = prune_tree_keep_refs(vec![nested(10)]);
    assert_eq!(node_count(&shallow), 11, "上限内的 ref 不能被 prune 误删");

    // 节点预算：1001 个节点的深链给 50 个预算，必须恰好保留 50 个，
    // 且结果的深度不超过预算（不能出现「预算只租约、子树照旧」）。
    let limited = fold_tree(vec![nested(1000)], 50);
    assert_eq!(node_count(&limited), 50, "fold_tree 必须严格按预算截断");
    let limited_depth = limited.iter().map(depth_of).max().unwrap_or(0);
    assert!(
        limited_depth <= 50,
        "fold_tree 结果深度必须受节点预算约束，实际 {limited_depth}"
    );
    // 预算远大于节点数时，fold 仍受深度上限约束：1000 层链在 depth=256 处
    // 丢掉子树，留下 0..=256 共 257 个节点（不是 1001）。
    assert_eq!(node_count(&folded), MAX_TREE_DEPTH + 1);

    // attach_details 只控制「下钻」深度，不裁剪树本身：断言它不崩且树原样保留。
    assert_eq!(node_count(&with_details), 1001);
    assert!(serde_json::to_string(&with_details).is_ok());
}

#[test]
fn site_match_pattern_strips_query_and_keeps_regular_hosts() {
    assert_eq!(
        site_match_pattern("https://example.com?q=1").as_deref(),
        Some("*://example.com/*")
    );
    assert_eq!(
        site_match_pattern("https://example.com:8443/a?b=1#c").as_deref(),
        Some("*://example.com/*")
    );
    // 以前 `starts_with("chrom")` 会误杀这两个正常域名。
    assert_eq!(
        site_match_pattern("https://chromium.org/docs").as_deref(),
        Some("*://chromium.org/*")
    );
    assert_eq!(
        site_match_pattern("https://chrome.google.com/webstore").as_deref(),
        Some("*://chrome.google.com/*")
    );
    // IPv6 + 端口：match pattern 不能带端口，必须保留方括号。
    assert_eq!(
        site_match_pattern("http://[::1]:8080/x").as_deref(),
        Some("*://[::1]/*")
    );
    // 非 http(s) 一律 fail-closed。
    for url in [
        "chrome://newtab/",
        "chrome-extension://abc/options.html",
        "about:blank",
        "data:text/html,x",
        "blob:https://example.com/uuid",
        "file:///tmp/a.html",
    ] {
        assert_eq!(site_match_pattern(url), None, "{url}");
    }
}

#[test]
fn net_hook_name_is_profile_scoped_and_stable() {
    let a = net_hook_name("chrome:profile-a");
    let b = net_hook_name("chrome:profile-b");
    assert_ne!(a, b);
    assert_eq!(a, net_hook_name("chrome:profile-a"));
    assert!(a.starts_with("__net_hook_"));
    assert_eq!(a.len(), "__net_hook_".len() + 16);
    assert!(a["__net_hook_".len()..]
        .chars()
        .all(|c| c.is_ascii_hexdigit()));
    // 固定向量：跨构建/跨 Rust 版本必须不变，否则升级后算不出旧注入名。
    assert_eq!(a, "__net_hook_dfbead77ec260ece");
}

// `--browser Edge` 同时命中 `Edge` 与 `Edge-Beta` 时必须报歧义，
/// 而不是按 default → latest → connected_at 静默挑一个。
#[test]
fn browser_label_ambiguity_is_reported_not_guessed() {
    let mut driver = DriverState::default();
    insert_test_session(&mut driver, "browser-a", "profile-a", "1");
    insert_test_session(&mut driver, "browser-b", "profile-b", "2");
    let key_a = crate::protocol::make_session_key("browser-a", "profile-a", "1");
    let key_b = crate::protocol::make_session_key("browser-b", "profile-b", "2");
    driver.sessions.get_mut(&key_a).unwrap().info.browser_label = Some("Edge".to_string());
    driver.sessions.get_mut(&key_b).unwrap().info.browser_label = Some("Edge-Beta".to_string());

    let selector = SessionSelector::new(None, Some("Edge".to_string()), None);
    let err = resolve_session(&driver, &selector, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("歧义"), "{err}");
    assert!(
        err.contains("Edge-Beta"),
        "候选里必须能看见另一个浏览器: {err}"
    );

    // 更精确的标签仍然可选：歧义判定不能把正常用法一起挡掉。
    let selector = SessionSelector::new(None, Some("Edge-Beta".to_string()), None);
    assert_eq!(resolve_session(&driver, &selector, None).unwrap().0, key_b);
}

/// 旧报告 #6：身份头只接受 `[A-Za-z0-9._:-]{1,80}`，其余一律回落 default。
#[test]
fn agent_identity_header_shape_is_validated() {
    assert!(is_valid_agent_id("main-agent"));
    assert!(is_valid_agent_id("team:reviewer.v2_1"));
    assert!(!is_valid_agent_id(""));
    assert!(!is_valid_agent_id("客服"));
    assert!(!is_valid_agent_id("agent one"));
    assert!(!is_valid_agent_id(&"a".repeat(81)));
}

// `ext_ready.restartSeq` 必须能被取出来；字段缺失/类型不对时当作没上报。
#[test]
fn restart_seq_is_parsed_from_ext_ready() {
    assert_eq!(
        parse_restart_seq(
            r#"{"type":"ext_ready","restartSeq":7,"browser_id":"b","profile_id":"p"}"#
        ),
        Some(("b".to_string(), "p".to_string(), 7))
    );
    // 缺 browser_id / profile_id 时用与反序列化同一套默认值。
    assert_eq!(
        parse_restart_seq(r#"{"type":"ext_ready","restartSeq":1}"#),
        Some((
            "browser-default".to_string(),
            "profile-default".to_string(),
            1
        ))
    );
    assert_eq!(parse_restart_seq(r#"{"type":"ext_ready"}"#), None);
    assert_eq!(
        parse_restart_seq(r#"{"type":"ext_ready","restartSeq":"3"}"#),
        None
    );
}

/// `multi_browser_hint` 是「只加字段」的能力，但判据必须精确：否则每条 read 都挂一句
/// 噪声提示，或者在真正的多浏览器场景下反而不提示。
#[test]
fn multi_browser_hint_requires_two_browsers_and_implicit_target() {
    let mut driver = DriverState::default();
    let key_a = crate::protocol::make_session_key("browser-a", "profile-1", "1");
    let _rx_a = insert_test_session(&mut driver, "browser-a", "profile-1", "1");

    let implicit = SessionSelector::new(None, None, None);
    assert!(
        multi_browser_hint(&driver, &key_a, &implicit).is_none(),
        "只有一个浏览器时不该提示"
    );

    let _rx_b = insert_test_session(&mut driver, "browser-b", "profile-2", "2");
    let hint = multi_browser_hint(&driver, &key_a, &implicit).expect("两个浏览器时必须提示");
    assert!(hint.contains("多个浏览器"), "提示文案应说明是多浏览器场景");

    // 显式指定目标时不需要提示：调用方已经知道自己在操作哪个。
    for selector in [
        SessionSelector::new(Some("1".to_string()), None, None),
        SessionSelector::new(None, Some("browser-a".to_string()), None),
        SessionSelector::new(None, None, Some("profile-1".to_string())),
    ] {
        assert!(
            multi_browser_hint(&driver, &key_a, &selector).is_none(),
            "显式目标不应附带隐式默认提示"
        );
    }
}

// ---------------------------------------------------------------------------
// / / / 补充用例
// ---------------------------------------------------------------------------

/// 构造一条指定下行通道的测试会话（`insert_test_session` 自己建通道，覆盖不到
/// "sender 相同 / 不同" 这两类清理判据）。
fn test_session_raw(
    session_key: &str,
    browser_id: &str,
    profile_id: &str,
    tab_id: &str,
    sender: mpsc::Sender<String>,
) -> Session {
    Session {
        session_key: session_key.to_string(),
        tab_id: tab_id.to_string(),
        browser_id: browser_id.to_string(),
        profile_id: profile_id.to_string(),
        profile_label: None,
        extension_version: Some("2.1".to_string()),
        extension_protocol: crate::protocol::EXTENSION_PROTOCOL_VERSION,
        file_scheme_access: Some(true),
        info: TabInfo {
            id: tab_id.to_string(),
            window_id: None,
            tab_id: tab_id.to_string(),
            browser_id: browser_id.to_string(),
            profile_id: profile_id.to_string(),
            profile_label: None,
            browser_label: None,
            session_key: session_key.to_string(),
            url: "https://example.com".to_string(),
            title: "test".to_string(),
            group_title: None,
            active: true,
            tab_type: "ext_ws".to_string(),
            connected_at: None,
        },
        sender,
        disconnected_at: None,
        conn_id: 0,
    }
}

fn registry_with(keys: &[String]) -> std::sync::Mutex<HashSet<String>> {
    let mut set = HashSet::new();
    for key in keys {
        set.insert(key.clone());
    }
    std::sync::Mutex::new(set)
}

// 会话已被新连接（conn 2）接管时，conn 1 送来的 Result 必须被丢弃，
/// 而且**不能**把 pending 取走——否则真正属于 conn 2 的结果永远没有接收者，
/// 调用方只能拿到泛化的 RecvError。
#[tokio::test]
async fn late_result_from_stale_generation_keeps_pending() {
    let state = test_state();
    let session_key = crate::protocol::make_session_key("browser-a", "profile-a", "1");
    {
        let mut driver = state.driver.write().await;
        let _rx = insert_test_session(&mut driver, "browser-a", "profile-a", "1");
    }
    // 会话归属连接 2（登记的会话表决定 `session_owner_conn` 的返回值）。
    let (owner_tx, _owner_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    let mut owned = HashSet::new();
    owned.insert(session_key.clone());
    ws_connections_lock(&state).insert(
        2,
        WsConn {
            extension_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            token_fingerprint: None,
            revoked: Arc::new(AtomicBool::new(false)),
            session_registry: Arc::new(std::sync::Mutex::new(owned)),
            sender: owner_tx,
        },
    );
    let (tx, mut rx) = oneshot::channel();
    {
        let mut execs = state.execs.lock().await;
        execs.register(
            "exec-stale",
            &session_key,
            crate::protocol::PendingExec {
                delivered_at: None,
                tx,
            },
        );
    }

    let (feed_tx, _feed_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    let scratch = std::sync::Mutex::new(HashSet::new());
    handle_ws_message(
        WsIncoming::Result {
            id: "exec-stale".to_string(),
            result: json!({ "ok": true }),
            new_tabs: None,
        },
        &state,
        feed_tx,
        1, // 旧代际
        &scratch,
    )
    .await;

    assert!(
        state.execs.lock().await.pending.contains_key("exec-stale"),
        "代际不符时必须保留 pending，等真正的回包或原超时收敛"
    );
    assert!(
        rx.try_recv().is_err(),
        "旧代际的结果不能投递给调用方的 oneshot"
    );
}

/// ：refresh 路径可能以 `conn_id = 0` 登记会话。旧判据
/// `conn_id != 0` 直接 continue，这类会话永远清不掉（`/health.ready` 谎报、
/// 空闲自杀判不出来）。现在必须按 sender 兜底。
#[tokio::test]
async fn disconnect_cleanup_falls_back_to_sender_for_zero_conn_id() {
    let state = test_state();
    let session_key = crate::protocol::make_session_key("browser-a", "profile-a", "1");
    let (sender, _receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
    {
        let mut driver = state.driver.write().await;
        let session = test_session_raw(&session_key, "browser-a", "profile-a", "1", sender.clone());
        driver.sessions.insert(session_key.clone(), session);
    }
    let (exec_tx, mut exec_rx) = oneshot::channel();
    {
        let mut execs = state.execs.lock().await;
        execs.register(
            "exec-on-zero",
            &session_key,
            crate::protocol::PendingExec {
                delivered_at: None,
                tx: exec_tx,
            },
        );
    }
    let registry = registry_with(std::slice::from_ref(&session_key));

    cleanup_conn_sessions(&state, 7, &sender, &registry).await;

    assert!(
        !state
            .driver
            .read()
            .await
            .sessions
            .contains_key(&session_key),
        "sender 同通道的 conn_id=0 会话必须被清理"
    );
    assert!(
        exec_rx.try_recv().is_ok(),
        "清理时必须让在途执行立即失败，而不是等 CLI 超时"
    );
    assert!(registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty());

    // 幂等：第二次调用不能 panic，也不能影响其他会话。
    cleanup_conn_sessions(&state, 7, &sender, &registry).await;
}

// 的反向断言：不同通道的 `conn_id = 0` 会话不能误伤
/// （sender 兜底必须真的比对通道，而不是「0 就清」）。
#[tokio::test]
async fn disconnect_cleanup_skips_zero_conn_id_other_sender() {
    let state = test_state();
    let session_key = crate::protocol::make_session_key("browser-a", "profile-a", "1");
    let (owner_sender, _owner_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    let (other_sender, _other_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    {
        let mut driver = state.driver.write().await;
        let session = test_session_raw(&session_key, "browser-a", "profile-a", "1", owner_sender);
        driver.sessions.insert(session_key.clone(), session);
    }
    let registry = registry_with(std::slice::from_ref(&session_key));

    cleanup_conn_sessions(&state, 9, &other_sender, &registry).await;

    assert!(
        state
            .driver
            .read()
            .await
            .sessions
            .contains_key(&session_key),
        "sender 不同的会话不属于这条连接，不能误清"
    );
}

/// ：关机清理必须 drain 在途 oneshot（给结构化错误）、
/// 把 running job 标成 daemon_shutdown 终态，且重复调用保持幂等。
#[tokio::test]
async fn shutdown_cleanup_drains_pending_and_aborts_running_jobs() {
    let state = test_state();
    let (tx, rx) = oneshot::channel();
    {
        let mut execs = state.execs.lock().await;
        execs.register(
            "exec-1",
            "browser-a:profile-a:1",
            crate::protocol::PendingExec {
                delivered_at: None,
                tx,
            },
        );
    }
    {
        let mut jobs = state.jobs.lock().await;
        jobs.jobs.insert(
            "job-1".to_string(),
            JobRecord {
                id: "job-1".to_string(),
                op: "exec".to_string(),
                status: JobStatus::Running,
                created_at: 0.0,
                started_at: None,
                finished_at: None,
                elapsed_ms: 0,
                last_heartbeat_ms: 0,
                result: None,
                error: None,
                session_key: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    let cleanup = cleanup_on_shutdown(&state).await;
    assert_eq!(cleanup["daemon"]["pending"], json!(1));

    let err = rx
        .await
        .expect("在途请求必须收到显式错误，而不是 RecvError")
        .expect_err("关机清理只能给 Err");
    assert!(
        err.to_string().contains("daemon_shutdown"),
        "错误必须带稳定的 daemon_shutdown 码，实际: {err}"
    );

    let jobs = state.jobs.lock().await;
    let job = jobs.jobs.get("job-1").expect("job 记录保留");
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.is_aborted_by_shutdown());
    drop(jobs);

    // 幂等：第二次 cleanup 直接跳过，不能重复发 debugClearAll / 重复报错。
    let again = cleanup_on_shutdown(&state).await;
    assert_eq!(again["skipped"], json!(true));
}

// 握手信任档位（纯函数）。默认 compat 必须保持旧行为，其余三档
/// fail-closed 且拒绝原因带稳定 error_code。
#[test]
fn ws_handshake_trust_modes_enforce_identity() {
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    let compat = WsTrust::default();
    let warning =
        ws_handshake_check(WsTrustMode::Compat, &compat, id, None).expect("compat 默认必须放行");
    assert!(warning.is_some(), "compat 必须返回安全告警");

    let whitelist = WsTrust {
        extension_ids: vec![id.to_string()],
        token: None,
    };
    assert!(ws_handshake_check(WsTrustMode::Whitelist, &whitelist, id, None).is_ok());
    let reject = ws_handshake_check(WsTrustMode::Whitelist, &whitelist, other, None)
        .expect_err("名单外的扩展必须拒绝");
    assert_eq!(reject.error_code, "ws_untrusted_extension");
    let reject = ws_handshake_check(WsTrustMode::Whitelist, &compat, id, None)
        .expect_err("档位要求白名单但没配置时必须 fail-closed");
    assert_eq!(reject.error_code, "ws_whitelist_not_configured");

    let token_only = WsTrust {
        extension_ids: Vec::new(),
        token: Some("s3cret".to_string()),
    };
    assert!(ws_handshake_check(WsTrustMode::Token, &token_only, id, Some("s3cret")).is_ok());
    assert_eq!(
        ws_handshake_check(WsTrustMode::Token, &token_only, id, Some("bad"))
            .expect_err("错误 token 必须拒绝")
            .error_code,
        "ws_token_invalid"
    );
    assert_eq!(
        ws_handshake_check(WsTrustMode::Token, &token_only, id, None)
            .expect_err("缺 token 必须拒绝")
            .error_code,
        "ws_token_invalid"
    );
    assert_eq!(
        ws_handshake_check(WsTrustMode::Token, &compat, id, None)
            .expect_err("档位要求 token 但没配置时必须 fail-closed")
            .error_code,
        "ws_token_not_configured"
    );

    let both = WsTrust {
        extension_ids: vec![id.to_string()],
        token: Some("s3cret".to_string()),
    };
    assert!(ws_handshake_check(WsTrustMode::Both, &both, id, Some("s3cret")).is_ok());
    assert!(ws_handshake_check(WsTrustMode::Both, &both, id, None).is_err());
    assert!(ws_handshake_check(WsTrustMode::Both, &token_only, id, Some("s3cret")).is_err());
}

// ---------------------------------------------------------------------------
// / / 补充用例
// ---------------------------------------------------------------------------

// 下行队列有界。满了必须给出 `ws_backpressure`（可重试），
/// 对端关闭必须给出 `ws_disconnected`——两者都不能是"静默丢弃"或"无限等待"。
#[tokio::test]
async fn send_downstream_reports_backpressure_and_disconnect() {
    let (sender, mut receiver) = mpsc::channel::<String>(1);
    assert!(
        send_downstream(&sender, "first".to_string()).await.is_ok(),
        "队列有空位时必须入队成功"
    );
    let err = send_downstream_within(&sender, "second".to_string(), Duration::from_millis(30))
        .await
        .expect_err("队列已满时必须报背压，而不是无限等待");
    let typed = err
        .downcast_ref::<SessionSelectError>()
        .expect("背压必须是结构化错误");
    assert_eq!(typed.code, "ws_backpressure");
    assert!(typed.retryable, "背压是暂时状态，必须可重试");

    // 消费一条后队列腾出位置，同一通道可以继续用。
    assert_eq!(receiver.recv().await.as_deref(), Some("first"));
    assert!(send_downstream(&sender, "third".to_string()).await.is_ok());

    drop(receiver);
    let err = send_downstream(&sender, "fourth".to_string())
        .await
        .expect_err("接收端已关闭时必须报断开");
    assert_eq!(
        err.downcast_ref::<SessionSelectError>()
            .expect("断开必须是结构化错误")
            .code,
        "ws_disconnected"
    );
}

// WS 入站 90s 静默判定的纯函数。定时分支与单测共用同一个判据，
/// 不会出现"测的和跑的两条线"。
#[test]
fn inbound_silence_turns_half_open_only_after_ninety_seconds() {
    assert_eq!(WS_INBOUND_SILENCE_LIMIT, Duration::from_secs(90));
    assert!(!inbound_is_half_open(Duration::ZERO));
    assert!(!inbound_is_half_open(Duration::from_secs(89)));
    assert!(
        !inbound_is_half_open(WS_INBOUND_SILENCE_LIMIT),
        "阈值当刻仍算健康（扩展可能正好卡在边界上）"
    );
    assert!(inbound_is_half_open(Duration::from_secs(91)));
}

// `ws_hello` 的 proof 必须能用"配置里的 token"复算出来，
/// 且没配 token 时 `auth_required:false` + `proof:null`。
#[test]
fn ws_hello_proves_token_possession() {
    let extension_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let hello = ws_hello_value(extension_id, Some("s3cret"));
    assert_eq!(hello["type"], json!("ws_hello"));
    assert_eq!(hello["v"], json!(1));
    assert_eq!(hello["auth_required"], json!(true));

    let nonce = hello["nonce"].as_str().expect("nonce 必须是字符串");
    assert_eq!(nonce.len(), 32, "nonce 必须是 32 位 hex");
    assert!(
        nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "nonce 必须是小写 hex，实际: {nonce}"
    );
    let expected = crate::hmac::hex_lower(&crate::hmac::hmac_sha256(
        b"s3cret",
        format!("{nonce}|{extension_id}").as_bytes(),
    ));
    assert_eq!(
        hello["proof"],
        json!(expected),
        "proof 必须是 HMAC-SHA256(token, nonce|extension_id) 的小写 hex"
    );

    // 每次握手换一个 nonce：固定挑战等于把 proof 变成可重放的静态口令。
    let again = ws_hello_value(extension_id, Some("s3cret"));
    assert_ne!(again["nonce"], hello["nonce"]);

    // 没配 token（含空白 token）：保持兼容路径，不要求扩展校验。
    for token in [None, Some("   ")] {
        let open = ws_hello_value(extension_id, token);
        assert_eq!(open["type"], json!("ws_hello"));
        assert_eq!(open["auth_required"], json!(false));
        assert!(
            open["proof"].is_null(),
            "auth_required=false 时 proof 必须是 null"
        );
    }
}

// 线协议固定向量：锁住 `nonce|extension_id` 的**拼接顺序与分隔符**。
///
/// 上面的 `ws_hello_proves_token_possession` 是自证（同一份 HMAC 算自己拼的串），
/// 改分隔符/换顺序也测不出来。这条用固定 nonce 断言一条与三方对拍过的完整 proof：
/// `src/hmac.rs` ↔ Node/OpenSSL `createHmac` ↔ 扩展 WebCrypto（`tests/closeout_ext.test.mjs`
/// 里固化的 known-answer 是同一条向量）。任一侧改了 hex 大小写、补零或拼接格式，
/// Rust 侧与 JS 侧会同时红，不必等真机。
#[test]
fn ws_hello_wire_vector_matches_the_extension_known_answer() {
    let hello = ws_hello_with_nonce(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some("s3cret"),
        "0123456789abcdef0123456789abcdef",
    );
    assert_eq!(hello["type"], json!("ws_hello"));
    assert_eq!(hello["v"], json!(1));
    assert_eq!(hello["auth_required"], json!(true));
    assert_eq!(
        hello["proof"],
        json!("74449fac6d934bb2f057e6804daa06b19a372d64d4c17ea7a85b37d05eb45958"),
        "proof 与扩展侧 WebCrypto / Node createHmac 必须逐字节一致，否则真机判 daemon_auth_failed"
    );
    let proof = hello["proof"].as_str().expect("proof 是字符串");
    assert_eq!(proof.len(), 64);
    assert!(
        proof
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "proof 必须是 64 位小写 hex"
    );
}

// 空闲退出判定的单轮驱动（直接用生产函数 `idle_tick`）。
///
/// 覆盖三条必须成立的语义：有扩展连接不退、有在途请求不退、都没有时走完
/// 宽限期才退。
#[tokio::test]
async fn idle_tick_keeps_running_until_nothing_is_connected() {
    let state = test_state();
    let mut disconnected_since = None;
    let ttl = state.idle_shutdown_ttl;
    // 把"最后一次业务请求"拨到 TTL 之前，模拟"已经空闲很久"。
    let expired_at = || {
        Instant::now()
            .checked_sub(ttl + Duration::from_secs(30))
            .expect("测试用的 TTL 足够小")
    };

    {
        let mut driver = state.driver.write().await;
        let _receiver = insert_test_session(&mut driver, "browser-a", "profile-a", "1");
    }
    *state.last_activity.lock().await = expired_at();
    assert!(
        matches!(
            idle_tick(&state, &mut disconnected_since).await,
            IdleAction::KeepAlive
        ),
        "还有活标签页时不能退出（重连代价远大于留着一个连接）"
    );
    assert!(
        state.last_activity.lock().await.elapsed() < Duration::from_secs(1),
        "连接期间必须续期 last_activity，否则 /health.ttl_remaining 会谎报"
    );

    // 在途请求（未达卡死阈值）同样阻止退出。
    {
        let mut driver = state.driver.write().await;
        for session in driver.sessions.values_mut() {
            session.disconnected_at = Some(Instant::now());
        }
    }
    *state.last_activity.lock().await = expired_at();
    state.inflight.store(1, Ordering::Relaxed);
    *state.inflight_since.lock().await = Some(Instant::now());
    assert!(
        matches!(
            idle_tick(&state, &mut disconnected_since).await,
            IdleAction::KeepAlive
        ),
        "有健康在途请求时必须继续等（否则长命令被腰斩）"
    );

    // 既没有连接也没有在途：第一轮只开始计宽限。
    state.inflight.store(0, Ordering::Relaxed);
    *state.last_activity.lock().await = expired_at();
    assert!(
        matches!(
            idle_tick(&state, &mut disconnected_since).await,
            IdleAction::KeepAlive
        ),
        "扩展刚断开要有宽限窗口，别把短暂断线当'没有浏览器'"
    );
    assert!(disconnected_since.is_some(), "宽限起点必须被记录");

    // 宽限期过去 → 允许退出。
    *state.last_activity.lock().await = expired_at();
    disconnected_since = Some(
        Instant::now()
            .checked_sub(EXTENSION_RECONNECT_GRACE + Duration::from_secs(1))
            .expect("宽限期足够小"),
    );
    assert!(matches!(
        idle_tick(&state, &mut disconnected_since).await,
        IdleAction::Exit
    ));
}

// `cleanup_conn_sessions` 重复调用必须幂等，且只收拾"本连接登记的"会话。
#[tokio::test]
async fn cleanup_conn_sessions_is_idempotent_and_owner_scoped() {
    let state = test_state();
    let owned_key = crate::protocol::make_session_key("browser-a", "profile-a", "1");
    let foreign_key = crate::protocol::make_session_key("browser-b", "profile-b", "2");
    let (owned_sender, _owned_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    let (foreign_sender, _foreign_rx) = mpsc::channel::<String>(TEST_WS_QUEUE);
    {
        let mut driver = state.driver.write().await;
        let mut owned = test_session_raw(
            &owned_key,
            "browser-a",
            "profile-a",
            "1",
            owned_sender.clone(),
        );
        owned.conn_id = 3;
        driver.sessions.insert(owned_key.clone(), owned);
        let mut foreign =
            test_session_raw(&foreign_key, "browser-b", "profile-b", "2", foreign_sender);
        foreign.conn_id = 4;
        driver.sessions.insert(foreign_key.clone(), foreign);
    }
    let registry = registry_with(&[owned_key.clone(), foreign_key.clone()]);

    cleanup_conn_sessions(&state, 3, &owned_sender, &registry).await;
    {
        let driver = state.driver.read().await;
        assert!(
            !driver.sessions.contains_key(&owned_key),
            "本连接登记的会话必须被清理"
        );
        assert!(
            driver.sessions.contains_key(&foreign_key),
            "别的连接的会话不能被误伤"
        );
    }
    assert!(
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "登记表随会话回收"
    );

    // 幂等：重复调用不 panic、不改变结果、不误清别的连接。
    cleanup_conn_sessions(&state, 3, &owned_sender, &registry).await;
    assert!(state
        .driver
        .read()
        .await
        .sessions
        .contains_key(&foreign_key));
}

// 连接已经收尾（登记表里没有它）之后才到的 tabs 快照，落库必须是
/// **已断开**，不能插成 active 的幽灵会话——否则 daemon 永不空闲退出。
#[tokio::test]
async fn snapshot_from_a_closed_connection_is_stored_as_disconnected() {
    let state = test_state();
    let (sender, _receiver) = mpsc::channel::<String>(TEST_WS_QUEUE);
    let tabs = |title: &'static str| -> Vec<crate::protocol::ExtTab> {
        serde_json::from_value(json!([
            { "id": 1, "url": "https://example.com", "title": title }
        ]))
        .expect("ext tabs")
    };

    // 1) 对照：连接仍在登记表里 → 正常活性。
    ws_connections_lock(&state).insert(
        42,
        WsConn {
            extension_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            token_fingerprint: None,
            revoked: Arc::new(AtomicBool::new(false)),
            session_registry: Arc::new(std::sync::Mutex::new(HashSet::new())),
            sender: sender.clone(),
        },
    );
    apply_tabs_snapshot(
        &state,
        &sender,
        42,
        "browser-a".to_string(),
        None,
        "profile-a".to_string(),
        None,
        None,
        crate::protocol::EXTENSION_PROTOCOL_VERSION,
        Some(true),
        tabs("alive"),
        &std::sync::Mutex::new(HashSet::new()),
    )
    .await;
    assert!(
        state
            .driver
            .read()
            .await
            .sessions
            .values()
            .all(|session| session.is_active()),
        "连接还在时快照必须落成 active"
    );

    // 2) 连接收尾之后快照才到（refresh 兜底的竞态）→ 必须按断开落库。
    ws_connections_lock(&state).remove(&42);
    let late_key = crate::protocol::make_session_key("browser-a", "profile-a", "2");
    apply_tabs_snapshot(
        &state,
        &sender,
        42,
        "browser-a".to_string(),
        None,
        "profile-a".to_string(),
        None,
        None,
        crate::protocol::EXTENSION_PROTOCOL_VERSION,
        Some(true),
        serde_json::from_value(json!([
            { "id": 2, "url": "https://late.example", "title": "late" }
        ]))
        .expect("ext tabs"),
        &std::sync::Mutex::new(HashSet::new()),
    )
    .await;
    let driver = state.driver.read().await;
    let late = driver.sessions.get(&late_key).expect("迟到的会话仍要落库");
    assert!(
        late.disconnected_at.is_some(),
        "连接已注销时快照晚到必须落成 disconnected（幽灵会话）"
    );
    assert!(
        driver
            .sessions
            .values()
            .filter(|session| session.browser_id == "browser-a"
                && session.profile_id == "profile-a"
                && session.tab_id == "2")
            .all(|session| !session.is_active()),
        "幽灵会话不能是可操作状态"
    );
}

// ---------------------------------------------------------------------------
// 幂等中间件：错误不缓存 / 并发同 key 只执行一次 / 占位随取消释放
// ---------------------------------------------------------------------------

static IDEMPOTENCY_FAIL_COUNT: AtomicUsize = AtomicUsize::new(0);

async fn idempotency_failing_probe() -> Response {
    IDEMPOTENCY_FAIL_COUNT.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error_code": "probe_failed" })),
    )
        .into_response()
}

/// ：失败响应不能进缓存，同 key 重发必须真的重跑
/// （否则幂等键会变成"永久失败锁"）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_does_not_cache_error_responses() {
    // 测试只关心内存表，关掉落盘避免污染用户目录。
    std::env::set_var("ATB_IDEMPOTENCY_PERSIST", "off");
    let state = test_state();
    let probe = state.clone();
    let router = Router::new()
        .route("/probe", post(idempotency_failing_probe))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            idempotency_replay,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    for _ in 0..2 {
        let status = tokio::task::spawn_blocking(move || {
            reqwest::blocking::Client::new()
                .post(format!("http://{addr}/probe"))
                .header("x-atb-idempotency-key", "fail-key")
                .header("content-type", "application/json")
                .body(r#"{"probe":1}"#)
                .send()
                .unwrap()
                .status()
        })
        .await
        .unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        IDEMPOTENCY_FAIL_COUNT.load(Ordering::SeqCst),
        2,
        "失败响应不能进缓存：同 key 重发必须真的再执行一次"
    );
    assert!(
        probe.idempotency.lock().await.is_empty(),
        "失败响应不能留下 Done 条目"
    );
    server.abort();
}

static IDEMPOTENCY_SLOW_COUNT: AtomicUsize = AtomicUsize::new(0);

async fn idempotency_slow_probe() -> Json<Value> {
    IDEMPOTENCY_SLOW_COUNT.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    Json(json!({ "ok": true }))
}

/// ：同一个 key 并发时，只有首个请求真正执行，第二个等它的
/// 完成通知后回放（不能"两个都执行一次"）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_concurrent_same_key_executes_once() {
    std::env::set_var("ATB_IDEMPOTENCY_PERSIST", "off");
    let state = test_state();
    let router = Router::new()
        .route("/probe", post(idempotency_slow_probe))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            idempotency_replay,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let call = move || {
        reqwest::blocking::Client::new()
            .post(format!("http://{addr}/probe"))
            .header("x-atb-idempotency-key", "slow-key")
            .header("content-type", "application/json")
            .body(r#"{"probe":1}"#)
            .send()
            .unwrap()
            .json::<Value>()
            .unwrap()
    };
    let first = tokio::task::spawn_blocking(call);
    // 让首个请求先进入 handler（拿到占位并开始执行）。
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = tokio::task::spawn_blocking(call);
    let (first, second) = (first.await.unwrap(), second.await.unwrap());

    assert_eq!(
        IDEMPOTENCY_SLOW_COUNT.load(Ordering::SeqCst),
        1,
        "同 key 并发只能执行一次"
    );
    assert_eq!(first["ok"], json!(true));
    assert_eq!(second["ok"], json!(true));
    assert!(
        second.get("idempotent_replay") == Some(&json!(true))
            || first.get("idempotent_replay") == Some(&json!(true)),
        "等待者必须拿到回放标记，否则调用方无法区分'刚执行'与'重复请求'"
    );
    server.abort();
}

/// /：首个请求被取消（客户端断开、task abort）时，
/// `Pending` 占位必须由 Drop 守卫释放，否则同 key 会被永久钉在 504 上。
#[tokio::test]
async fn idempotency_pending_slot_is_released_when_the_lead_is_cancelled() {
    let state = test_state();
    let cache_key = "default|guard-key".to_string();
    let scope = "default|POST|/probe||0000000000000000".to_string();

    let notify = match claim_idempotency_slot(
        &state,
        &cache_key,
        &scope,
        "guard-key",
        Duration::from_millis(500),
    )
    .await
    {
        SlotDecision::Lead(notify) => notify,
        _ => panic!("首个请求必须拿到占位"),
    };

    let task = tokio::spawn({
        let state = state.clone();
        let cache_key = cache_key.clone();
        let scope = scope.clone();
        async move {
            // 与中间件同构：拿到占位后挂上守卫，然后"永远不完成"。
            let _guard = IdempotencySlotGuard::new(state, cache_key, scope, notify);
            std::future::pending::<()>().await;
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        matches!(
            state
                .idempotency
                .lock()
                .await
                .get(&cache_key)
                .map(|slot| &slot.entry),
            Some(IdempotencyEntry::Pending(_))
        ),
        "执行期间槽位必须是 Pending"
    );

    task.abort();
    let _ = task.await;
    // Drop 里的清理是 spawn 出去的，轮询等它跑完（最多 500ms）。
    let mut released = false;
    for _ in 0..50 {
        if state.idempotency.lock().await.get(&cache_key).is_none() {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "首个请求被取消后占位必须释放，否则同 key 永久 504"
    );
    assert!(
        matches!(
            claim_idempotency_slot(
                &state,
                &cache_key,
                &scope,
                "guard-key",
                Duration::from_millis(500)
            )
            .await,
            SlotDecision::Lead(_)
        ),
        "释放之后同 key 必须能重新抢到占位"
    );
}
