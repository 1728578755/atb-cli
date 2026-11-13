use super::*;

/// 当前标签所属站点的 content-script 匹配模式：`*://host/*`。
///
/// 只在没有显式 `--match` 时用来收窄持久化注入，避免 `net hook --persist`
// 把脚本注册到所有网站。第二个返回值是「端口未能保留」的告警。
pub(crate) async fn current_site_match_pattern(
    state: &AppState,
    session_key: &str,
) -> Option<(String, Option<String>)> {
    let driver = state.driver.read().await;
    let url = driver
        .sessions
        .get(session_key)
        .map(|session| session.info.url.clone())
        .unwrap_or_default();
    drop(driver);
    site_match_pattern_with_warning(&url)
}

/// 从 URL 提取 content-script match pattern；非 http(s) 页面返回 `None`。
///
// 以前手写切分有两个 bug：`https://example.com?q=1` 会把 query 当成 host，
/// 生成非法 pattern；`host.starts_with("chrom")` 又会误杀 `chromium.org`。
/// 现在交给 `url` crate 解析，过滤条件改为按 scheme 判断。
pub(crate) fn site_match_pattern(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    // Chrome 的 match pattern 不允许带端口（`host_str` 已剥掉端口）；
    // IPv6 必须保留方括号，`host_str` 是否带括号随 url 版本而变，这里统一兜住。
    let host = if host.starts_with('[') || !host.contains(':') {
        host
    } else {
        format!("[{host}]")
    };
    Some(format!("*://{host}/*"))
}

// 同 [`site_match_pattern`]，额外返回「端口未能保留」的告警。
///
/// Chrome 的 match pattern 语法里 host 段不接受 `host:port`（注册时整条会被拒），
/// pattern 里没有任何办法表达「只在这个端口生效」。所以持久注入只能按 host 注册，
/// 同 host 的其它端口也会被注入 —— 这是**协议级限制**，不是 daemon 的取舍。
///
/// 方案上不再给配置塞一个 config 级 `port`（net-hooks.json 是 per-profile 的多 host/
/// 多规则集合，单一 port 会让「先 hook :8080、再给 :443 加规则」被静默跳过，比跨端口
/// 污染更难排查）。需要严格按端口隔离时，调用方有两条明确通道：
/// - `net hook --persist --match *://host:port/*` 之外的显式 pattern（自担语义），
/// - `net route --engine cdp`（按 URL 正则拦截，可带端口）。
///
/// 这里只如实告警，让调用方知道「注册范围比想要的范围宽」。
pub(crate) fn site_match_pattern_with_warning(raw_url: &str) -> Option<(String, Option<String>)> {
    let parsed = url::Url::parse(raw_url).ok()?;
    let pattern = site_match_pattern(raw_url)?;
    let warning = parsed.port().map(|port| {
        format!(
            "当前页面端口是 {port}，但 Chrome 的 match pattern 语法不支持端口，持久注入只能按 `{pattern}` 注册；\
             同一 host 的其它端口也会被注入（协议级限制，无法在 pattern 里表达）。\
             需要按端口隔离时请用 `--match` 显式指定，或改用 `net route --engine cdp`（按 URL 正则，可带端口）"
        )
    });
    Some((pattern, warning))
}

/// net 配置的键：同一 profile 下的所有标签共用一套规则。
pub(crate) async fn net_profile_key(state: &AppState, session_key: &str) -> Result<String> {
    let driver = state.driver.read().await;
    let session = driver
        .sessions
        .get(session_key)
        .ok_or_else(|| anyhow!("会话ID {session_key} 未连接"))?;
    Ok(format!("{}:{}", session.browser_id, session.profile_id))
}

pub(crate) fn net_config_default() -> Value {
    json!({
        "persist": false,
        "matches": ["<all_urls>"],
        "captureBodies": false,
        "maxLog": 200,
        "routes": [],
        "cdpRules": [],
        "cdpEnabled": false,
    })
}

// 取某个 profile 的落盘锁。
///
/// 旧实现是一把**全局**单锁：两个 profile 的并发 net 命令排在同一条队列上，
/// 每条最多等 `NET_RMW_LOCK_WAIT`（150s），叠加后调用方的请求预算必然先耗尽。
/// 现在按 `profile_key` 分配：先用**瞬时**的 map 锁取/建各自的 `Arc<Mutex<()>>`
/// 后立刻释放 map（map 锁不跨 await 持有），再只等自己那一把 —— 不同 profile
/// 的落盘完全并行，同一 profile 仍严格串行。
///
/// map 项按 profile 数量增长（浏览器 + profile 的笛卡尔积量级），不做回收：
/// 回收会引入「刚取到 Arc 就被移除导致两把锁并存」的竞态，不值得。
pub(crate) async fn profile_persist_lock(
    state: &AppState,
    profile_key: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    let handle = {
        let mut map = state.net_persist_lock.lock().await;
        map.entry(profile_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    handle.lock_owned().await
}

/// 把某个 profile 的 net 配置写入内存并落盘。
///
// **前提**：调用方已持有该 profile 的落盘锁（[`profile_persist_lock`]， 的
/// 读-改-写串行化）。这里不再自己拿锁：`net_value` 在整个「读 → 下发 → 写」期间
/// 持锁，若这里再锁一次就是自死锁。落盘只带当前 profile 的条目，其余 profile 由
/// 后台线程从磁盘读回后合并：旧实现克隆**所有** profile（mock body 可达 2MB）
// 再同步写盘，全部发生在 async worker 上，配置一多就会卡住整个 runtime。
pub(crate) async fn store_net_config_locked(state: &AppState, profile_key: &str, config: &Value) {
    {
        let mut driver = state.driver.write().await;
        driver
            .net_hooks
            .insert(profile_key.to_string(), config.clone());
    }
    let entry = config.clone();
    let key = profile_key.to_string();
    // 读-改-写必须串行：两个 profile 并发保存时，后写的那份不能把先写的覆盖掉。
    // （同一个 profile 由 [`profile_persist_lock`] 串行，不同 profile 之间本来就不
    // 共享键；两侧都遵守锁序，不会互相覆盖。）
    let outcome = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut map = config::load_net_hooks()?;
        map.insert(key, entry);
        config::save_net_hooks(&map)
    })
    .await;
    // 落盘失败不能让命令失败：net 规则不是安全配置，只提示一行。
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            eprintln!("net: 保存 net-hooks.json 失败（本次配置仍生效，daemon 重启后会丢）: {err}");
        }
        Err(err) => eprintln!("net: 保存 net-hooks.json 的后台任务失败: {err}"),
    }
}

/// 删除某个 profile 的 net 规则（内存 + 磁盘），供 `net reset` 使用。
///
/// **前提**：调用方已持有该 profile 的落盘锁（同 `store_net_config_locked`）。
pub(crate) async fn remove_net_config_locked(state: &AppState, profile_key: &str) {
    {
        let mut driver = state.driver.write().await;
        driver.net_hooks.remove(profile_key);
    }
    let key = profile_key.to_string();
    let outcome = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut map = config::load_net_hooks()?;
        map.remove(&key);
        config::save_net_hooks(&map)
    })
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            eprintln!("net: 保存 net-hooks.json 失败（reset 后重启可能读到旧规则）: {err}");
        }
        Err(err) => eprintln!("net: 保存 net-hooks.json 的后台任务失败: {err}"),
    }
}

#[cfg(test)]
mod net_settings_tests {
    use super::*;

    // match pattern 表达不了端口，所以只能告警；默认端口（url crate 归一化后
    /// `port()==None`）不该告警。
    #[test]
    fn site_pattern_reports_explicit_port_only() {
        let (pattern, warning) =
            site_match_pattern_with_warning("https://example.com:8443/a/b?q=1").unwrap();
        assert_eq!(pattern, "*://example.com/*");
        assert!(warning.as_deref().unwrap_or_default().contains("8443"));

        let (pattern, warning) = site_match_pattern_with_warning("https://example.com/a").unwrap();
        assert_eq!(pattern, "*://example.com/*");
        assert!(warning.is_none());

        let (pattern, warning) = site_match_pattern_with_warning("http://127.0.0.1:8080/").unwrap();
        assert_eq!(pattern, "*://127.0.0.1/*");
        assert!(warning.as_deref().unwrap_or_default().contains("8080"));
    }

    /// 只接受 http/https：其它 scheme 不该生成 pattern（fail-closed）。
    #[test]
    fn site_pattern_rejects_non_http() {
        assert!(site_match_pattern("chrome-extension://abc/page.html").is_none());
        assert!(site_match_pattern("about:blank").is_none());
    }
}
