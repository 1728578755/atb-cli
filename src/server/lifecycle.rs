use super::*;

pub(crate) async fn close(
    State(state): State<AppState>,
    Json(req): Json<CloseRequest>,
) -> Json<Value> {
    touch(&state).await;
    let selector = SessionSelector::new(Some(req.tab_id), req.browser, req.profile);
    // 旧实现手写 `{"ok":false,"error":err.to_string()}`，typed 错误的结构化字段
    // （error_code/retryable/hint）在这里被剥掉，CLI 只能靠中文子串映射退出码。
    // 统一走 session_error_json：downcast SessionSelectError 输出稳定码，文本不变。
    let session_key = match select_tab(&state, selector).await {
        Ok(value) => value,
        Err(err) => return Json(session_error_json(&err)),
    };
    let physical_tab_id = match session_tab_id(&state, &session_key).await {
        Ok(value) => value,
        Err(err) => return Json(session_error_json(&err)),
    };
    // `close --tab @N` 是「先解析序号、再逐个关闭」两次请求，中间标签页可能
    // 被打开/关闭/重排。CLI 会把解析时的 url/title 一起带下来，这里在真正关闭前
    // 复核一次：对不上就拒绝，避免关掉用户正在看的另一个标签。
    if req.expect_url.is_some() || req.expect_title.is_some() {
        let current = {
            let driver = state.driver.read().await;
            driver
                .sessions
                .get(&session_key)
                .map(|session| (session.info.url.clone(), session.info.title.clone()))
        };
        if let Some((url, title)) = current {
            let url_ok = req
                .expect_url
                .as_deref()
                .map(|expected| expected.is_empty() || expected == url)
                .unwrap_or(true);
            let title_ok = req
                .expect_title
                .as_deref()
                .map(|expected| expected.is_empty() || expected == title)
                .unwrap_or(true);
            if !url_ok || !title_ok {
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "target_mismatch: 目标标签在解析后已变化（期望 url={:?} title={:?}，当前 url={:?} title={:?}），已跳过关闭",
                        req.expect_url, req.expect_title, url, title
                    ),
                    "error_code": "target_mismatch",
                    "hint": "重新执行 `tabs --ids` 确认目标后再关；`--tab @N` 在标签频繁变动的窗口里不稳定",
                    "expected": { "url": req.expect_url, "title": req.expect_title },
                    "actual": { "url": url, "title": title },
                }));
            }
        }
    }
    let physical_tab_number = match require_tab_id(&physical_tab_id) {
        Ok(value) => value,
        Err(err) => return Json(session_error_json(&err)),
    };
    let payload = json!({
        "cmd": "closeTab",
        "tabId": physical_tab_number,
    })
    .to_string();
    let result = execute_page_js(
        &state,
        &payload,
        SessionSelector::new(Some(session_key.clone()), None, None),
        true,
        Duration::from_secs(15),
    )
    .await;
    match result {
        Ok(value) => {
            // 先让挂在这个标签上的在途执行立刻失败，再回收会话级状态。
            {
                let mut execs = state.execs.lock().await;
                for pending in execs.take_session(&session_key) {
                    // 旧实现裸 anyhow「tab_closed: ...」：调用方只能靠前缀文本判断。
                    // typed 化后错误带 tab_closed 码，同一标签的后续重试可直接放弃。
                    // message 保持原文（含 tab_closed 前缀），向后兼容。
                    let _ = pending.tx.send(Err(tab_closed_error(
                        "tab_closed: 标签页已关闭，命令未完成",
                    )));
                }
            }
            forget_session(&state, &session_key).await;
            Json(json!({ "ok": true, "closed": true, "close_confirmed": true, "result": value }))
        }
        Err(err) => {
            // 失败 ≠ 已关闭。旧实现无条件标 `disconnected_at`，网络抖动/超时时
            // 用户看到「标签未连接」，浏览器里却明明开着；而标签真关掉时下一次
            // tabs_update 也会把不存在的会话标成 disconnected，所以这里保留 active
            // 不会漏掉真实关闭。只有扩展明确回报成功才改状态。
            Json(json!({
                "ok": false,
                "closed": false,
                "close_confirmed": false,
                "session_key": session_key,
                "error": err.to_string(),
                "hint": "关闭未被扩展确认（标签可能仍开着，也可能已关）；用 tabs 复核后再决定是否重试 close"
            }))
        }
    }
}

pub(crate) async fn shutdown(State(state): State<AppState>) -> Json<Value> {
    touch(&state).await;
    let cleanup = cleanup_on_shutdown(&state).await;
    let _ = state.shutdown.send(());
    Json(json!({
        "ok": true,
        "status": "shutdown_requested",
        "cleanup": cleanup
    }))
}

/// 关机清理的单条下发窗口：daemon 正在退出，不值得为一条
/// `debugClearAll` 等满 10s；送不出去只是 `sent:false`，不影响其它标签的清理。
const SHUTDOWN_SEND_WINDOW: Duration = Duration::from_millis(500);

pub(crate) async fn cleanup_on_shutdown(state: &AppState) -> Value {
    // 幂等：idle / inflight-stall / job-stall 都是「先 cleanup 再 send(shutdown)」，
    // graceful_shutdown 闭包与 `/shutdown` handler 还会再调一次。第二遍没有意义，
    // 而且会把扩展侧刚重建的调试/拦截状态再清一遍。
    if state.cleanup_done.swap(true, Ordering::SeqCst) {
        return json!({ "skipped": true, "reason": "already_cleaned" });
    }
    let (snapshot_count, sessions) = {
        let mut driver = state.driver.write().await;
        let snapshot_count = driver.snapshots.len();
        let sessions: Vec<(String, String, tokio::sync::mpsc::Sender<String>)> = driver
            .sessions
            .iter()
            .filter(|(_, session)| session.is_active())
            .map(|(key, session)| (key.clone(), session.tab_id.clone(), session.sender.clone()))
            .collect();
        driver.snapshots.clear();
        (snapshot_count, sessions)
    };
    let (pending_count, active_exec_count, pending) = {
        let mut execs = state.execs.lock().await;
        let pending_count = execs.pending.len();
        let active_exec_count = execs.active.len();
        // 不能只 `clear()` —— 那会让在途命令的 oneshot 发送端随条目一起
        // 被丢弃，调用方只看到泛化的 RecvError/超时，既不知道「是 daemon 关机」，
        // 也无法据此决定重试。drain 后逐个补一条结构化错误再丢弃。
        let pending: Vec<crate::protocol::PendingExec> =
            execs.pending.drain().map(|(_, pending)| pending).collect();
        execs.active.clear();
        execs.acked.clear();
        (pending_count, active_exec_count, pending)
    };
    for item in pending {
        let _ = item.tx.send(Err(daemon_shutdown_error()));
    }

    let mut plugin_results = Vec::new();
    for (session_key, tab_id, sender) in sessions {
        let exec_id = uuid::Uuid::new_v4().to_string();
        // 关闭流程里不能因为一个坏 tab_id 中断其余标签的清理：记为 sent:false 并带上原因。
        let tab_id_number = match parse_tab_id(&tab_id) {
            Ok(value) => value,
            Err(err) => {
                plugin_results.push(json!({
                    "session_key": session_key,
                    "tab_id": tab_id,
                    "sent": false,
                    "error": err.to_string()
                }));
                continue;
            }
        };
        let payload = json!({
            "id": exec_id,
            "code": json!({ "cmd": "debugClearAll" }).to_string(),
            "tabId": tab_id_number
        })
        .to_string();
        // 关机路径不能用默认的 10s 入队超时——daemon 已经在退出，
        // 一条连接等 10s 会让「N 个标签」串成 N×10s。给 500ms 的短窗口：
        // 送不出去就记 `sent:false`，清理本身不因此失败。
        let ok = send_downstream_within(&sender, payload, SHUTDOWN_SEND_WINDOW)
            .await
            .is_ok();
        plugin_results.push(json!({ "session_key": session_key, "tab_id": tab_id, "sent": ok }));
    }

    let (job_total, job_aborted) = {
        let mut store = state.jobs.lock().await;
        let total = store.jobs.len();
        let finished_at = epoch_secs();
        let mut aborted = 0;
        let mut abort_ids: Vec<String> = Vec::new();
        for job in store.jobs.values_mut() {
            if job.status.is_running() {
                aborted += 1;
                job.status = JobStatus::Failed;
                job.finished_at = Some(finished_at);
                // 终态标记：任务协程可能在清理之后才醒来，`run.rs` 的回写路径用
                // `JobRecord::is_aborted_by_shutdown`（按这个 code 判断）保住 Failed，
                // 不会把「daemon 已清理」覆盖成 Succeeded。
                job.error = Some(json!({
                    "code": "daemon_shutdown",
                    "message": "daemon 在任务完成前退出"
                }));
                abort_ids.push(job.id.clone());
            }
        }
        // 标记终态还不够——协程还在跑就会继续占用标签/浏览器，甚至
        // 在清理之后把动作发出去。登记过的 abort 句柄在这里真正终止它。
        for id in abort_ids {
            if let Some(handle) = store.aborts.remove(&id) {
                handle.abort();
            }
        }
        (total, aborted)
    };

    json!({
        "daemon": {
            "snapshots": snapshot_count,
            "pending": pending_count,
            "active_exec_sessions": active_exec_count
        },
        "jobs": {
            "total": job_total,
            "aborted": job_aborted
        },
        "plugin": {
            "sent": plugin_results
        }
    })
}

/// 关机清理给在途命令的结构化错误。
///
/// 用 JSON 字符串承载 `error_code`/`retryable`/`hint`：exec 的 oneshot 只传
/// `anyhow::Error`，CLI 侧会尝试把消息当 JSON 解析（与 `extension_disconnected`
/// 路径同一约定），这样调用方知道「是 daemon 关机，不是命令失败」。
fn daemon_shutdown_error() -> anyhow::Error {
    anyhow!(json!({
        "ok": false,
        "error": "daemon_shutdown: daemon 在命令完成前退出，命令结果未知",
        "error_code": "daemon_shutdown",
        "retryable": true,
        "hint": "daemon 已退出（空闲回收 / 升级 / 手动 stop）；先 atb status 确认已恢复，重发时带上 --idempotency-key 避免重复执行"
    })
    .to_string())
}
