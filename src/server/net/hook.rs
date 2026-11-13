use super::*;

// 旧版本（未按 profile 隔离）使用的固定注入名，仅用于清理残留。
pub(crate) const LEGACY_NET_HOOK_NAME: &str = "__net_hook";

/// 持久化注入名：扩展侧 `chrome.storage.local.persistInjections` 按字符串键全局存，
/// 不分 browser/profile。名字必须带 profile 摘要，否则 B 的 `net hook --persist`
// 会覆盖 A 的条目，A 的 `net reset` 又会删掉 B 的共享条目。
///
/// 用 FNV-1a 而不是 `DefaultHasher`：后者不保证跨 Rust 版本稳定，而这个名字会写进
/// 扩展的持久存储，必须保证 daemon 重启/升级后同一个 profile 还能算出同一个名字。
pub(crate) fn net_hook_name(profile_key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in profile_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("__net_hook_{hash:016x}")
}

/// 把「页面网络库 + 当前规则」注册成持久注入，导航后由 persist_boot 自动重装。
pub(crate) async fn install_net_hook(
    state: &AppState,
    session_key: &str,
    profile_key: &str,
    config: &Value,
) -> Result<Value> {
    let matches: Vec<String> = config
        .get("matches")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // fail-closed：没有 match pattern 就不注册，绝不退回 <all_urls>。
    if matches.is_empty() {
        return Ok(json!({
            "ok": false,
            "action": "persist",
            "skipped": true,
            "name": net_hook_name(profile_key),
            "warning": "没有可用的 match pattern，未注册持久化注入（不会退回 <all_urls>）",
        }));
    }
    let code = format!(
        "{}\n;__abcNet({});",
        html::js_page_net(),
        json!({
            "action": "hook",
            "routes": config.get("routes").cloned().unwrap_or_else(|| json!([])),
            "captureBodies": config.get("captureBodies").cloned().unwrap_or(json!(false)),
            // 再兜一次 clamp（N1-03）：config 可能来自旧版磁盘文件，那里面存的是
            // 未钳制的原始值；这里按页面默认区间收一次，避免整份日志撑爆注入脚本。
            "maxLog": json!(config
                .get("maxLog")
                .and_then(Value::as_i64)
                .unwrap_or(200)
                .clamp(10, 5000)),
        })
    );
    inject_value(
        state,
        InjectRequest {
            action: Some("persist".to_string()),
            code: Some(code),
            name: Some(net_hook_name(profile_key)),
            world: Some("MAIN".to_string()),
            matches: Some(matches),
            frame_id: None,
            all_frames: false,
            switch_tab_id: Some(session_key.to_string()),
            browser: None,
            profile: None,
            // 网络 hook 是基础设施，不是用户调试用的临时注入：不受 --persist TTL 约束。
            ttl_sec: None,
            persist_forever: true,
            userscript: false,
            gm_namespace: None,
            cleanup_selectors: Vec::new(),
            cleanup_code: None,
        },
    )
    .await
}

pub(crate) async fn remove_injection_named(
    state: &AppState,
    session_key: &str,
    name: &str,
) -> Result<Value> {
    inject_value(
        state,
        InjectRequest {
            action: Some("remove".to_string()),
            code: None,
            name: Some(name.to_string()),
            world: None,
            matches: None,
            frame_id: None,
            all_frames: false,
            switch_tab_id: Some(session_key.to_string()),
            browser: None,
            profile: None,
            ttl_sec: None,
            persist_forever: false,
            userscript: false,
            gm_namespace: None,
            cleanup_selectors: Vec::new(),
            cleanup_code: None,
        },
    )
    .await
}

pub(crate) async fn remove_net_hook(
    state: &AppState,
    session_key: &str,
    profile_key: &str,
) -> Result<Value> {
    // 迁移：旧版本所有 profile 共用 `__net_hook`，新代码不会再写入这个名字。
    // 不清理的话它会一直留在 chrome.storage 里，导航后继续重装一个共享 hook，
    // 继续污染别的 profile。删除是 best-effort，失败不影响本次 reset 的结论。
    let _ = remove_injection_named(state, session_key, LEGACY_NET_HOOK_NAME).await;
    remove_injection_named(state, session_key, &net_hook_name(profile_key)).await
}
