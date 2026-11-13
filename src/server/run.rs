use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    pub(crate) fn is_running(self) -> bool {
        matches!(self, JobStatus::Running)
    }
}

/// 后台异步任务：CLI 提交后立即返回 job_id，页面操作在 daemon 内继续执行。
/// 这样智能体不必阻塞等待，用户也可以继续使用浏览器。
pub(crate) struct JobRecord {
    pub(crate) id: String,
    pub(crate) op: String,
    pub(crate) status: JobStatus,
    pub(crate) created_at: f64,
    pub(crate) started_at: Option<f64>,
    pub(crate) finished_at: Option<f64>,
    pub(crate) elapsed_ms: u64,
    // 最近一次「任务状态发生变化」的 epoch 毫秒。
    ///
    /// 单条后台 op 没有细粒度进度通道，所以心跳只在开始 / 取消 / 结束时更新；
    /// `stalled` 只表示「距上次状态变化超过阈值」，不是「页面无响应」的判定。
    pub(crate) last_heartbeat_ms: u64,
    pub(crate) result: Option<Value>,
    pub(crate) error: Option<Value>,
    pub(crate) session_key: Option<String>,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// 当前 epoch 毫秒，统一用 u64（`last_heartbeat` / `ts` 同一单位）。
pub(crate) fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

/// `ATB_JOB_STALL_SEC`：心跳超过该秒数未更新时把 running 任务标记为 stalled。
///
/// `ATB_*` 统一经 `settings.rs` 访问器读取；默认 60 秒。
pub(crate) fn job_stall_sec() -> u64 {
    crate::settings::job_stall_sec()
}

/// 纯函数：running + 心跳年龄超过阈值才算 stalled（终态永不 stalled）。
pub(crate) fn job_is_stalled(
    status: JobStatus,
    last_heartbeat_ms: u64,
    now_ms: u64,
    stall_sec: u64,
) -> bool {
    status.is_running()
        && now_ms.saturating_sub(last_heartbeat_ms) > stall_sec.saturating_mul(1_000)
}

impl JobRecord {
    pub(crate) fn to_json(&self) -> Value {
        let now_ms = epoch_millis();
        let heartbeat_age_ms = now_ms.saturating_sub(self.last_heartbeat_ms);
        json!({
            "id": self.id,
            "op": self.op,
            "status": self.status.as_str(),
            "created_at": self.created_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "elapsed_ms": self.elapsed_ms,
            // 只增字段。last_heartbeat 用 epoch 毫秒，heartbeat_age_ms
            // 由 daemon 计算，调用方不必自己拿本地时钟做差（可能有时区/时钟漂移）。
            "last_heartbeat": self.last_heartbeat_ms,
            "heartbeat_age_ms": heartbeat_age_ms,
            "stalled": job_is_stalled(
                self.status,
                self.last_heartbeat_ms,
                now_ms,
                job_stall_sec()
            ),
            "result": self.result,
            "error": self.error,
            "session_key": self.session_key,
        })
    }

    /// 这条任务是否已经被关机清理判死。
    ///
    /// `cleanup_on_shutdown` 把仍在 Running 的任务写成 `Failed` + `daemon_shutdown`；
    /// 任务协程晚一步醒来时，结果已经没人会读（会话与扩展侧状态都清过了），
    /// 再用它覆盖终态会让 `job get` 显示 succeeded，与实际发生的事相反。
    ///
    /// 判定落在 `error.code` 上而不是新增 `aborted: bool` 字段：`JobRecord` 在
    /// 单测里是整结构体字面量构造的，加字段会牵动测试代码；错误码本身就是
    /// 这条终态的唯一来源（见 `cleanup_on_shutdown`），语义等价且不影响对外 JSON。
    pub(crate) fn is_aborted_by_shutdown(&self) -> bool {
        self.error
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str)
            == Some("daemon_shutdown")
    }
}

#[derive(Default)]
pub(crate) struct JobStore {
    pub(crate) jobs: HashMap<String, JobRecord>,
    /// 强制淘汰存根：硬上限淘汰只打 eprintln 时，调用方拿到的 `job_not_found`
    /// 看起来像 id 写错，会去改一条本来正确的命令。留存根后 `job get/wait` 能拿到
    /// 稳定的 `job_evicted` 语义。存根不含业务结果，只是「这个 id 被 daemon 丢了」的回执。
    pub(crate) evicted: HashMap<String, JobEvictStub>,
    /// 运行中任务的 abort 句柄：`job_id -> AbortHandle`。
    ///
    /// 硬上限淘汰 Running 时，旧实现只是把记录从表里删掉、日志里写「任务继续执行」——
    /// 于是被淘汰的任务既不可查询，又继续占着标签、浏览器与并发额度，直到它自己跑完
    /// （最长 600s）。现在淘汰前先 `abort()`，让协程真的停下来。
    ///
    /// 放在 `JobStore` 而不是 `JobRecord` 里：`JobRecord` 在单测里是整结构体字面量
    /// 构造的，加字段会牵动测试代码；句柄本来也只服务于「淘汰」这一个动作。
    pub(crate) aborts: HashMap<String, tokio::task::AbortHandle>,
}

/// 强制淘汰存根的最小记录。`error_code` 固定为 `job_evicted`，由 `job_get` 拼装响应。
pub(crate) struct JobEvictStub {
    pub(crate) evicted_at: f64,
}

/// 存根上限：只保留最近 32 条。存根没有业务结果，留太久没有查询价值，
/// 不设上限的话长期高并发下存根表自身会变成第二个无界表。
pub(crate) const JOB_EVICT_STUB_LIMIT: usize = 32;

impl JobStore {
    /// 登记一条强制淘汰存根并维持上限：超过 32 条时删掉最早的存根（按 `evicted_at` 升序）。
    fn record_eviction(&mut self, id: String, evicted_at: f64) {
        if self.evicted.len() >= JOB_EVICT_STUB_LIMIT {
            let oldest = self
                .evicted
                .iter()
                .min_by(|left, right| {
                    left.1
                        .evicted_at
                        .partial_cmp(&right.1.evicted_at)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.evicted.remove(&oldest);
            }
        }
        self.evicted.insert(id, JobEvictStub { evicted_at });
    }
}

/// 任务表软上限：超过它就开始按「最旧已结束」淘汰。
pub(crate) const JOB_STORE_SOFT_LIMIT: usize = 100;
/// 任务表硬上限：全是 Running 时也不能无限增长。
///
/// 旧实现只在 `len >= 100` 时从 `values()` 里随便删 20 条（HashMap 无序），
/// 可能删掉刚完成、正等 `job wait` 的新任务，陈旧结果反而留着；
/// 且 >=100 条全 Running 时一条也删不掉，insert 照常 → 表没有上界。
pub(crate) const JOB_STORE_HARD_LIMIT: usize = 500;
/// 一次常规淘汰的批量大小。
pub(crate) const JOB_EVICT_BATCH: usize = 20;

impl JobStore {
    /// 为即将插入的新任务腾空间。
    ///
    /// 排序键固定为 `finished_at.unwrap_or(created_at)` 升序（同值按 id 字典序），
    /// 保证「先完成、最旧」的先走，而不是取决于 HashMap 的迭代顺序。
    /// 已结束的都删完了还到硬上限时，只能强制淘汰最旧的 Running——**先 abort 再删**
    // 。被淘汰的 id 会留
    /// `job_evicted` 存根（见 `record_eviction`），这是表有界的代价，必须显式打日志。
    pub(crate) fn evict_for_insert(&mut self) {
        if self.jobs.len() < JOB_STORE_SOFT_LIMIT {
            return;
        }
        let mut finished: Vec<(f64, String)> = self
            .jobs
            .values()
            .filter(|job| !job.status.is_running())
            .map(|job| (job.finished_at.unwrap_or(job.created_at), job.id.clone()))
            .collect();
        finished.sort_by(|left, right| {
            left.0
                .partial_cmp(&right.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.cmp(&right.1))
        });
        for (_, id) in finished.into_iter().take(JOB_EVICT_BATCH) {
            self.jobs.remove(&id);
            // 已结束的任务句柄不会再做什么，顺手丢掉避免 `aborts` 自己变成泄漏表。
            self.aborts.remove(&id);
        }
        if self.jobs.len() < JOB_STORE_HARD_LIMIT {
            return;
        }
        // 走到这里说明几乎全是 Running（已结束的不足一批，或刚好都被删完）。
        // 强制淘汰：按 started_at/created_at 升序删到硬上限以下，给新任务留一个位置。
        let mut running: Vec<(f64, String)> = self
            .jobs
            .values()
            .filter(|job| job.status.is_running())
            .map(|job| (job.started_at.unwrap_or(job.created_at), job.id.clone()))
            .collect();
        running.sort_by(|left, right| {
            left.0
                .partial_cmp(&right.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.cmp(&right.1))
        });
        let mut forced = 0usize;
        for (_, id) in running {
            if self.jobs.len() < JOB_STORE_HARD_LIMIT {
                break;
            }
            // 淘汰 Running 必须真的 abort。只删记录的话，任务仍在后台跑：
            // 标签被它占着、浏览器还在执行、并发额度也被耗掉，而调用方查不到任何痕迹。
            match self.aborts.remove(&id) {
                Some(handle) => {
                    handle.abort();
                    eprintln!("job: 任务 {id} 因任务表达到硬上限被强制淘汰，已 abort 其协程");
                }
                // 没有句柄（例如进程刚重启、或任务已在收尾）：只能丢记录。
                None => {
                    eprintln!("job: 任务 {id} 因任务表达到硬上限被强制淘汰，但没有可 abort 的句柄")
                }
            }
            self.jobs.remove(&id);
            // 旧实现只打 eprintln：调用方随后 `job get/wait` 只会拿到 job_not_found，
            // 看起来像 id 写错，于是去改一条本来正确的命令。留一条
            // `job_evicted` 存根，把「daemon 侧丢弃」和「id 不存在」区分开。
            self.record_eviction(id, epoch_secs());
            forced += 1;
        }
        if forced > 0 {
            // 消息里的「结果不可再查询」已按 存根语义更新：结果确实拿不回来，
            // 但 `job get/wait <id>` 会命中存根、明确报 job_evicted，不再是 job_not_found。
            eprintln!(
                "job: 任务表达到硬上限 {JOB_STORE_HARD_LIMIT}，已强制淘汰并 abort {forced} 条最旧的 running 任务（查询将返回 job_evicted）"
            );
        }
    }
}

pub(crate) const JOB_OPS: &[&str] = &[
    "exec",
    // 动作类：run_job 只有 "action" 一个实现，但提交侧允许按具体动作名提交，
    // 这样 `--async hover` 与 `hover` 的语义一致，不会报 unsupported op。
    "hover",
    "select",
    "check",
    "uncheck",
    "scroll",
    "focus",
    "scan",
    "read",
    "find",
    "inject",
    "frames",
    "downloads",
    "net",
    "action",
    "wait",
    "cookies",
    "snapshot",
    "click",
    "fill",
    "mouse-click",
    "send-keys",
    "upload",
    "screenshot",
    "save-pdf",
];

// ============================================================================
// 多目标：一次请求打多个标签页
// ============================================================================

pub(crate) async fn run_steps(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Json<Value> {
    touch(&state).await;
    if req.steps.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "run: steps 不能为空",
            "error_code": "empty_steps"
        }));
    }

    let mut results: Vec<Value> = Vec::with_capacity(req.steps.len());
    let mut stopped_at: Option<usize> = None;
    for (index, step) in req.steps.iter().enumerate() {
        let started = Instant::now();
        let outcome = run_job(
            &state,
            &step.op,
            step.args.clone(),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            // 请求成功不等于业务成功：`wait` 未命中、页面脚本 ok:false 都会以 Ok 回来。
            // 只看 Result 会把「等了但没等到」记成 ok:true。
            Ok(value) => {
                let inner_ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                results.push(json!({
                    "index": index,
                    "op": step.op,
                    "ok": inner_ok,
                    "elapsed_ms": elapsed_ms,
                    "result": value
                }));
                if !inner_ok && req.stop_on_error {
                    stopped_at = Some(index);
                    break;
                }
            }
            Err(err) => {
                results.push(json!({
                    "index": index,
                    "op": step.op,
                    "ok": false,
                    "elapsed_ms": elapsed_ms,
                    "error": err.to_string()
                }));
                if req.stop_on_error {
                    stopped_at = Some(index);
                    break;
                }
            }
        }
    }

    let ok = results
        .iter()
        .all(|step| step.get("ok").and_then(Value::as_bool) == Some(true));
    Json(json!({
        "ok": ok,
        "count": results.len(),
        "total": req.steps.len(),
        "stopped_at": stopped_at,
        "steps": results
    }))
}

/// 并行批量执行：一次请求内跑多步，最多 `concurrency` 个并发。
///
/// 与 `/run` 的分工：`/run` 串行、遇错即停，适合有前后依赖的流程；
/// `/batch` 并发，适合跨标签页批量读取/截图/操作——这是大规模处理的主路径。
pub(crate) async fn batch_steps(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Json<Value> {
    touch(&state).await;
    // `/batch` 并发执行，不按步中断；该字段只为兼容 CLI 统一的步骤文件。
    let _ = req.stop_on_error;
    if req.steps.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "batch: steps 不能为空",
            "error_code": "empty_steps"
        }));
    }
    let total = req.steps.len();
    let concurrency = req.concurrency.clamp(1, 32);
    let started = Instant::now();
    let agent = current_agent();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set = tokio::task::JoinSet::new();
    for (index, step) in req.steps.into_iter().enumerate() {
        let state = state.clone();
        let permits = semaphore.clone();
        let agent = agent.clone();
        set.spawn(with_agent(agent, async move {
            // 拿不到许可时不能「无许可继续执行」——那会让 `concurrency` 形同虚设，
            // 几十个步骤一起压浏览器。`.ok()` 把 Err 吞成 None 的旧写法在这里改成显式失败。
            let _guard = match permits.acquire_owned().await {
                Ok(permit) => permit,
                Err(err) => {
                    eprintln!(
                        "batch: 第 {index} 步（{}）并发许可获取失败（semaphore closed），该步未执行: {err}",
                        step.op
                    );
                    return (
                        index,
                        step.op,
                        0u64,
                        Err(anyhow!(
                            "concurrency_closed: 内部并发控制器已关闭，该步骤未执行，可重试"
                        )),
                    );
                }
            };
            let step_started = Instant::now();
            let outcome = run_job(
                &state,
                &step.op,
                step.args,
                Arc::new(AtomicBool::new(false)),
            )
            .await;
            (
                index,
                step.op,
                step_started.elapsed().as_millis() as u64,
                outcome,
            )
        }));
    }
    let mut slots: Vec<Option<Value>> = vec![None; total];
    let mut ok_count = 0usize;
    let mut panicked = 0usize;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((index, op, elapsed_ms, outcome)) => {
                let entry = match outcome {
                    Ok(value) => {
                        let inner_ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                        if inner_ok {
                            ok_count += 1;
                        }
                        json!({ "index": index, "op": op, "ok": inner_ok, "elapsed_ms": elapsed_ms, "result": value })
                    }
                    Err(err) => json!({
                        "index": index, "op": op, "ok": false,
                        "elapsed_ms": elapsed_ms, "error": err.to_string()
                    }),
                };
                if let Some(slot) = slots.get_mut(index) {
                    *slot = Some(entry);
                }
            }
            // JoinError 以前直接丢弃（`Err(_) => panicked += 1`），只剩一个
            // 计数、没有原因。带上 `err` 才能区分「任务 panic」与「运行时取消任务」。
            // 注意 index 随任务一起返回，JoinError 本身不带 index——任务 panic 时
            // 拿不到它，所以日志里只能给错误文本，由 responses 的缺项定位是哪一步。
            Err(err) => {
                eprintln!("batch step panic: {err}");
                panicked += 1;
            }
        }
    }
    let steps: Vec<Value> = slots.into_iter().flatten().collect();
    Json(json!({
        "ok": ok_count == total && panicked == 0,
        "count": steps.len(),
        "total": total,
        "failed": total - ok_count,
        "panicked": panicked,
        "concurrency": concurrency,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "steps": steps
    }))
}

pub(crate) async fn job_submit(
    State(state): State<AppState>,
    Json(req): Json<JobSubmitRequest>,
) -> Json<Value> {
    touch(&state).await;
    let op = req.op.trim().to_string();
    if !JOB_OPS.contains(&op.as_str()) {
        return Json(json!({
            "ok": false,
            "error": format!("unsupported async op: {op}"),
            "error_code": "unsupported_job_op",
            "supported": JOB_OPS
        }));
    }

    let id = Uuid::new_v4().simple().to_string();
    let created_at = epoch_secs();
    let cancel = Arc::new(AtomicBool::new(false));
    let record = JobRecord {
        id: id.clone(),
        op: op.clone(),
        status: JobStatus::Running,
        created_at,
        started_at: None,
        finished_at: None,
        elapsed_ms: 0,
        last_heartbeat_ms: epoch_millis(),
        result: None,
        error: None,
        session_key: None,
        cancel: cancel.clone(),
    };
    {
        let mut store = state.jobs.lock().await;
        // 先限「真正的执行并发」。超限直接拒绝，不 spawn：
        // 无限 spawn 只会把所有任务一起拖慢，最终全部超时，比明确拒绝更难排查。
        let running = store
            .jobs
            .values()
            .filter(|job| job.status.is_running())
            .count();
        let limit = crate::settings::max_running_jobs();
        if running >= limit {
            drop(store);
            eprintln!(
                "job: 拒绝提交 {op}（运行中 {running} 个，上限 {limit}）；可用 ATB_MAX_RUNNING_JOBS 调整"
            );
            return Json(json!({
                "ok": false,
                "error": format!("同时运行的后台任务已达上限（{running}/{limit}）"),
                "error_code": "too_many_running_jobs",
                "error_reason": "job_rejected",
                "limit": limit,
                "running": running,
                "hint": format!("等待已有任务完成（job list 看 running）或 job cancel 取消；上限可用环境变量 ATB_MAX_RUNNING_JOBS 调整（当前 {limit}）"),
            }));
        }
        // 只保留最近一段历史，避免长期运行无限增长（淘汰顺序见 `evict_for_insert`）。
        store.evict_for_insert();
        store.jobs.insert(id.clone(), record);
    }

    let task_state = state.clone();
    let task_id = id.clone();
    let task_op = op.clone();
    let agent = current_agent();
    let handle = tokio::spawn(with_agent(agent, async move {
        let started = Instant::now();
        {
            let mut store = task_state.jobs.lock().await;
            if let Some(job) = store.jobs.get_mut(&task_id) {
                job.started_at = Some(epoch_secs());
                job.last_heartbeat_ms = epoch_millis();
            }
        }
        let outcome = run_job(&task_state, &task_op, req.args, cancel.clone()).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let cancelled = cancel.load(Ordering::Relaxed);
        let outcome_label = if cancelled {
            "cancelled"
        } else if outcome.is_ok() {
            "succeeded"
        } else {
            "failed"
        };
        let mut store = task_state.jobs.lock().await;
        match store.jobs.get_mut(&task_id) {
            // 关机清理已经把这条任务写成 Failed + daemon_shutdown，这里的
            // 结果不再覆盖它（连同 finished_at/elapsed_ms 一起保留清理时的快照）。
            Some(job) if job.is_aborted_by_shutdown() => {
                eprintln!(
                    "job: 任务 {task_id}（{}）在 daemon 关机清理之后才完成，保留 daemon_shutdown 终态，结果丢弃（outcome={outcome_label}）",
                    job.op
                );
            }
            Some(job) => {
                job.finished_at = Some(epoch_secs());
                job.elapsed_ms = elapsed_ms;
                job.last_heartbeat_ms = epoch_millis();
                // 只有「仍在 Running」才允许 cancel 改写终态：cancel 落在任务刚完成的
                // 窗口里时结果已经产生，判成 Cancelled 会把结果整个丢掉。
                if job.status.is_running() && cancelled {
                    job.status = JobStatus::Cancelled;
                    job.error =
                        Some(json!({ "code": "job_cancelled", "message": "job cancelled" }));
                } else {
                    match outcome {
                        Ok(value) => {
                            let inner_ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                            job.session_key = value
                                .get("session_key")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            job.result = Some(value);
                            // `--async wait` 超时以 Ok(ok:false) 返回；以前一律记 Succeeded，
                            // `job wait` 于是报成功（同族）。
                            if inner_ok {
                                job.status = JobStatus::Succeeded;
                            } else {
                                job.status = JobStatus::Failed;
                                job.error = Some(json!({
                                    "code": "job_not_satisfied",
                                    "message": job
                                        .result
                                        .as_ref()
                                        .and_then(|value| value.get("error"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("目标未达成")
                                }));
                            }
                        }
                        Err(err) => {
                            job.status = JobStatus::Failed;
                            job.error =
                                Some(json!({ "code": "job_failed", "message": err.to_string() }));
                        }
                    }
                }
            }
            // 残留：任务表硬上限强制淘汰过这条任务时，`get_mut` 返回 None 后旧实现
            // 静默丢弃结果——调用方只看到 job_evicted，不知道命令到底跑完没有。
            None => {
                eprintln!(
                    "job: 任务 {task_id}（{task_op}）完成时已不在任务表（被强制淘汰或 daemon 正在退出），结果丢弃（outcome={outcome_label}）"
                );
            }
        }
        // 任务已经收尾，abort 句柄没有意义了；不清理的话 `aborts` 会随历史任务无限增长。
        store.aborts.remove(&task_id);
    }));

    // 登记 abort 句柄。放在 spawn 之后、同一把锁内复查状态：
    // 任务可能在 spawn 后瞬间完成并 remove 掉自己的句柄，若不看状态就插入，
    // 这条句柄会永久留在 `aborts` 里。
    {
        let mut store = state.jobs.lock().await;
        if store
            .jobs
            .get(&id)
            .is_some_and(|job| job.status.is_running())
        {
            store.aborts.insert(id.clone(), handle.abort_handle());
        }
    }

    Json(json!({
        "ok": true,
        "job": { "id": id, "op": op, "status": "running", "created_at": created_at },
        "hint": format!("atb job wait {id}")
    }))
}

pub(crate) async fn job_get(
    State(state): State<AppState>,
    AxumPath(job_id): AxumPath<String>,
) -> Json<Value> {
    touch(&state).await;
    let store = state.jobs.lock().await;
    match store.jobs.get(&job_id) {
        Some(job) => Json(json!({ "ok": true, "job": job.to_json() })),
        // 被硬上限强制淘汰的任务：不能回落到 job_not_found（那会被当成 id 写错，
        // 引导调用方去改一条本来正确的命令）。给出稳定的 job_evicted 语义。
        None if store.evicted.contains_key(&job_id) => Json(json!({
            "ok": false,
            "error": format!("job {job_id} 因任务表达到硬上限被强制淘汰"),
            "error_code": "job_evicted",
            "hint": "任务因并发上限被强制淘汰，属 daemon 侧丢弃，不是 id 写错；请降低并发或尽快 wait",
        })),
        None => Json(json!({
            "ok": false,
            "error": format!("job {job_id} 不存在或已过期"),
            "error_code": "job_not_found"
        })),
    }
}

pub(crate) async fn job_list(
    State(state): State<AppState>,
    Query(query): Query<JobListQuery>,
) -> Json<Value> {
    touch(&state).await;
    let limit = query.limit.clamp(1, 200);
    let store = state.jobs.lock().await;
    let mut jobs: Vec<&JobRecord> = store.jobs.values().collect();
    jobs.sort_by(|left, right| {
        right
            .created_at
            .partial_cmp(&left.created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let running = jobs.iter().filter(|job| job.status.is_running()).count();
    let items: Vec<Value> = jobs.iter().take(limit).map(|job| job.to_json()).collect();
    Json(json!({
        "ok": true,
        "count": items.len(),
        "running": running,
        "jobs": items
    }))
}

pub(crate) async fn job_cancel(
    State(state): State<AppState>,
    AxumPath(job_id): AxumPath<String>,
) -> Json<Value> {
    touch(&state).await;
    let mut store = state.jobs.lock().await;
    let Some(job) = store.jobs.get_mut(&job_id) else {
        return Json(json!({
            "ok": false,
            "error": format!("job {job_id} 不存在或已过期"),
            "error_code": "job_not_found"
        }));
    };
    // 只有 Running 的任务才真正接受取消：已结束的任务再 cancel 不能把终态
    // 覆盖成 Cancelled（结果已经产生，丢了就再也拿不回来）。
    // 对终态任务保持幂等：仍返回 ok:true，但明确 cancel_requested:false。
    let running = job.status.is_running();
    if running {
        job.cancel.store(true, Ordering::Relaxed);
        // 取消请求本身也是一次状态变化：刷新心跳，避免 `job tail` 把
        // 「刚请求取消、任务正在收尾」误报成 stalled。
        job.last_heartbeat_ms = epoch_millis();
    }
    Json(json!({
        "ok": true,
        "job_id": job_id,
        "status": job.status.as_str(),
        "cancel_requested": running,
        "note": if running {
            "取消是协作式的：exec 的 --wait-js 轮询与 wait --js 可及时停止；wait --selector/--text 的观察器、scan/read/snapshot 需等当前步骤完成；已发出的页面脚本无法中断"
        } else {
            "任务已结束，取消未生效（状态与结果保持不变）"
        }
    }))
}

pub(crate) async fn run_job(
    state: &AppState,
    op: &str,
    args: Value,
    cancel: Arc<AtomicBool>,
) -> Result<Value> {
    match op {
        "exec" => {
            exec_value(
                state,
                serde_json::from_value::<ExecRequest>(args)?,
                Some(cancel),
            )
            .await
        }
        "scan" => {
            let req: ScanRequest = serde_json::from_value(args)?;
            let timeout = secs(req.timeout.unwrap_or(30.0).clamp(5.0, 300.0));
            scan_page(
                state,
                req.tabs_only,
                SessionSelector::new(req.switch_tab_id, req.browser, req.profile),
                req.text_only,
                req.fast,
                req.max_nodes,
                timeout,
            )
            .await
        }
        "snapshot" => snapshot_page(state, serde_json::from_value(args)?).await,
        "read" => read_value(state, serde_json::from_value(args)?).await,
        "inject" => inject_value(state, serde_json::from_value(args)?).await,
        "frames" => frames_value(state, serde_json::from_value(args)?).await,
        "downloads" => downloads_value(state, serde_json::from_value(args)?).await,
        "net" => net_value(state, serde_json::from_value(args)?).await,
        "find" => find_value(state, serde_json::from_value(args)?).await,
        "wait" => {
            let req: WaitRequest = serde_json::from_value(args)?;
            wait_value(state, req, Some(cancel.as_ref())).await
        }
        "action" | "hover" | "select" | "check" | "uncheck" | "scroll" | "focus" => {
            let mut value = args;
            if op != "action" {
                if let Some(object) = value.as_object_mut() {
                    object.insert("op".to_string(), json!(op));
                }
            }
            action_value(state, serde_json::from_value(value)?).await
        }
        "cookies" => cookies_value(state, serde_json::from_value(args)?).await,
        "click" => {
            target_operation_value(state, OperationKind::Click, serde_json::from_value(args)?).await
        }
        "mouse-click" => {
            target_operation_value(
                state,
                OperationKind::MouseClick,
                serde_json::from_value(args)?,
            )
            .await
        }
        "fill" => fill_value(state, serde_json::from_value(args)?).await,
        "send-keys" => send_keys_value(state, serde_json::from_value(args)?).await,
        "upload" => upload_value(state, serde_json::from_value(args)?).await,
        "screenshot" => screenshot_page(state, serde_json::from_value(args)?).await,
        "save-pdf" => save_pdf_page(state, serde_json::from_value(args)?).await,
        other => Err(anyhow!("unsupported async op: {other}")),
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    // 只有 running 且心跳年龄 **超过** 阈值才算 stalled。
    #[test]
    fn stalled_only_for_running_jobs_over_threshold() {
        // 60s 阈值，心跳 61s 前 = stalled。
        assert!(job_is_stalled(JobStatus::Running, 1_000, 62_000, 60));
        // 恰好 60s 不算「超过」。
        assert!(!job_is_stalled(JobStatus::Running, 1_000, 61_000, 60));
        // 终态永不 stalled，即使心跳很旧。
        assert!(!job_is_stalled(JobStatus::Succeeded, 0, 10_000_000, 60));
        assert!(!job_is_stalled(JobStatus::Failed, 0, 10_000_000, 60));
        assert!(!job_is_stalled(JobStatus::Cancelled, 0, 10_000_000, 60));
    }

    /// 心跳时间在「未来」（时钟回拨）时年龄夹到 0，不能把任务误判成 stalled。
    #[test]
    fn heartbeat_in_the_future_is_not_stalled() {
        assert!(!job_is_stalled(JobStatus::Running, 5_000, 1_000, 60));
    }

    fn job_with_heartbeat(status: JobStatus, last_heartbeat_ms: u64) -> JobRecord {
        JobRecord {
            id: "job-1".to_string(),
            op: "exec".to_string(),
            status,
            created_at: 0.0,
            started_at: Some(0.0),
            finished_at: None,
            elapsed_ms: 0,
            last_heartbeat_ms,
            result: None,
            error: None,
            session_key: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 只增字段：`job get/list` 的 JSON 必须同时给出 last_heartbeat /
    /// heartbeat_age_ms / stalled，且旧字段保持原样。
    #[test]
    fn job_json_exposes_heartbeat_fields() {
        let fresh = job_with_heartbeat(JobStatus::Running, epoch_millis()).to_json();
        assert!(fresh["last_heartbeat"].as_u64().unwrap() > 0);
        assert_eq!(fresh["stalled"], json!(false));
        assert!(fresh["heartbeat_age_ms"].as_u64().is_some());
        // 旧字段仍在。
        assert_eq!(fresh["id"], json!("job-1"));
        assert_eq!(fresh["status"], json!("running"));

        // 心跳停在 epoch 1ms：running 必须被判 stalled。
        let stale = job_with_heartbeat(JobStatus::Running, 1).to_json();
        assert_eq!(stale["stalled"], json!(true));
        // 终态同样保留心跳字段，但不会 stalled。
        let done = job_with_heartbeat(JobStatus::Succeeded, 1).to_json();
        assert_eq!(done["stalled"], json!(false));
    }
}
