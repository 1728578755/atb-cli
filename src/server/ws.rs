use super::*;

/// WebSocket 信任配置：`<用户配置目录>/ws-trust.json`。
///
/// 默认不存在 = 兼容模式（任何 Chrome 扩展都能连）。用户填了 `extension_ids`
/// 或 `token` 之后才真正收紧。每次握手都重新读文件，CLI 改完无需重启 daemon。
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WsTrust {
    #[serde(default)]
    pub(crate) extension_ids: Vec<String>,
    #[serde(default)]
    pub(crate) token: Option<String>,
}

impl WsTrust {
    pub(crate) fn token_value(&self) -> Option<&str> {
        self.token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    pub(crate) fn enforces_identity(&self) -> bool {
        !self.extension_ids.is_empty() || self.token_value().is_some()
    }
}

/// 白名单是否放行某个扩展 id。空白名单 = 兼容模式，一律放行。
/// 握手与 `/reload-trust` 共用这一处判断，避免两边对「大小写/空白」的处理跑偏。
pub(crate) fn ws_trust_allows(trust: &WsTrust, extension_id: &str) -> bool {
    trust.extension_ids.is_empty()
        || trust
            .extension_ids
            .iter()
            .any(|item| item.trim().eq_ignore_ascii_case(extension_id))
}

// WS 握手信任档位。
///
/// `Origin` 头是客户端自断言、可被本机任意进程伪造的，所以它只能当「像不像扩展」
/// 的第一道筛子，不能当身份证明。真正的档位由用户在配置里显式选择：
/// - `compat`（默认，向后兼容）：沿用旧行为，只校验 Origin 前缀 + 配置了才校验的
///   白名单/token；握手回包带 `x-atb-ws-security-warning` 头提示风险。
/// - `whitelist`：必须配置非空 `extension_ids`，且扩展 id 在名单内。
/// - `token`：必须配置 `ws-trust.json` 的 token，且握手 `?token=` 恒定时间匹配。
/// - `both`：白名单与 token 同时满足（多人共用机器的推荐档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WsTrustMode {
    Compat,
    Whitelist,
    Token,
    Both,
}

impl WsTrustMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WsTrustMode::Compat => "compat",
            WsTrustMode::Whitelist => "whitelist",
            WsTrustMode::Token => "token",
            WsTrustMode::Both => "both",
        }
    }
}

/// 解析档位；非法值回落到 `compat`（与 `settings::ws_trust_mode` 的白名单一致，
/// 这里再兜一层，保证直接从原始字符串构造时也不会 panic）。
pub(crate) fn parse_ws_trust_mode(raw: &str) -> WsTrustMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "whitelist" => WsTrustMode::Whitelist,
        "token" => WsTrustMode::Token,
        "both" => WsTrustMode::Both,
        _ => WsTrustMode::Compat,
    }
}

// 握手被拒绝时的结构化信息。`error_code` 稳定，供 CLI/ext 分支处理。
#[derive(Debug)]
pub(crate) struct WsHandshakeReject {
    pub(crate) status: StatusCode,
    pub(crate) error_code: &'static str,
    pub(crate) message: String,
    pub(crate) hint: &'static str,
}

/// 握手信任判定（纯函数，便于单测）。
///
/// 返回 `Ok(Some(warning))` 表示放行且应在回包里带安全告警（只有 compat 档），
/// `Ok(None)` 表示放行且无需告警。
pub(crate) fn ws_handshake_check(
    mode: WsTrustMode,
    trust: &WsTrust,
    extension_id: &str,
    provided_token: Option<&str>,
) -> Result<Option<&'static str>, WsHandshakeReject> {
    let token_matches = |expected: &str| {
        provided_token.is_some_and(|provided| constant_time_eq(provided, expected))
    };
    let untrusted = |extension_id: &str| {
        WsHandshakeReject {
        status: StatusCode::FORBIDDEN,
        error_code: "ws_untrusted_extension",
        message: format!("扩展未信任：{extension_id} 不在 ws-trust.json 的 extension_ids 内"),
        hint: "运行 atb trust-extension <扩展ID> 加入白名单，或把 ATB_WS_TRUST_MODE 改回 compat（不推荐）",
    }
    };
    let token_invalid = || {
        WsHandshakeReject {
        status: StatusCode::UNAUTHORIZED,
        error_code: "ws_token_invalid",
        message: "WebSocket token 无效".to_string(),
        hint: "在扩展里填写与 ws-trust.json 的 token 一致的值（atb trust-extension <扩展ID> --token <值>）",
    }
    };
    let whitelist_missing = || WsHandshakeReject {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_code: "ws_whitelist_not_configured",
        message: "ATB_WS_TRUST_MODE=whitelist/both，但 ws-trust.json 没有配置 extension_ids"
            .to_string(),
        hint: "运行 atb trust-extension <扩展ID> 写入白名单；或把 ATB_WS_TRUST_MODE 改回 compat",
    };
    let token_missing = || {
        WsHandshakeReject {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_code: "ws_token_not_configured",
        message: "ATB_WS_TRUST_MODE=token/both，但 ws-trust.json 没有配置 token".to_string(),
        hint: "运行 atb trust-extension <扩展ID> --token <值> 写入 token；或把 ATB_WS_TRUST_MODE 改回 compat",
    }
    };

    match mode {
        // 兼容档：完整保留旧行为（配置了白名单/token 才校验），只在成功时给出告警。
        WsTrustMode::Compat => {
            if !ws_trust_allows(trust, extension_id) {
                return Err(untrusted(extension_id));
            }
            if let Some(expected) = trust.token_value() {
                if !token_matches(expected) {
                    return Err(token_invalid());
                }
            }
            Ok(Some(
                "compat: origin header is forgeable; set ATB_WS_TRUST_MODE=token or run atb trust-extension",
            ))
        }
        WsTrustMode::Whitelist => {
            if trust.extension_ids.is_empty() {
                return Err(whitelist_missing());
            }
            if !ws_trust_allows(trust, extension_id) {
                return Err(untrusted(extension_id));
            }
            Ok(None)
        }
        WsTrustMode::Token => {
            let Some(expected) = trust.token_value() else {
                return Err(token_missing());
            };
            if !token_matches(expected) {
                return Err(token_invalid());
            }
            Ok(None)
        }
        WsTrustMode::Both => {
            if trust.extension_ids.is_empty() {
                return Err(whitelist_missing());
            }
            if !ws_trust_allows(trust, extension_id) {
                return Err(untrusted(extension_id));
            }
            let Some(expected) = trust.token_value() else {
                return Err(token_missing());
            };
            if !token_matches(expected) {
                return Err(token_invalid());
            }
            Ok(None)
        }
    }
}

/// `/reload-trust` 的判定核心：`Some(reason)` = 撤销，`None` = 保留。
///
/// 判定顺序固定为「损坏 → 白名单 → token」，理由如下：
/// 1. 配置损坏必须 fail-closed，先于任何"看起来合法"的分支，绝不能让损坏变成放行；
/// 2. 白名单是最粗的那道门，被移出名单的连接连比对 token 都不必做；
/// 3. 最后比 token：当前配置**有** token 时，连接的指纹必须等于当前 token 的指纹。
///    连接指纹为 `None` 也要撤销——它是在"还没配 token"时连上的，现在配置收紧了就该断。
///    （这是 §4.1：token-only 配置以前恒不踢人，等于"加了 token 也没锁死"。）
///
/// `reason` 取值：`corrupt_config` / `untrusted_id` / `token_mismatch`，仅用于回包与文案。
pub(crate) fn ws_trust_revoke_reason(
    trust: &WsTrustState,
    extension_id: &str,
    conn_token_fp: Option<u64>,
) -> Option<&'static str> {
    match trust {
        // 配置损坏 = fail-closed，与握手侧一致：撤销所有连接。
        WsTrustState::Corrupt(_) => Some("corrupt_config"),
        WsTrustState::Ok(trust) => {
            if !ws_trust_allows(trust, extension_id) {
                return Some("untrusted_id");
            }
            // 配置没 token（含从有改回无）时不复核指纹：那是回到兼容模式，
            // 已建立的老连接不该因为"少了把锁"被踢。
            if let Some(expected) = trust.token_value() {
                if conn_token_fp != Some(token_fingerprint(expected)) {
                    return Some("token_mismatch");
                }
            }
            None
        }
    }
}

pub(crate) fn ws_trust_should_revoke(
    trust: &WsTrustState,
    extension_id: &str,
    conn_token_fp: Option<u64>,
) -> bool {
    ws_trust_revoke_reason(trust, extension_id, conn_token_fp).is_some()
}

pub(crate) fn ws_trust_path() -> Option<PathBuf> {
    config::user_config_dir()
        .ok()
        .map(|dir| dir.join("ws-trust.json"))
}

/// `ws-trust.json` 的读取结果。
///
/// `Corrupt` 是 fail-closed 的落点：文件存在但读不懂时**不能**当成「从没配过」。
/// 否则用户在配置里多写一个尾逗号，整套白名单就会静默失效（daemon 在后台，
/// 那行 stderr 也没人看得见），等于「越写配置越不安全」。
#[derive(Debug)]
pub(crate) enum WsTrustState {
    /// 正常读到配置；`default()` 表示从没配过 = 兼容模式。
    Ok(WsTrust),
    /// 文件存在但读取/解析失败，细节用于报错文案。
    Corrupt(String),
}

/// 纯解析：只负责把 JSON 文本变成状态，便于单测覆盖「坏 JSON 不能退化成兼容模式」。
///
/// 空/纯空白同样算 `Corrupt`（§4.4）：写文件是原子 rename，正常不会出现空文件，
/// 一旦出现只可能是手工 `> ws-trust.json` 或编辑器保存中断。若把它当成"未配置"，
/// 用户会**静默**回到「任意扩展可连」——配置越写越不安全。回到兼容模式的正确做法
/// 是删除该文件（`NotFound` 那条分支），而不是清空内容。
pub(crate) fn parse_ws_trust(raw: &str) -> WsTrustState {
    if raw.trim().is_empty() {
        return WsTrustState::Corrupt(
            "配置为空：空文件不会被视为「未配置」，删除该文件才会回到兼容模式".to_string(),
        );
    }
    match serde_json::from_str::<WsTrust>(raw) {
        Ok(trust) => WsTrustState::Ok(trust),
        Err(err) => WsTrustState::Corrupt(err.to_string()),
    }
}

pub(crate) fn load_ws_trust() -> WsTrustState {
    let Some(path) = ws_trust_path() else {
        return WsTrustState::Ok(WsTrust::default());
    };
    // 用带备份回退的读取：Windows 的原子替换是「rename(target, .bak) → rename(temp, target)」
    // 三步，进程在两步之间被杀时目标文件暂时不存在。裸 `read_to_string` 会把这种
    // 情况判成「从没配过」→ 退回兼容模式（fail-open）；`read_with_backup` 会读 `.bak`。
    let content = match config::read_with_backup(&path) {
        Ok(content) => content,
        // 目标与 .bak 都不存在 = 从没配过，兼容模式（有意保留）。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return WsTrustState::Ok(WsTrust::default());
        }
        // 存在但读不了（权限/编码/路径是个目录……）同样按损坏处理：
        // 用户显然配了东西，此时退回兼容模式是一样的静默降级。
        Err(err) => return WsTrustState::Corrupt(format!("读取失败: {err}")),
    };
    // 空内容不在这里短路：交给 `parse_ws_trust` 判成 `Corrupt`（§4.4）。
    parse_ws_trust(&content)
}

/// 从 Origin 头里取出合法的 Chrome 扩展 ID（32 位 a-p）。
pub(crate) fn extension_id_from_origin(headers: &HeaderMap) -> Option<String> {
    let origin = headers.get(ORIGIN)?.to_str().ok()?;
    let extension_id = origin.strip_prefix(CHROME_EXTENSION_ORIGIN_PREFIX)?;
    if extension_id.len() == 32 && extension_id.bytes().all(|byte| matches!(byte, b'a'..=b'p')) {
        Some(extension_id.to_string())
    } else {
        None
    }
}

/// 仅作为「Origin 是否是合法扩展」的谓词保留（单元测试与文档用途）；
/// 生产握手路径用 `extension_id_from_origin`，因为它还要拿 id 做白名单比对。
#[allow(dead_code)]
pub(crate) fn is_allowed_extension_origin(headers: &HeaderMap) -> bool {
    extension_id_from_origin(headers).is_some()
}

/// 从 query string 里取一个参数（不引入额外依赖）。
pub(crate) fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name != key {
            return None;
        }
        Some(percent_decode(value))
    })
}

/// 百分号解码（query string 语义）。
///
/// **不**把裸 `+` 当空格：`+` 只在 `application/x-www-form-urlencoded` 的
/// **请求体**里有空格含义，query 里的 `+` 是合法字面字符。以前这里把 `+`
/// 解成空格，token 里带 `+` 且客户端没有 `encodeURIComponent` 时握手恒失败——
// 表现是「token 明明抄对了却 401」。真正的空格请用 `%20`。
pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub(crate) async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    // 1) 必须来自 Chrome 扩展。这一条保持原样：没有 Origin 的网页/本地进程一律拒绝。
    let Some(extension_id) = extension_id_from_origin(&headers) else {
        return (
            StatusCode::FORBIDDEN,
            "WebSocket connections require a Chrome extension Origin",
        )
            .into_response();
    };
    let trust = match load_ws_trust() {
        WsTrustState::Ok(trust) => trust,
        // 配置损坏 = fail-closed：拒绝握手，绝不静默退回「任意扩展可连」。
        WsTrustState::Corrupt(detail) => {
            let path = ws_trust_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<用户配置目录>/ws-trust.json".to_string());
            eprintln!(
                "WebSocket 信任配置损坏，已拒绝所有握手（fail-closed，不回退到兼容模式）: {path} ({detail})"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "WebSocket 信任配置损坏，已拒绝所有连接：为避免静默降级成「任意扩展可连」，daemon 不会把它当作未配置。配置文件：{path}；解析错误：{detail}。修复：删除该文件（回到兼容模式），或运行 atb trust-extension --clear 重建（atb trust-extension --list 会显示同一处错误）"
                ),
            )
                .into_response();
        }
    };
    // 2) 档位 + 白名单 + token 统一走纯函数判定。
    //
    // 旧实现只把 Origin 头当身份证明：本机任意进程伪造 `chrome-extension://<id>`
    // 就能接入（白名单也要先伪造 id 才绕得过，但 id 不是秘密）。现在由用户显式
    // 选择信任档位；默认 compat 完全保持旧行为，只多返回一条安全告警。
    let trust_mode = parse_ws_trust_mode(&crate::settings::ws_trust_mode());
    let provided_token = query_param(uri.query(), "token");
    let security_warning =
        match ws_handshake_check(trust_mode, &trust, &extension_id, provided_token.as_deref()) {
            Ok(warning) => warning,
            Err(reject) => {
                eprintln!(
                    "WebSocket 握手被拒绝（mode={} extension={extension_id}）：{}",
                    trust_mode.as_str(),
                    reject.message
                );
                return (
                    reject.status,
                    Json(json!({
                        "ok": false,
                        "error": reject.message,
                        "error_code": reject.error_code,
                        "hint": reject.hint,
                        "trust_mode": trust_mode.as_str(),
                    })),
                )
                    .into_response();
            }
        };
    // 3) compat 档且**完全没配**身份校验时，每个进程只提醒一次，别在握手热路径刷屏。
    if trust_mode == WsTrustMode::Compat
        && !trust.enforces_identity()
        && !state.ws_security_warned.swap(true, Ordering::Relaxed)
    {
        eprintln!(
            "WebSocket 未启用身份校验（ATB_WS_TRUST_MODE=compat），任意 Chrome 扩展/本机进程均可连接；可用 atb trust-extension <id> 或 ATB_WS_TRUST_MODE=token 加固"
        );
    }
    // 只留指纹进登记表，明文 token 不落地。
    let conn_token_fp = provided_token.as_deref().map(token_fingerprint);
    let mut response = ws
        .on_upgrade(move |socket| handle_socket(socket, state, extension_id, conn_token_fp))
        .into_response();
    // 兼容档在握手回包里给出机器可读告警（ASCII，避免非法 header 字节）。
    if let Some(warning) = security_warning {
        response.headers_mut().insert(
            "x-atb-ws-security-warning",
            axum::http::HeaderValue::from_static(warning),
        );
        response.headers_mut().insert(
            "x-atb-ws-trust-mode",
            axum::http::HeaderValue::from_static("compat"),
        );
    }
    response
}

/// 撤销信号的轮询间隔。信任配置变更极少发生，所以用轮询而不是全局条件变量：
/// 连接循环里只读一个 `AtomicBool`，不碰登记表锁（热路径无锁）。
pub(crate) const WS_REVOKE_POLL_INTERVAL: Duration = Duration::from_millis(500);

// 入站静默检查的轮询间隔。
pub(crate) const WS_INBOUND_CHECK_INTERVAL: Duration = Duration::from_secs(15);
// 允许的入站静默上限：超过即判定 TCP half-open。
///
/// 扩展每 30s 发一次 `{"type":"ping"}`，正常链路不可能连续 3 个周期一个字都不来。
/// 旧实现收到 ping 直接 `continue`：既不回 pong，也不记录最后入站时间，更没有
/// read deadline——于是「网线拔了 / NAT 表项过期 / 浏览器被挂起」时连接会一直显示
/// `extension_connected=true`，`/health.ready` 谎报，空闲自杀也永远判不出来。
///
/// 与扩展侧的分工：扩展的 `lastPongAt` 在 75s 静默时先做一次 HTTP 探活（客户端侧
/// 自愈优先），服务端这条 90s 硬超时是最后一道闸——两者阈值刻意错开，避免同时抖动。
pub(crate) const WS_INBOUND_SILENCE_LIMIT: Duration = Duration::from_secs(90);

// 入站静默是否已构成「半开连接」。
///
/// 只比阈值不做任何 IO：连接循环里的定时分支与单测用**同一个**判据，
/// 避免"测的和跑的不是一条线"。
pub(crate) fn inbound_is_half_open(silent: Duration) -> bool {
    silent > WS_INBOUND_SILENCE_LIMIT
}

/// 单条下行消息的发送上限。
///
/// `socket.send(...).await` 在对端不读时会一直挂着（TCP 缓冲区满），所以它必须
// 带超时：超时即主动断开，扩展会按自己的重试节奏重连，比僵在这里好。
///
/// S1-02：等待发送完成的这段时间**也**必须能被撤销打断——只把这个值当上限是不够的，
/// 那意味着撤销最多被拖后 `WS_SEND_TIMEOUT` 才生效。实际实现在发送期间每
/// `WS_REVOKE_POLL_INTERVAL`（500ms）查一次撤销位（见 `handle_socket`），
/// 所以「撤销后 ≤500ms 断开」的承诺与实现一致；本值只是「对端一直不读」时的兜底上限。
pub(crate) const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// 发送 Close 帧，带超时（S1-01）。
///
/// 撤销断开时旧实现直接 `socket.send(Message::Close(None)).await` 并把结果丢掉：对端不读
/// （TCP 缓冲区满）时这一句会一直挂着，而连接循环已经准备退出，挂住的代价是
/// 整条连接的收尾（含 `WsConnGuard` 注销、会话清理）全部推迟，且失败/超时被
/// 静默吞掉，日志里看不出连接为什么没断干净。
///
/// 现在与文本帧一样用 `WS_SEND_TIMEOUT` 兜底，并且**无论 Ok / Err / 超时**都只
/// 打一行 stderr（含 connection_id）后返回——调用方随后统一 `break`，
/// 不再有「等 Close 回执」这条无界等待路径。
async fn close_socket(socket: &mut WebSocket, extension_id: &str, conn_id: u64) {
    match tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Close(None))).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            eprintln!("扩展 {extension_id} 的 Close 帧发送失败（connection_id={conn_id}）: {err}")
        }
        Err(_) => eprintln!(
            "扩展 {extension_id} 在 {}s 内没有读走 Close 帧（connection_id={conn_id}），直接断开",
            WS_SEND_TIMEOUT.as_secs()
        ),
    }
}

/// 登记表加锁的唯一入口：即使某次持锁 panic 导致中毒，也继续用里面的数据，
/// 不能让「中毒」把撤销能力也一起弄坏（fail-closed 优先）。
pub(crate) fn ws_connections_lock(
    state: &AppState,
) -> std::sync::MutexGuard<'_, HashMap<u64, WsConn>> {
    state
        .ws_connections
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// 单条连接的下行队列上限。
///
/// 值域 `8..=65536`：太小会让正常心跳被丢，太大失去「有界」的意义。
/// 命令通道与 pong 通道各自独立，但共用一个上限：两条队列都只服务于
/// **同一条**对端连接，给它两倍缓冲没有意义，反而让「积压」更晚暴露。
pub(crate) fn ws_outbound_max() -> usize {
    crate::settings::ws_outbound_max()
}

// 等到队列入队成功的最长时间。
///
/// 这个值不是「对端不读」的上限（那是 [`WS_SEND_TIMEOUT`]），而是「队列满时
/// 业务侧愿意等多久」。取与发送同量级：再长会让调用方（HTTP handler）的
/// 请求预算被通道排队吃掉，用户看到超时却不知道是背压；再短则正常的突发
/// 批量下发会被误判成背压。
pub(crate) const WS_OUTBOUND_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(10);

// 把一条下行命令放进连接队列。
///
/// 旧实现用 `mpsc::unbounded_channel`：扩展读得慢（大 snapshot / 卡住的标签页）时
/// 命令在内存里无限堆积，daemon 只看到内存上涨，命令方也永远收不到「对端跟不上」。
/// 现在通道有界，这里负责把「满」和「断」翻译成**结构化**错误：
/// - `ws_backpressure`：队列在 [`WS_OUTBOUND_ENQUEUE_TIMEOUT`] 内没有腾出空位（可重试）；
/// - `ws_disconnected`：接收端已关闭（连接已收尾，必须重连）。
pub(crate) async fn send_downstream(sender: &mpsc::Sender<String>, payload: String) -> Result<()> {
    send_downstream_within(sender, payload, WS_OUTBOUND_ENQUEUE_TIMEOUT).await
}

/// [`send_downstream`] 的可配超时版本。
///
/// 关机清理这类「一条连接一条命令、不能各等 10s」的路径需要更短的窗口：
/// daemon 已经在退出，等不到就直接记 `sent:false`，比拖住整个关闭流程划算。
pub(crate) async fn send_downstream_within(
    sender: &mpsc::Sender<String>,
    payload: String,
    wait: Duration,
) -> Result<()> {
    match tokio::time::timeout(wait, sender.send(payload)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SessionSelectError {
            code: "ws_disconnected",
            message: "浏览器扩展连接已断开，命令未下发".to_string(),
            hint: "扩展连接已收尾（扩展重载 / 浏览器关闭 / daemon 刚重启）；先用 status 确认扩展已重连，再重发".to_string(),
            retryable: true,
            retry_after_ms: Some(3000),
        }
        .into_anyhow()),
        Err(_) => Err(SessionSelectError {
            code: "ws_backpressure",
            message: format!(
                "扩展下行队列在 {}ms 内一直是满的，命令未下发（连接仍在，但对端读不过来）",
                wait.as_millis()
            ),
            hint: "对端可能在处理大页面/大响应；减小并发或分批操作，稍后用同一 --idempotency-key 重试".to_string(),
            retryable: true,
            retry_after_ms: Some(1000),
        }
        .into_anyhow()),
    }
}

/// 一次握手的 `nonce`：32 位小写 hex（128bit）。
///
/// 取两个 v4 UUID 的前 16 位十六进制拼接：一次 `Uuid::new_v4()` 的熵足够，
/// 两段的意义是避免「同一毫秒内两个连接拿到同前缀」这种可预测形态，
/// 同时保持字节级简单（不引 rand / getrandom 直接依赖）。
pub(crate) fn new_ws_nonce() -> String {
    let first = Uuid::new_v4().simple().to_string();
    let second = Uuid::new_v4().simple().to_string();
    format!("{}{}", &first[..16], &second[..16])
}

// 握手挑战帧。
///
/// daemon 先证明自己：`proof = HMAC-SHA256(key=ws-trust token, msg=nonce|extension_id)`。
/// 扩展用本地 `wsToken` 复算并做恒定时间比较——本机进程即使抢占了扩展端口，
/// 不知道 token 就算不出 proof，无法让扩展发出 `ext_ready`（含全量标签与 exec 能力）。
///
/// `auth_required=false`（没配 token）时 `proof` 必须是 `null`：扩展据此保持旧的
/// 兼容路径（onopen 立即 `ext_ready`），老 daemon 不发这一帧时行为也不变。
pub(crate) fn ws_hello_value(extension_id: &str, token: Option<&str>) -> Value {
    ws_hello_with_nonce(extension_id, token, &new_ws_nonce())
}

/// 用**指定** nonce 构造 `ws_hello`。
///
/// 抽出来是为了让「线协议」本身可被固定向量锁住：`ws_hello_value` 每次换 nonce，
/// 单测只能自证（自己的 HMAC 算自己的串），改了分隔符/拼接顺序也测不出来。这里允许
/// 传入固定 nonce，测试就能断言一条与扩展侧 WebCrypto 对拍过的完整 proof。
pub(crate) fn ws_hello_with_nonce(extension_id: &str, token: Option<&str>, nonce: &str) -> Value {
    let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else {
        return json!({
            "type": "ws_hello",
            "v": 1,
            "nonce": nonce,
            "proof": Value::Null,
            "auth_required": false,
        });
    };
    // 消息里带 extension_id：proof 只对这一条扩展有效，避免同一 token 下的
    // 其它扩展把挑战帧原样转发给被劫持的连接。
    let message = format!("{nonce}|{extension_id}");
    let proof = crate::hmac::hmac_sha256(token.as_bytes(), message.as_bytes());
    json!({
        "type": "ws_hello",
        "v": 1,
        "nonce": nonce,
        "proof": crate::hmac::hex_lower(&proof),
        "auth_required": true,
    })
}

/// 单帧发送的结果（发送/撤销/超时三态，供命令队列与 pong 队列共用）。
enum WsSendOutcome {
    Sent,
    Failed(String),
    TimedOut,
    Revoked,
}

/// 带超时且可被撤销打断地发送一条文本帧。
///
/// 与旧内联实现等价：`WS_SEND_TIMEOUT` 是对端一直不读时的兜底上限；
/// 等待期间每 `WS_REVOKE_POLL_INTERVAL` 查一次撤销位，保证「撤销后 ≤500ms 断开」。
async fn send_text_guarded(
    socket: &mut WebSocket,
    text: String,
    revoked: &AtomicBool,
) -> WsSendOutcome {
    let send = socket.send(Message::Text(text));
    tokio::pin!(send);
    let deadline = tokio::time::sleep(WS_SEND_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            result = &mut send => {
                return match result {
                    Ok(()) => WsSendOutcome::Sent,
                    Err(err) => WsSendOutcome::Failed(err.to_string()),
                };
            }
            _ = &mut deadline => return WsSendOutcome::TimedOut,
            _ = tokio::time::sleep(WS_REVOKE_POLL_INTERVAL) => {
                // 撤销位一旦置位就立刻放弃这条消息（连接马上就要断）。
                if revoked.load(Ordering::Relaxed) {
                    return WsSendOutcome::Revoked;
                }
            }
        }
    }
}

// 连接结束后的会话清理。
///
/// 从 `handle_socket` 末尾抽出来，既供正常路径 `await`，也供守卫在 panic 展开时
/// spawn 兜底。只处理「确实属于这条连接」的会话：扩展重载时新旧连接会短暂并存，
/// 旧连接晚一步收尾不能把新连接刚注册的会话标成断开再删掉。
///
// 连接代际 + 兜底：`session_key` 是确定性拼接，新连接重注册后会拿到
/// **同一个 key**。判定因此是「`conn_id` 相等 **或**（`conn_id == 0` 且 sender 同通道）」：
/// - `conn_id` 相等 = 显式代数匹配；
/// - `conn_id == 0` 是 refresh 登记路径在「找不到连接」时的占位值，历史实现里这类
///   会话永远清不掉（`0 != conn_id` 恒成立），必须按 `same_channel` 兜底。
///
/// 先克隆登记表（std Mutex，语句结束即释放）再取 driver 写锁：两把锁从不嵌套，
/// 与 `apply_tabs_snapshot`（driver → registry，且 registry 在 drop 之后才取）一致。
pub(crate) async fn cleanup_conn_sessions(
    state: &AppState,
    conn_id: u64,
    sender: &mpsc::Sender<String>,
    registered_ids: &std::sync::Mutex<HashSet<String>>,
) {
    let owned: Vec<String> = {
        let registered_all: Vec<String> = registered_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect();
        let mut driver = state.driver.write().await;
        let mut owned = Vec::new();
        for id in &registered_all {
            if let Some(session) = driver.sessions.get_mut(id) {
                let same_sender = session.sender.same_channel(sender);
                // `conn_id == 0` 的会话靠 sender 兜底，否则永远清不掉。
                let belongs_to_this =
                    session.conn_id == conn_id || (session.conn_id == 0 && same_sender);
                if !belongs_to_this {
                    // 同一 key 已被新一代连接接管：这是**别人的**会话，跳过并留痕。
                    eprintln!(
                        "ws: 连接 {conn_id} 断开时跳过会话 {id}（当前代际 {} 已接管）",
                        session.conn_id
                    );
                    continue;
                }
                session.disconnected_at = Some(Instant::now());
                owned.push(id.clone());
            }
        }
        owned
    };
    // 扩展断开时，挂在这些会话上的命令不会再有结果。
    // 立即以 extension_disconnected 失败，别让 CLI 干等到 15~60 秒超时。
    //
    // 只对本连接**确实拥有**的会话取在途执行：`execs.active` 是「exec_id -> session_key」，
    // 按 key 批量取会把新代际在同一标签上的在途执行一起打死。
    {
        let mut execs = state.execs.lock().await;
        for session_key in owned.iter() {
            for pending in execs.take_session(session_key) {
                let _ = pending.tx.send(Err(anyhow!(
                    "extension_disconnected: 浏览器扩展连接已断开，命令未完成（daemon 可能重启或插件被重载）"
                )));
            }
        }
    }
    // 连接已断 = 会话确实销毁，会话级缓存（snapshots / page_scale / session_locks）
    // 一并回收；扩展重连会重新 ExtReady 注册，代价只是重新探测一次页面规模。
    //
    // 断线走 `forget_session_after_disconnect`——它额外把「原来的默认目标」记进
    // preferred，等重连后同一个 session_key 回到快照时再设回默认。真实关闭（close/prune/
    // tab 关闭事件）仍走 `forget_session`，会把这个记忆一起忘掉。
    for session_key in owned.iter() {
        forget_session_after_disconnect(state, session_key).await;
    }
    // 登记表随会话回收。这个 Arc 在连接断开后本来就没有读者，
    // 但 refresh 路径可能还短暂持有它（`connection_session_registry` 返回克隆），
    // 清空后即便被读到也不会再指向已销毁的会话。
    registered_ids
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

pub(crate) struct WsConnGuard {
    pub(crate) state: AppState,
    pub(crate) conn_id: u64,
}

impl WsConnGuard {
    // 幂等注销：连接循环一 break 就调用，别等函数末尾的 Drop。
    ///
    /// 旧实现把注销完全交给 Drop，`break` 之后还要排队做会话清理、`forget_session`、
    /// 向扩展发 debugClearAll……这整个异步窗口里死连接仍在 `ws_connections` 里，
    /// 广播/`/reload-trust` 仍会选中它并静默丢消息。`remove` 本身幂等，重复调用无害。
    pub(crate) fn unregister(&self) {
        // 全程无 await（std Mutex），所以能在 Drop 里安全完成。
        ws_connections_lock(&self.state).remove(&self.conn_id);
    }
}

impl Drop for WsConnGuard {
    fn drop(&mut self) {
        self.unregister();
    }
}

// 会话清理守卫：会话清理是异步的（driver/execs 都是 tokio 锁），
/// 没法放进 `Drop` 直接做，所以分两层：
/// - 正常路径在 `handle_socket` 末尾显式 `cleanup_conn_sessions(...).await`，
///   成功后 `disarm()`；
/// - 只有 panic 展开等异常路径才会走到 `Drop`：同步注销连接（幂等），
///   再把清理任务 spawn 到当前 runtime，保证会话不会永久滞留在「已连接」状态。
pub(crate) struct WsSessionCleanupGuard {
    state: AppState,
    conn_id: u64,
    sender: mpsc::Sender<String>,
    registered_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    armed: bool,
}

impl WsSessionCleanupGuard {
    pub(crate) fn new(
        state: AppState,
        conn_id: u64,
        sender: mpsc::Sender<String>,
        registered_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            state,
            conn_id,
            sender,
            registered_ids,
            armed: true,
        }
    }

    /// 显式清理已经完成，Drop 时不再重复 spawn。
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WsSessionCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        // 同步部分：注销登记（再调一次也幂等）。
        ws_connections_lock(&self.state).remove(&self.conn_id);
        // 异步部分：交给当前 runtime；不在 runtime 上下文（理论不可达）时只能放弃。
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            eprintln!(
                "ws: 连接 {} 异常结束时不在 tokio runtime 内，跳过会话清理兜底",
                self.conn_id
            );
            return;
        };
        let state = self.state.clone();
        let conn_id = self.conn_id;
        let sender = self.sender.clone();
        let registered = self.registered_ids.clone();
        handle.spawn(async move {
            cleanup_conn_sessions(&state, conn_id, &sender, &registered).await;
        });
    }
}

pub(crate) async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    extension_id: String,
    conn_token_fp: Option<u64>,
) {
    // 登记这条连接（N3-04）：`/reload-trust` 通过它把「握手时合法、之后被移出白名单
    // 或 token 收紧」的连接真正断开，而不是只在下次握手时才生效。
    let conn_id = state.ws_conn_seq.fetch_add(1, Ordering::Relaxed) as u64;
    let revoked = Arc::new(AtomicBool::new(false));
    // 下行通道必须先建好：登记表要带着它，断开清理与 refresh 才能按 `same_channel`
    // 找到"这条连接登记过哪些会话"。
    //
    // 命令队列换成**有界**通道。旧的无界队列在扩展读得慢时把命令无限堆在
    // 内存里，发送方也永远察觉不到「对端跟不上」；现在满则走
    // [`send_downstream`] 的 `ws_backpressure` 背压语义。
    let (tx, mut rx) = mpsc::channel::<String>(ws_outbound_max());
    // pong 走独立的有界控制通道，不再和命令挤同一个队列。
    // 控制队列满时丢弃 pong（下个心跳会重发，比拖住命令更划算）。
    let (pong_tx, mut pong_rx) = mpsc::channel::<String>(ws_outbound_max());
    let mut pong_dropped = 0usize;
    // 会话登记表放进登记表共享给 refresh 路径：refresh 自己攒一份再丢掉的旧写法，
    // 会让"由 refresh 首次登记的会话"在断开时漏清理。
    let registered_ids: Arc<std::sync::Mutex<HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    ws_connections_lock(&state).insert(
        conn_id,
        WsConn {
            extension_id: extension_id.clone(),
            token_fingerprint: conn_token_fp,
            revoked: revoked.clone(),
            session_registry: registered_ids.clone(),
            sender: tx.clone(),
        },
    );
    // 注销靠 Drop：正常 break / 提前 return / panic 都不会漏。
    // 正常路径在 break 后立刻显式 `unregister()`（见循环之后），Drop 只是兜底。
    let conn_guard = WsConnGuard {
        state: state.clone(),
        conn_id,
    };
    // 会话清理的异常安全兜底（panic 展开时也能清掉会话）。
    let mut session_guard =
        WsSessionCleanupGuard::new(state.clone(), conn_id, tx.clone(), registered_ids.clone());
    // 进入收帧循环前**立即**下发握手挑战帧，让扩展先验证"对端确实是持有
    // token 的 daemon"再发 `ext_ready`（那里带着全量标签、版本与后续 exec 能力）。
    //
    // 这是 upgrade 之后的独立 task，不会阻塞 HTTP 握手；发送失败/超时直接 return，
    // 让上面两个守卫按原有路径注销连接并清理会话（它们已 armed）。
    {
        // token 必须取**当前配置**里的值，不是客户端在 `?token=` 里给的那份：
        // 要证明的是"daemon 持有服务端口令"，用客户端提供的值等于自证。
        let hello_token = match load_ws_trust() {
            WsTrustState::Ok(trust) => trust.token_value().map(str::to_string),
            // 握手阶段已对损坏配置 fail-closed；走到这里只可能是刚被改坏。
            // 按"没有 token"下发（auth_required:false）比卡住连接更安全：
            // 扩展配了 token 会把这次当作对端不诚实的失败并重连。
            WsTrustState::Corrupt(_) => None,
        };
        let hello = ws_hello_value(&extension_id, hello_token.as_deref()).to_string();
        match tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Text(hello))).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                eprintln!(
                    "扩展 {extension_id} 的 ws_hello 发送失败，直接断开（connection_id={conn_id}）: {err}"
                );
                return;
            }
            Err(_) => {
                eprintln!(
                    "扩展 {extension_id} 在 {}s 内没有读走 ws_hello，直接断开（connection_id={conn_id}）",
                    WS_SEND_TIMEOUT.as_secs()
                );
                return;
            }
        }
    }
    // 收帧循环只负责「读帧 / 刷新 last_inbound / 跑定时器」，
    // `handle_ws_message` 派发到独立 task 串行执行（独立 channel 保序）。
    //
    // 旧实现把 `handle_ws_message(...).await` 放在 select 分支体里：慢分发（等
    // driver 写锁、apply_tabs_snapshot）期间 select 不再轮询 90s/500ms 定时器，
    // 健康连接会被误判半开，撤销轮询也会停摆。
    let (dispatch_tx, mut dispatch_rx) = mpsc::unbounded_channel::<WsIncoming>();
    let dispatch_state = state.clone();
    let dispatch_sender = tx.clone();
    let dispatch_registry = registered_ids.clone();
    let mut dispatch_task = tokio::spawn(async move {
        while let Some(incoming) = dispatch_rx.recv().await {
            handle_ws_message(
                incoming,
                &dispatch_state,
                dispatch_sender.clone(),
                conn_id,
                &dispatch_registry,
            )
            .await;
        }
    });
    // 撤销检查走独立定时分支：连接里只读一个 AtomicBool，不查询全局登记表。
    let mut revoke_tick = tokio::time::interval(WS_REVOKE_POLL_INTERVAL);
    // 最后一次收到**任何**入站帧的时刻（文本/二进制/ping 都算）。
    // 只在 socket.recv 分支里刷新，不涉及任何锁。
    let mut last_inbound = Instant::now();
    // 入站静默检查用独立定时分支（与 revoke 轮询分开，互不影响节奏）。
    let mut inbound_tick = tokio::time::interval(WS_INBOUND_CHECK_INTERVAL);
    // 首次 tick 立刻完成，等价于「进来就查一次」，不影响后续节奏。
    //
    // 外层循环带标号：发送分支里「超时 / 发送失败 / 被撤销」都要直接结束整条连接
    // （普通 `break` 只会退出内层选择，S1-02）。
    'connection: loop {
        tokio::select! {
            Some(outgoing) = rx.recv() => {
                // 带超时 + 可被撤销打断的发送（实现见 `send_text_guarded`）：
                // 对端不读时不能把整个连接循环（含 revoke 轮询）拖死；等待期间
                // 撤销位一旦置位就立刻放弃这条消息（连接马上就要断）。
                match send_text_guarded(&mut socket, outgoing, revoked.as_ref()).await {
                    WsSendOutcome::Sent => {}
                    WsSendOutcome::Failed(err) => {
                        eprintln!(
                            "扩展 {extension_id} 下行发送失败，断开连接等待重连（connection_id={conn_id}）: {err}"
                        );
                        break 'connection;
                    }
                    WsSendOutcome::TimedOut => {
                        eprintln!(
                            "扩展 {extension_id} 在 {}s 内没有读走下行消息，断开连接等待重连（connection_id={conn_id}）",
                            WS_SEND_TIMEOUT.as_secs()
                        );
                        break 'connection;
                    }
                    WsSendOutcome::Revoked => {
                        eprintln!(
                            "扩展 {extension_id} 已不再满足当前信任配置，断开其现有连接（connection_id={conn_id}）"
                        );
                        close_socket(&mut socket, &extension_id, conn_id).await;
                        break 'connection;
                    }
                }
            }
            // pong 走独立优先通道。命令队列积压时心跳仍能按时回，
            // 扩展不会因为「daemon 正在发大批结果」而误判断线。
            Some(pong) = pong_rx.recv() => {
                match send_text_guarded(&mut socket, pong, revoked.as_ref()).await {
                    WsSendOutcome::Sent => {}
                    WsSendOutcome::Revoked => {
                        close_socket(&mut socket, &extension_id, conn_id).await;
                        break 'connection;
                    }
                    WsSendOutcome::Failed(err) => {
                        eprintln!(
                            "扩展 {extension_id} 的 pong 发送失败，断开连接等待重连（connection_id={conn_id}）: {err}"
                        );
                        break 'connection;
                    }
                    WsSendOutcome::TimedOut => {
                        eprintln!(
                            "扩展 {extension_id} 在 {}s 内没有读走 pong，断开连接等待重连（connection_id={conn_id}）",
                            WS_SEND_TIMEOUT.as_secs()
                        );
                        break 'connection;
                    }
                }
            }
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break; };
                // **任何**入站帧都算「对端还活着」。扩展 30s 一次文本 ping，
                // 正常链路不可能静默；协议层 Ping/Pong 同样刷新（tungstenite 自动回 Pong，
                // 这里只用来判活，不参与业务）。
                match &msg {
                    Message::Text(_)
                    | Message::Binary(_)
                    | Message::Ping(_)
                    | Message::Pong(_) => last_inbound = Instant::now(),
                    Message::Close(_) => {}
                }
                let Message::Text(text) = msg else { continue; };
                if is_app_ping(&text) {
                    // 回一条应用层 pong（旧实现直接 `continue`，扩展永远收不到回应，
                    // 只能靠 HTTP 探活兜底）。：pong 走独立的有界优先通道，
                    // 满时丢弃这一条（下个心跳周期会重发），绝不拖住命令下行。
                    let pong = json!({ "type": "pong" }).to_string();
                    if pong_tx.try_send(pong).is_err() {
                        pong_dropped += 1;
                        if pong_dropped == 1 || pong_dropped.is_multiple_of(64) {
                            eprintln!(
                                "扩展 {extension_id} 的控制队列已满（ATB_WS_OUTBOUND_MAX={}），已丢弃 {pong_dropped} 条 pong（connection_id={conn_id}）",
                                ws_outbound_max()
                            );
                        }
                    }
                    continue;
                }
                // 扩展在 `ext_ready` 里带 `restartSeq`（Service Worker 冷启动 +1）。
                // `WsIncoming::ExtReady` 没声明这个字段，反序列化时会被静默丢弃，
                // 所以这里在原始文本上先取一次（只有 ext_ready 会带，热路径无额外成本）。
                if text.contains("\"restartSeq\"") {
                    if let Some((browser_id, profile_id, seq)) = parse_restart_seq(&text) {
                        note_extension_restart(&state, &browser_id, &profile_id, seq).await;
                    }
                }
                match serde_json::from_str::<WsIncoming>(&text) {
                    Ok(incoming) => {
                        // 派发到独立 task 串行处理，收帧循环立即回到 select，
                        // 90s 半开检测/撤销轮询不再被慢分发饿死。
                        if dispatch_tx.send(incoming).is_err() {
                            eprintln!(
                                "扩展 {extension_id} 的消息分发队列已关闭，断开连接（connection_id={conn_id}）"
                            );
                            break 'connection;
                        }
                    }
                    Err(err) => eprintln!("WebSocket 消息解析失败: {err}: {text}"),
                }
            }
            _ = revoke_tick.tick() => {
                if revoked.load(Ordering::Relaxed) {
                    // 撤销原因可能是白名单也可能是 token（见 `ws_trust_revoke_reason`），
                    // 这里只说"不再被信任"，具体原因由 `/reload-trust` 回包给出。
                    eprintln!(
                        "扩展 {extension_id} 已不再满足当前信任配置，断开其现有连接（connection_id={conn_id}）"
                    );
                    close_socket(&mut socket, &extension_id, conn_id).await;
                    break;
                }
            }
            // 入站静默硬超时。TCP half-open（拔网线 / NAT 表项过期 /
            // 浏览器被挂起）时 `socket.recv()` 不会返回，连接会一直显示「已连接」，
            // `/health.ready` 谎报，空闲自杀也永远判不出来。
            //
            // 这里只做「断开」这一个动作，不 touch 任何状态：连接结束后的
            // WsConnGuard 注销、会话清理、idle/inflight 判定都走原有路径，
            // 不会因为心跳检查而把「假活连接」换成「daemon 不肯退出」。
            _ = inbound_tick.tick() => {
                let silent = last_inbound.elapsed();
                if inbound_is_half_open(silent) {
                    eprintln!(
                        "扩展 {extension_id} 已 {}s 没有任何入站消息（connection_id={conn_id}），判定为半开连接，主动断开等待重连",
                        silent.as_secs()
                    );
                    close_socket(&mut socket, &extension_id, conn_id).await;
                    break 'connection;
                }
            }
        }
    }
    // 先把这条连接从登记表摘掉，再做后续异步清理。旧实现要等到函数
    // 末尾（会话清理、forget_session 全部完成之后）才靠 Drop 注销，这个窗口里
    // 死连接仍会被广播 / `/reload-trust` 选中并静默丢消息。`remove` 幂等。
    conn_guard.unregister();
    // 先让分发队列把已经收到的消息处理完，再清理会话——否则最后一条
    // `ext_ready`/`tabs_update` 可能在清理之后才落库，把刚清掉的会话重新登记。
    drop(dispatch_tx);
    match tokio::time::timeout(Duration::from_secs(5), &mut dispatch_task).await {
        Ok(_) => {}
        Err(_) => {
            eprintln!(
                "ws: 连接 {conn_id} 的消息分发任务未在 5s 内退出，强制中止（避免拖住断开清理）"
            );
            dispatch_task.abort();
        }
    }
    // 正常路径显式清理；守卫在 panic 展开时用 spawn 兜底。
    cleanup_conn_sessions(&state, conn_id, &tx, &registered_ids).await;
    session_guard.disarm();
}

pub(crate) fn should_set_default_session(driver: &DriverState, session_key: &str) -> bool {
    // 读的是「仍然有效的」待生效目标：过期的记忆（6h）在这里当作没有，
    // 否则一次断线留下的旧 key 会让默认目标再也建立不起来。
    match driver.preferred_default_session() {
        Some(preferred) => preferred == session_key,
        // 默认目标粘性：**只有当前默认已失效（会话不存在 / 非 active）才允许改选**。
        //
        // 旧实现只看 `default_session_key.is_none()`，两头的表现都不对：
        // - 默认指向一个「已断开但仍在表内」的会话（`disconnected_at.is_some()`）时永远不换，
        //   用户看到的是命令一直落在死会话上；
        // - 而「默认还健康」的保护完全依赖调用点不去清空它，多浏览器并存时一旦默认被
        //   清空（WS 断开清理 / prune），另一个浏览器下一次推送就把默认抢走，且没有提示。
        //
        // 收敛成一句判据：active 就不动，失效才改选。`open` 显式打开新标签属例外
        // （不经过这里，见 `open.rs` / `register_opened_session`），行为保持不变。
        None => match driver.default_session_key.as_deref() {
            None => true,
            Some(current) => !driver
                .sessions
                .get(current)
                .is_some_and(|session| session.is_active()),
        },
    }
}

// 迟到结果暂存的保留时间。
///
/// 旧实现写死 300s（5 分钟）：长任务超时后，用户还没看到提示结果就过期了。
/// 现在每次调用读取 `ATB_EXEC_RESULT_TTL_SEC`（默认 3600s，值域 60..86400），
/// `GET /exec-result/:exec_id` 的 `ttl_seconds` 与清理判定同源。
pub(crate) fn late_result_ttl() -> Duration {
    Duration::from_secs(crate::settings::exec_result_ttl_sec())
}
/// 迟到结果暂存的兜底上限：防异常扩展灌爆内存，超出淘汰最旧一项。
pub(crate) const LATE_RESULT_LIMIT: usize = 512;

// 把迟到结果/错误存入暂存表。
///
/// 旧实现在 pending 已被超时/断开路径 take 掉之后收到扩展回包，直接静默丢弃：
/// 调用方只知道「超时」，动作实际已执行成什么样永远查不到。暂存后 CLI 可凭
/// 错误信息里的 exec_id 用 `GET /exec-result/:id`（或 `atb job result`）查回。
pub(crate) async fn store_late_result(state: &AppState, exec_id: &str, value: Value) {
    let mut late = state.late_results.lock().await;
    let now = Instant::now();
    let ttl = late_result_ttl();
    // 顺带清理过期项。
    late.retain(|_, (_, at)| now.duration_since(*at) < ttl);
    // 兜底上限：HashMap 无序，按登记时间找最旧的一项淘汰。
    if late.len() >= LATE_RESULT_LIMIT && !late.contains_key(exec_id) {
        if let Some(oldest) = late
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(key, _)| key.clone())
        {
            late.remove(&oldest);
        }
    }
    late.insert(exec_id.to_string(), (value, now));
}

/// 上行消息分发。
///
/// **锁约束（旧报告 #1 的死锁，防止回归）**：扩展的回包必须先进入这条消息循环
/// 才能被处理，所以任何"持锁等回包"都是死锁。当前实现遵守：
/// - `Ack` / `Result` / `Error` 只碰 `execs`（`state.execs`）与**同步**的
// `ws_connections` 登记表，不碰 `driver`、不做跨任务等待；
/// - `driver` 锁只在 `apply_tabs_snapshot` 内短暂持有，持锁期间没有任何 await
///   外部回包（登记表合并也挪到了 drop 之后）；
/// - 需要等结果的地方一律「取 sender → 释放 `driver` 锁 → 等 oneshot」
///   （见 `exec.rs::execute_raw_js_inner`）。
///
/// 加锁顺序固定为 `execs → ws_connections → session_registry`：后两者都是
/// `std::sync::Mutex` 且从不在持有时 await，登记表侧也从不反向取 `execs`，
/// 因此不构成环。
///
/// 例外且**不是**死锁：`hold_lock=true` 的执行会故意持有**会话锁**跨 await，
/// 让同一标签的命令串行（避免两次点击互相踩）。回包路径不碰会话锁，两者不构成环。
pub(crate) async fn handle_ws_message(
    incoming: WsIncoming,
    state: &AppState,
    sender: mpsc::Sender<String>,
    conn_id: u64,
    registered_ids: &std::sync::Mutex<HashSet<String>>,
) {
    // 结果 / ACK 只碰「在途执行表」，不再抢全局 driver 锁：
    // 每个命令的回包都会走到这里，之前它和会话读写共用一把锁。
    match &incoming {
        WsIncoming::Ack { id } => {
            let mut execs = state.execs.lock().await;
            // 只有仍然在途的请求才记 ACK：迟到的 / 重复的 ACK 以前会永久留在
            // `acked` 里（只有 Result/Error/超时 才 take），长跑 daemon 上只增不减。
            // 加了这层判断后集合规模天然被 pending 数量约束。
            if execs.pending.contains_key(id) {
                // ACK 也必须来自「这条会话当前归属的那一代连接」。
                // 扩展重载时旧连接可能晚一步送来 ACK，若照单全收，
                // 超时文案会误报「已送达」而实际没有。
                let owner_matches = execs
                    .active
                    .get(id)
                    .map(|session_key| {
                        session_owner_conn(state, session_key)
                            .map(|owner| owner == conn_id)
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if !owner_matches {
                    eprintln!("ws: 丢弃连接 {conn_id} 的迟到 ACK {id}（会话已由新连接接管）");
                    return;
                }
                // 兜底上限：`acked` 无序，直接整体清空最省事；它只影响超时文案的
                // 「是否收到过 ACK」提示，清空不会让任何请求提前失败。
                if execs.acked.len() >= ACK_TRACK_LIMIT {
                    eprintln!("ack 跟踪表超过 {ACK_TRACK_LIMIT} 条，已重置");
                    execs.acked.clear();
                }
                execs.acked.insert(id.clone());
                if let Some(pending) = execs.pending.get_mut(id) {
                    pending.delivered_at = Some(Instant::now());
                }
            }
            return;
        }
        WsIncoming::Result {
            id,
            result,
            new_tabs,
        } => {
            // 代际判定必须在 `take(id)` **之前**。
            //
            // 旧实现先 take 再判 owner：一旦这条 Result 来自旧连接，pending 已被
            // 移走，oneshot 被静默丢弃，调用方只能等到泛化的 RecvError/超时；
            // 而真正属于新连接的结果也不会再来。现在「不属于本连接」就整条放弃、
            // **保留** pending/active 登记，让命令按自己的超时（或新连接的正常回包）收敛。
            //
            // 锁序：execs（tokio）→ ws_connections → session_registry（后两者 std），
            // 与 ACK 分支、模块注释里的约定一致；取得 execs 之后不再 await。
            let pending = {
                let mut execs = state.execs.lock().await;
                let session_key = execs.active.get(id).cloned();
                if let Some(session_key) = session_key.as_deref() {
                    if let Some(owner) = session_owner_conn(state, session_key) {
                        if owner != conn_id {
                            eprintln!(
                                "ws: 丢弃连接 {conn_id} 的迟到结果 {id}（会话 {session_key} 已由连接 {owner} 接管，保留原登记）"
                            );
                            // 不放 late_results：那是「请求已完成但调用方超时」的补偿路径，
                            // 而这一条根本没被执行者认可过，缓存它只会误导后续查询。
                            return;
                        }
                    }
                }
                execs.take(id)
            };
            if let Some(pending) = pending {
                let _ = pending.tx.send(Ok(ExecResult {
                    data: Some(result.clone()),
                    result: None,
                    closed: None,
                    new_tabs: new_tabs.clone(),
                }));
            } else {
                // 迟到结果暂存：pending 不存在说明请求已被超时/断开路径
                // take 掉（或 id 未知）。旧实现在这里静默丢弃——调用方只知道
                // 「超时」，动作实际已执行成什么样永远查不到。现在按
                // `late_result_ttl()` 暂存，CLI 可凭 exec_id 用 GET /exec-result/:id
                // 或 `atb job result <exec_id>` 查回。
                let mut value = json!({ "result": result.clone(), "error": Value::Null });
                if let Some(tabs) = new_tabs.clone() {
                    value["newTabs"] = tabs;
                }
                store_late_result(state, id, value).await;
            }
            return;
        }
        WsIncoming::Error {
            id,
            error,
            new_tabs,
        } => {
            // 同 Result 分支——代际不符时保留登记，不 take。
            let pending = {
                let mut execs = state.execs.lock().await;
                let session_key = execs.active.get(id).cloned();
                if let Some(session_key) = session_key.as_deref() {
                    if let Some(owner) = session_owner_conn(state, session_key) {
                        if owner != conn_id {
                            eprintln!(
                                "ws: 丢弃连接 {conn_id} 的迟到错误 {id}（会话 {session_key} 已由连接 {owner} 接管，保留原登记）"
                            );
                            return;
                        }
                    }
                }
                execs.take(id)
            };
            if let Some(pending) = pending {
                let mut value = json!({ "error": error.clone() });
                if let Some(tabs) = new_tabs.clone() {
                    value["newTabs"] = tabs;
                }
                let _ = pending.tx.send(Err(anyhow!(value.to_string())));
            } else {
                // 迟到错误同样暂存（对称于 Result 分支），不再静默丢弃。
                let value = json!({ "result": Value::Null, "error": error.clone() });
                store_late_result(state, id, value).await;
            }
            return;
        }
        _ => {}
    }

    match incoming {
        WsIncoming::ExtReady {
            browser_id,
            browser_label,
            profile_id,
            profile_label,
            extension_version,
            protocol,
            file_scheme_access,
            warnings,
            tabs,
        }
        | WsIncoming::TabsUpdate {
            browser_id,
            browser_label,
            profile_id,
            profile_label,
            extension_version,
            protocol,
            file_scheme_access,
            warnings,
            tabs,
        } => {
            // 扩展自检信息以前被 serde 静默丢弃（R-19）：最典型的是
            // 「queryTabs 失败 → 上报 0 个标签」，用户只看到「没有可用标签页」，
            // 完全不知道扩展自己已经报了错。留最近一次快照给 `/health` 展示。
            if !warnings.is_empty() {
                *state.ext_warnings.lock().await = warnings.clone();
            }
            // **空表 + 标签枚举失败**不是权威事实。
            //
            // `queryTabs` 失败时扩展会带着 warnings 上报 0 个标签（见 assets/*/background.js），
            // 旧实现把它当成「所有标签都没了」，于是 `apply_tabs_snapshot` 把全部会话
            // 标成断开——一次扩展内部抖动就等于「整批标签消失」，紧接着的重连又会把它们
            // 重建，用户看到标签页随机闪断。这里保留旧会话表，等下一次可信快照来覆盖。
            //
            // 判定只看告警里有没有 `queryTabs`：`warnings` 是与标签无关的混装数组，
            // 真实空浏览器也可能带告警，那种情况必须照常落库（见 `snapshot_is_invalid`）。
            if snapshot_is_invalid(&warnings, tabs.len()) {
                eprintln!(
                    "snapshot_invalid: 扩展上报 0 个标签且告警含 queryTabs 失败，本次快照不可信，保留现有会话表（browser={browser_id} profile={profile_id} warnings={warnings:?}）"
                );
                return;
            }
            if tabs.is_empty() && !warnings.is_empty() {
                // 空表 + 与标签枚举无关的告警：照常落库（用户确实关光了标签），
                // 只留一行诊断，说明我们**故意**没有走 snapshot_invalid 分支。
                eprintln!(
                    "snapshot: 空标签快照，告警与标签枚举无关，按真实空表处理（browser={browser_id} profile={profile_id} warnings={warnings:?}）"
                );
            }
            apply_tabs_snapshot(
                state,
                &sender,
                conn_id,
                browser_id,
                browser_label,
                profile_id,
                profile_label,
                extension_version,
                protocol,
                file_scheme_access,
                tabs,
                registered_ids,
            )
            .await;
        }
        // Ack / Result / Error 已在函数开头用独立的 execs 锁处理。
        _ => {}
    }
}

// 这份 tabs 快照是否「明显不可信」。
///
/// 判据：**0 个标签 + 告警里确实有标签枚举失败**。
///
/// 为什么不能只看「0 标签 + 任意告警」：`ext_ready` 的 `warnings` 是**混装**数组
/// （见 `assets/atb_bridge/background.js` 的 `connectWS` finally 块），
/// `clearConnectError` / `scheduleKeepalive` / `clientIdentity` / `queryTabs` /
/// `tabGroups` / `fileSchemeAccess` / `restartSeq` 共用一个数组。用户真的关光了标签时
/// 也常常同时带 `fileSchemeAccess` 之类的无关告警；把那种情况判成「枚举失败」会让
/// 旧会话永远清不掉（`/health.active_tabs` 谎报、空闲自杀判不出来）。
/// 只有 `queryTabs` 这一条（前缀由扩展硬编码为 `'queryTabs: '`）才说明这 0 个标签
/// 是「没枚举成功」而不是「真的没有」。
///
/// `tabs_update` 不带 `warnings` 字段（serde 默认空数组），所以它的空表一律按
/// 真实事实落库——那条路径的 `queryScriptableTabs()` 抛错时根本发不出消息。
fn snapshot_is_invalid(warnings: &[Value], tab_count: usize) -> bool {
    tab_count == 0 && tab_enumeration_failed(warnings)
}

/// 告警里是否有「标签枚举失败」的证据。
///
/// 兼容两种形态：现在的字符串数组（`"queryTabs: <err>"`），以及将来可能的对象形态
/// （按任意字符串字段匹配）。匹配大小写不敏感，避免扩展改了前缀大小写就静默失效。
fn tab_enumeration_failed(warnings: &[Value]) -> bool {
    warnings.iter().any(|warning| match warning {
        Value::String(text) => text.to_ascii_lowercase().contains("querytabs"),
        Value::Object(map) => map
            .values()
            .filter_map(Value::as_str)
            .any(|field| field.to_ascii_lowercase().contains("querytabs")),
        _ => false,
    })
}

// `ext_ready.restartSeq` 的线协议约定。
///
/// 扩展侧 `background.js::ensureRestartSeq` 在每次 Service Worker 冷启动时把
/// `chrome.storage.local.extRestartSeq` +1 并随 `ext_ready` 上报（读失败时上报 0）。
/// daemon 侧必须**记住上一次看到的值**才能区分「真的重启」与「普通重连」
/// （网络抖动/空闲退出后重连，Fetch 域状态可能还在）。
///
/// 重建范围（daemon 侧能力边界，别写空头承诺）：
/// - net 拦截（CDP Fetch 域）的 intent 存在 daemon（`DriverState.net_hooks`），
///   扩展重启后浏览器侧拦截已失效，所以把该 profile 的配置标记
///   `cdpRebuildRequired`，下一次 `net route` / `net routes` 用 `start` 整体重下发；
/// - network / console 抓取的 intent 从来不在 daemon（各自由 CLI 那条命令持有），
// daemon 没有可重建的状态，恢复由扩展侧自己完成，
///   这里**不**假装重建。
pub(crate) fn parse_restart_seq(text: &str) -> Option<(String, String, u32)> {
    let value: Value = serde_json::from_str(text).ok()?;
    // 只认 ext_ready：`restartSeq` 是它的字段，别的消息（result 里的业务数据）
    // 即使撞上同名键也不该被当成"扩展重启"。
    if value.get("type").and_then(Value::as_str) != Some("ext_ready") {
        return None;
    }
    // 只认数字：旧扩展不带该字段，或字段类型不对时一律当作"没上报"。
    let seq = u32::try_from(value.get("restartSeq")?.as_u64()?).ok()?;
    let browser_id = value
        .get("browser_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("browser-default")
        .to_string();
    let profile_id = value
        .get("profile_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("profile-default")
        .to_string();
    Some((browser_id, profile_id, seq))
}

async fn note_extension_restart(state: &AppState, browser_id: &str, profile_id: &str, seq: u32) {
    // key 必须与 net 模块（`net_hooks` 的 `profile_key`）用**同一套净化**口径。
    // 原始 id 里带 `:`/空格时，旧实现会写进一个没人读的 key——
    // 「扩展重启后重建 CDP 拦截」于是静默失效，且日志看不出异常。
    let profile_key = format!(
        "{}:{}",
        sanitize_identity_component(browser_id),
        sanitize_identity_component(profile_id)
    );
    // 序号没变 = 普通重连，不做任何事：无脑重建会让下一次 `net route` 走 `start`，
    // 而 `start` 会清掉扩展侧的拦截日志与统计。
    let changed = {
        let mut seen = state.ext_restart_seq.lock().await;
        match seen.insert(profile_key.clone(), seq) {
            Some(previous) => previous != seq,
            // 首次看到（daemon 刚启动 / 新 profile）：按"可能是重启"处理，
            // 代价只是下一次 route 走一次幂等的 `start`。
            None => true,
        }
    };
    if !changed {
        return;
    }
    let mut driver = state.driver.write().await;
    let Some(config) = driver.net_hooks.get_mut(&profile_key) else {
        return;
    };
    let has_cdp = config.get("cdpEnabled").and_then(Value::as_bool) == Some(true)
        || config
            .get("cdpRules")
            .and_then(Value::as_array)
            .is_some_and(|rules| !rules.is_empty());
    if !has_cdp {
        return;
    }
    config["cdpRebuildRequired"] = json!(true);
    eprintln!(
        "扩展重启（{profile_key} restartSeq={seq}）：浏览器侧 Fetch 拦截已失效，已把 net 规则标记为待重建；下一次 net route / net routes 会整体重下发"
    );
}

/// 应用层心跳（扩展每 30s 发一次 `{"type":"ping"}`）。
///
/// 用解析而不是字符串全等：`JSON.stringify` 的顺序/空白一旦变化（或调用方手写
/// 带空格），全等匹配就静默失效，表现为「服务端 90s 后把健康连接当死连接掐掉」。
/// 限制长度是为了让普通命令（都带 id、且可能包含 "type":"ping" 字样）不受影响。
fn is_app_ping(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() > 64 || !trimmed.starts_with('{') {
        return false;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => {
            value.get("type").and_then(Value::as_str) == Some("ping") && value.get("id").is_none()
        }
        Err(_) => false,
    }
}
