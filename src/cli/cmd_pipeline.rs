use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

pub(crate) fn submit_job(op: &str, args: Value) -> Result<Value> {
    // 后台任务走的是同一套参数，必须做和同步路径完全相同的收尾：路径绝对化、
    // 预检、目标解析。否则 `screenshot --async --out shots/a.png` 会被
    // daemon 按自己的 cwd（home）解析，`upload --async` 则拿相对路径直接报不存在。
    let mut args = args;
    if let Err(problem) = finalize_page_payload(op, &mut args) {
        return Ok(problem.to_json());
    }
    // 写类 op 显式目标护栏：与同步路径同一口径，ATB_REQUIRE_TAB=1 时生效。
    enforce_write_tab(op, args.get("switch_tab_id").and_then(Value::as_str))?;
    // 后台任务按「一条命令一个目标」建模；多目标提交会被拆成 N 个 job，
    // 但调用方只拿到一个 id，结果必然对不上。这里直接说清楚怎么改。
    if let Some(tab) = args.get("switch_tab_id").and_then(Value::as_str) {
        if let Some(problem) = async_target_problem(tab)? {
            return Ok(problem.to_json());
        }
    }
    // 防御性第二道闸：所有调用点都应在 `ensure_server()` 之前走
    // [`async_dry_run_plan`]；万一以后新增的调用点漏了那一步，这里也绝不能把动作
    // 真的提交成后台 job——dry-run 的承诺是「不产生任何页面副作用」。
    if DRY_RUN.load(Ordering::Relaxed) {
        return Ok(async_plan_json(op, args));
    }
    request(
        "POST",
        "/job/submit",
        Some(json!({ "op": op, "args": args })),
        15.0,
    )
}

/// `--dry-run` + `--async` 的统一短路，返回 `true` 表示已输出计划、调用方应直接返回。
///
/// 异步路径以前绕过了所有 dry-run 检查：`atb click '#pay' --async --dry-run` 会真的
// 创建 job 并在后台点击。这里在**任何** `ensure_server()` / HTTP 之前
/// 收口，且 args 仍走与真实提交同一份收尾（路径绝对化 + 预检 + 目标语法解析），
/// 避免 dry-run 报「假绿」。
pub(crate) fn async_dry_run_plan(op: &str, payload: &Value, is_async: bool) -> Result<bool> {
    if !is_async || !DRY_RUN.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let mut args = payload.clone();
    if let Err(problem) = finalize_page_payload(op, &mut args) {
        usage_error(&problem)?;
        return Ok(true);
    }
    enforce_write_tab(op, args.get("switch_tab_id").and_then(Value::as_str))?;
    if let Some(tab) = args.get("switch_tab_id").and_then(Value::as_str) {
        if let Some(problem) = async_target_problem(tab)? {
            print_json(problem.to_json());
            return Ok(true);
        }
    }
    print_json(async_plan_json(op, args));
    Ok(true)
}

/// 异步提交的目标形态校验（供 [`submit_job`] 与 [`async_dry_run_plan`] 共用）。
fn async_target_problem(tab: &str) -> Result<Option<preflight::Problem>> {
    // 解析多半已由 finalize_page_payload 完成，这里只判定形态。
    let spec =
        TargetSpec::parse(tab).map_err(|err| usage_failure(err.code, err.message, err.hint))?;
    if spec.is_multi() || spec.needs_daemon_resolution() {
        return Ok(Some(
            preflight::Problem::new(
                "unsupported_combination",
                "--async 不能和 all / @N / url: 这类需要解析的 --tab 一起用",
                "要么去掉 --async（多标签会并发执行并一次返回），要么用明确的 tab id 逐个提交",
            )
            .with_example("atb exec --tab all 'return document.title'"),
        ));
    }
    Ok(None)
}

/// `--dry-run --async` 的计划体：字段名固定为 `would.op` / `would.args`，
/// 与同步 dry-run 的 `args` 同形（都经过绝对化与预检），Agent 可以用同一套解析。
fn async_plan_json(op: &str, args: Value) -> Value {
    let mut args = args;
    redact_plan_script(&mut args);
    json!({
        "ok": true,
        "dry_run": true,
        "async": true,
        "would": {
            "op": op,
            "args": args,
            "endpoint": "POST /job/submit（dry-run 下不会发出）",
        },
        "note": "去掉 --dry-run 才会真的提交 job；args 已按当前工作目录绝对化并通过本地预检",
    })
}

/// 计划里的脚本/代码只回显前 2000 字符：dry-run 输出给人看，几十 KB 的脚本原样
/// 回显没有价值；真正会被 daemon 解释的路径类字段保持完整。
fn redact_plan_script(payload: &mut Value) {
    const PREVIEW_CHARS: usize = 2000;
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    for key in ["script", "code"] {
        let preview = match object.get(key) {
            Some(Value::String(text)) if text.chars().count() > PREVIEW_CHARS => Some((
                text.chars().take(PREVIEW_CHARS).collect::<String>(),
                text.len(),
            )),
            _ => None,
        };
        if let Some((head, bytes)) = preview {
            object.insert(key.to_string(), json!(head));
            object.insert(format!("{key}_bytes"), json!(bytes));
            object.insert(format!("{key}_truncated"), json!(true));
        }
    }
}

pub(crate) fn run_job_command(args: JobCommand) -> Result<()> {
    match args.action {
        JobAction::List(args) => {
            ensure_server()?;
            print_json(request(
                "GET",
                &format!("/job/list?limit={}", args.limit),
                None,
                10.0,
            )?);
        }
        JobAction::Get(args) => {
            ensure_server()?;
            print_json(request(
                "GET",
                &format!("/job/get/{}", query_escape(&args.id)),
                None,
                10.0,
            )?);
        }
        JobAction::Cancel(args) => {
            // `job cancel --dry-run` 以前照样真的发取消请求：取消是不可撤销的
            // 状态变更，必须只输出计划。
            if DRY_RUN.load(Ordering::Relaxed) {
                print_json(json!({
                    "ok": true,
                    "result": {
                        "dry_run": true,
                        "command": "job cancel",
                        "id": args.id,
                        "would": [format!("POST /job/cancel/{}（协作式取消，已在执行的动作可能继续到结束）", args.id)],
                        "note": "去掉 --dry-run 才会真的提交取消",
                    },
                }));
                return Ok(());
            }
            ensure_server()?;
            print_json(request(
                "POST",
                &format!("/job/cancel/{}", query_escape(&args.id)),
                Some(json!({})),
                10.0,
            )?);
        }
        JobAction::Wait(args) => wait_job(&args)?,
        // 2026-11-13：进度跟踪 + 超时 exec 结果取回。
        JobAction::Tail(args) => tail_job(&args)?,
        JobAction::Result(args) => job_result(&args)?,
    }
    Ok(())
}

// `job tail <id>`：轮询 `/job/get/:id` 流式输出状态快照。
///
/// 默认每行一个 JSON，字段固定为
/// `{ts, job_id, state, elapsed_ms, last_heartbeat, stalled[, progress]}`；
/// 状态 / 进度变化时才输出（相同快照按 `--interval` 静默）。
/// 任务结束后输出最终完整结果，并按 succeeded/failed/cancelled 映射 0/1/130。
pub(crate) fn tail_job(args: &JobTailArgs) -> Result<()> {
    if !args.timeout.is_finite() || args.timeout < 0.0 {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "job tail --timeout 必须是非负有限秒数（收到 {}）",
                args.timeout
            ),
            "0 表示一直跟到任务结束（上限 24h）；也可以给 600 这类明确预算",
        ));
    }
    // 0 = 直到结束；上限 24h 防止误写超大值挂死 CLI（与 http::request 的上限一致）。
    let timeout = if args.timeout == 0.0 {
        86_400.0
    } else {
        args.timeout.min(86_400.0)
    };
    let interval = if args.interval.is_finite() {
        args.interval.clamp(0.05, 10.0)
    } else {
        0.5
    };
    ensure_server()?;
    let path = format!("/job/get/{}", query_escape(&args.id));
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    // (state, progress 文本, stalled)：只有这三者变化才输出新快照。
    let mut last_snapshot: Option<(String, String, bool)> = None;
    let mut last_job: Option<Value> = None;

    loop {
        let value = match request("GET", &path, None, 10.0) {
            Ok(value) => value,
            Err(err) => {
                if Instant::now() >= deadline {
                    let message = err.to_string();
                    return tail_timeout(&args.id, timeout, last_job.as_ref(), Some(&message));
                }
                sleep(Duration::from_secs_f64(interval));
                continue;
            }
        };
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            // 响应读不出来（daemon 正在重启 / 404 空 body）是可重试的轮询错误；
            // 其余（job_not_found / job_evicted）原样输出，print_json 映射 exit 2。
            let recoverable = value.get("error_code").and_then(Value::as_str)
                == Some("invalid_response")
                && Instant::now() < deadline;
            if recoverable {
                let remaining = deadline.saturating_duration_since(Instant::now());
                sleep(remaining.min(Duration::from_secs_f64(interval)));
                continue;
            }
            print_json(value);
            return Ok(());
        }
        let job = value.get("job").cloned().unwrap_or(Value::Null);
        last_job = Some(job.clone());
        let state = job
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let progress = job.get("progress").cloned().unwrap_or(Value::Null);
        let stalled = job.get("stalled").and_then(Value::as_bool).unwrap_or(false);
        let snapshot = (state.clone(), progress.to_string(), stalled);
        if last_snapshot.as_ref() != Some(&snapshot) {
            if args.human {
                write_stdout(&job_tail_human_line(&job, &state));
            } else {
                let line = job_tail_line(&job, &args.id, &state, cli_epoch_millis(), &progress);
                write_stdout(&serde_json::to_string(&line).unwrap_or_else(|_| "{}".to_string()));
            }
            last_snapshot = Some(snapshot);
        }
        if state != "running" {
            // 终态：输出完整结果（result/error/session_key 都在里面）。
            print_json(value);
            let code = match state.as_str() {
                "succeeded" => 0,
                "cancelled" => 130,
                _ => EXIT_FAILURE,
            };
            if code != 0 {
                FAILURE_EXIT_CODE.store(code, Ordering::Relaxed);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return tail_timeout(&args.id, timeout, last_job.as_ref(), None);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(remaining.min(Duration::from_secs_f64(interval)));
    }
}

// `job result <exec_id>`：取回 exec 超时后暂存的迟到结果。
///
/// 轮询 `GET /exec-result/:id`：命中即输出 `{ok:true,found:true,result,...}` 并
/// exit 0；等待超时（默认 5s，可 `--wait`）输出 `exec_result_not_found` 且
/// exit 3；`--wait 0` 的立即未命中按普通失败 exit 1。
pub(crate) fn job_result(args: &JobResultArgs) -> Result<()> {
    if !args.wait.is_finite() || args.wait < 0.0 {
        return Err(usage_failure(
            "invalid_argument",
            format!("job result --wait 必须是非负有限秒数（收到 {}）", args.wait),
            "例如 --wait 60；--wait 0 表示只查一次",
        ));
    }
    let wait = args.wait.min(86_400.0);
    let interval = if args.interval.is_finite() {
        args.interval.clamp(0.05, 10.0)
    } else {
        0.5
    };
    ensure_server()?;
    let path = format!("/exec-result/{}", query_escape(&args.id));
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    let mut last_error: Option<String> = None;

    loop {
        let value = match request("GET", &path, None, 10.0) {
            Ok(value) => value,
            Err(err) => {
                // 轮询期传输错误不立刻判死；预算用完时把最后一次错误带进结果。
                last_error = Some(err.to_string());
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                sleep(remaining.min(Duration::from_secs_f64(interval)));
                continue;
            }
        };
        if value.get("found").and_then(Value::as_bool) == Some(true) {
            print_json(value);
            return Ok(());
        }
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            // `invalid_response`（旧 daemon 没有该路由时 axum 回 404 空 body）
            // 属于可重试的轮询错误：等在预算内继续查，到期按「未找到」处理。
            let recoverable = value.get("error_code").and_then(Value::as_str)
                == Some("invalid_response")
                && wait > 0.0
                && Instant::now() < deadline;
            if recoverable {
                last_error = Some(
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("invalid_response")
                        .to_string(),
                );
                let remaining = deadline.saturating_duration_since(Instant::now());
                sleep(remaining.min(Duration::from_secs_f64(interval)));
                continue;
            }
            // 明确的 daemon 错误（token 失配等）：原样输出。
            print_json(value);
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_secs_f64(interval)));
    }

    let ttl_sec = crate::settings::exec_result_ttl_sec();
    let ttl_text = if ttl_sec.is_multiple_of(60) {
        format!("{} 分钟", ttl_sec / 60)
    } else {
        format!("{ttl_sec} 秒")
    };
    let mut out = json!({
        "ok": false,
        "error_code": "exec_result_not_found",
        "error": format!(
            "exec 结果 {} 在 {:.1}s 内未找到（可能从未迟到、已被取走，或超过 TTL 已清理）",
            args.id, wait
        ),
        "hint": format!(
            "确认 exec_id 来自 exec_timeout 错误；迟到结果默认保留 {ttl_text}（ATB_EXEC_RESULT_TTL_SEC 可调）；稍后可重试 atb job result {} --wait 60",
            args.id
        ),
        "exec_id": args.id,
        "wait_sec": wait,
        "ttl_seconds": ttl_sec,
    });
    if let Some(err) = last_error {
        out["last_error"] = json!(err);
    }
    print_json(out);
    // 等待过（--wait > 0）＝ 等待超时 → exit 3；--wait 0 的立即未命中 → exit 1
    // （print_json 已按 ok:false 置 1）。
    if wait > 0.0 {
        FAILURE_EXIT_CODE.store(EXIT_JOB_TIMEOUT, Ordering::Relaxed);
    }
    Ok(())
}

/// 当前 epoch 毫秒（与 daemon 的 `last_heartbeat` 同一单位）。
fn cli_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

/// running 任务的 elapsed 不能只看 daemon 的 `elapsed_ms`（它只在终态写入），
/// 用 `started_at` / `created_at` 结合当前时钟补算。
fn job_elapsed_ms_at(job: &Value, now_ms: u64) -> u64 {
    if let Some(elapsed) = job
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
    {
        return elapsed;
    }
    let started = job
        .get("started_at")
        .and_then(Value::as_f64)
        .or_else(|| job.get("created_at").and_then(Value::as_f64))
        .unwrap_or(0.0);
    let started_ms = (started * 1_000.0).max(0.0) as u64;
    now_ms.saturating_sub(started_ms)
}

/// 一行 `job tail` JSONL 快照。
fn job_tail_line(job: &Value, job_id: &str, state: &str, now_ms: u64, progress: &Value) -> Value {
    let mut line = json!({
        "ts": now_ms,
        "job_id": job_id,
        "state": state,
        "elapsed_ms": job_elapsed_ms_at(job, now_ms),
        "last_heartbeat": job.get("last_heartbeat").cloned().unwrap_or(Value::Null),
        "heartbeat_age_ms": job.get("heartbeat_age_ms").cloned().unwrap_or(Value::Null),
        "stalled": job.get("stalled").and_then(Value::as_bool).unwrap_or(false),
    });
    if !progress.is_null() {
        line["progress"] = progress.clone();
    }
    line
}

/// `--human` 的一行：`[12.3s] running  elapsed=12.3s  heartbeat=2s ago`。
fn job_tail_human_line(job: &Value, state: &str) -> String {
    let elapsed_ms = job_elapsed_ms_at(job, cli_epoch_millis());
    let elapsed_sec = elapsed_ms as f64 / 1_000.0;
    let heartbeat = match job.get("heartbeat_age_ms").and_then(Value::as_u64) {
        Some(age_ms) if age_ms >= 1_000 => format!("{}s ago", age_ms / 1_000),
        Some(age_ms) => format!("{age_ms}ms ago"),
        None => "unknown".to_string(),
    };
    format!("[{elapsed_sec:.1}s] {state}  elapsed={elapsed_sec:.1}s  heartbeat={heartbeat}")
}

/// `job tail` 超时出口：输出 `tail_timeout` 快照并 exit 3。
fn tail_timeout(
    job_id: &str,
    timeout: f64,
    last_job: Option<&Value>,
    last_error: Option<&str>,
) -> Result<()> {
    let mut out = json!({
        "ok": true,
        "tail_timeout": true,
        "job_id": job_id,
        "timeout_sec": timeout,
    });
    if let Some(job) = last_job {
        out["job"] = job.clone();
    }
    if let Some(err) = last_error {
        out["last_error"] = json!(err);
    }
    print_json(out);
    FAILURE_EXIT_CODE.store(EXIT_JOB_TIMEOUT, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn wait_job(args: &JobWaitArgs) -> Result<()> {
    // 负值 / nan / inf 以前被 `.max(0.1)` 静默夹成 0.1s：调用方写 `--timeout -5`
    // 本意是「立刻返回」，实际却报 job_wait_timeout，而退出码/文本都指向「job 太慢」。
    // 这是用法问题，必须在发请求前就以 exit 2 拒绝。
    if !args.timeout.is_finite() || args.timeout <= 0.0 {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "job wait --timeout 必须是大于 0 的有限秒数（收到 {}）",
                args.timeout
            ),
            "例如 --timeout 60；只想探一下状态用 `atb job get <id>`，不要给 0 或负数",
        ));
    }
    ensure_server()?;
    let timeout = args.timeout.max(0.1);
    let deadline = Instant::now() + secs(timeout);
    let interval = secs(args.interval.clamp(0.05, 5.0));
    let path = format!("/job/get/{}", query_escape(&args.id));
    loop {
        // 轮询期的传输错误不能直接冒成 exit 1：调用方会把它当成「执行失败」而不是
        // 「等待超时」，于是重试整条管道。
        let value = match request("GET", &path, None, 10.0) {
            Ok(value) => value,
            Err(err) => {
                if Instant::now() >= deadline {
                    print_json(json!({
                        "ok": false,
                        "error": format!("job {} 在 {}s 内未完成（最后一次轮询失败：{err}）", args.id, timeout),
                        "error_code": "job_wait_timeout",
                    }));
                    return Ok(());
                }
                sleep(interval);
                continue;
            }
        };
        let status = value
            .pointer("/job/status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if status == "unknown" {
            // daemon 不认识这个 job（过期或 id 写错）= 用法问题，退出码 2。
            // 注意要在 print_json 之后设置：print_json 会按 error_code 重算退出码。
            print_json(json!({
                "ok": false,
                "error": format!("job {} 不存在或已过期", args.id),
                "error_code": "job_not_found",
                "hint": "用 `atb job list` 看最近的 job；daemon 只保留最近 100 条",
            }));
            FAILURE_EXIT_CODE.store(EXIT_USAGE, Ordering::Relaxed);
            return Ok(());
        }
        if status != "running" {
            print_json(value);
            // 只有 succeeded 才算成功；cancelled 用 130（与 shell 的 SIGINT 惯例一致），
            // 其余终态（failed）按普通失败处理，交给 run 末尾统一 exit。
            if status != "succeeded" {
                let code = if status == "cancelled" {
                    130
                } else {
                    EXIT_FAILURE
                };
                FAILURE_EXIT_CODE.store(code, Ordering::Relaxed);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            print_json(json!({
                "ok": false,
                "error": format!("job {} 在 {}s 内未完成", args.id, timeout),
                "error_code": "job_wait_timeout",
                "job": value.get("job").cloned().unwrap_or(Value::Null)
            }));
            return Ok(());
        }
        sleep(interval);
    }
}

/// 管道请求的 HTTP 超时预算。
///
/// 旧实现给 `/run`、`/batch` 写死 600s：10 步 × 60s 的管道在 daemon 里还没跑完，
/// CLI 就先断开（reqwest 裸 "operation timed out"，退出码也归不到 job 超时），
/// 而 daemon 仍在后台继续跑——调用方看到的是「失败」，实际是孤儿化。
///
/// 口径：`30s 底座 + Σ(每步 timeout + 5s 余量)`，每步没写 `timeout` 按服务端默认
/// 60s 计，最后夹在 `[60, 3600]`——上界保证一个写错的超大 timeout 不会挂住 CLI。
pub(crate) fn pipeline_http_timeout(steps: &Value) -> f64 {
    // 单步没写 timeout 时的服务端默认值（与 daemon run/batch 的步默认一致）。
    const DEFAULT_STEP_TIMEOUT: f64 = 60.0;
    // 每步的调度/回包余量。
    const STEP_OVERHEAD: f64 = 5.0;
    let mut total = 30.0;
    for step in steps
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let step_timeout = step
            .get("timeout")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(DEFAULT_STEP_TIMEOUT);
        total += step_timeout + STEP_OVERHEAD;
    }
    // 上限必须高于「单步上限 × 合理步数」：`preflight::TIMEOUT_MAX_SECONDS` 是 3600s/步，
    // 原来这里和 `http::request` 都 clamp 到 3600s，等于「只要管道总预算超过一小时，
    // CLI 一定先断」—— 只修一半。24h 只作为防溢出的兜底，不再是实际约束。
    total.clamp(60.0, 86_400.0)
}

/// 合并 `stop_on_error` 的两种来源（步骤文件 / `--continue-on-error`）并回写生效值。
///
// 同一套「文件值优先、显式冲突报错、最终回显」的逻辑以前只写在 `run`
/// 里，`batch` 完全没有，导致两条管道对同一个键的处理完全不同。抽出来共用。
fn apply_stop_on_error(
    object: &mut serde_json::Map<String, Value>,
    continue_on_error: bool,
) -> Result<bool> {
    let file_stop_on_error = object.get("stop_on_error").and_then(Value::as_bool);
    if continue_on_error && file_stop_on_error == Some(true) {
        return Err(usage_failure(
            "unsupported_combination",
            "--continue-on-error（遇错继续）与步骤文件里的 \"stop_on_error\": true（遇错即停）冲突",
            "二者只能留一个：去掉 --continue-on-error，或把步骤文件的 stop_on_error 改成 false / 删掉该键",
        ));
    }
    // 文件给了就以文件为准；否则 `--continue-on-error` 取反（默认遇错即停）。
    let stop_on_error = file_stop_on_error.unwrap_or(!continue_on_error);
    object.insert("stop_on_error".to_string(), json!(stop_on_error));
    Ok(stop_on_error)
}

/// 把「合并后真正生效的 stop_on_error」回显到 `result_meta`，调用方不必自己再推一遍。
fn echo_stop_on_error(value: &mut Value, stop_on_error: bool) {
    if let Some(map) = value.as_object_mut() {
        let meta = map
            .entry("result_meta".to_string())
            .or_insert_with(|| json!({}));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert("stop_on_error".to_string(), json!(stop_on_error));
        }
    }
}

pub(crate) fn cmd_run(args: RunArgs, is_async: bool) -> Result<()> {
    if is_async {
        return Err(usage_failure(
            "unsupported_flag",
            "run 不支持 --async",
            "需要后台执行请用 --async 提交单步任务，或把管道拆成多个 job",
        ));
    }
    let payload = load_steps(&args.file, "run")?;
    // `--dry-run` 以前对 run 完全无效：`atb run steps.json --dry-run` 会真跑完整条管道。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(plan_json("run", &payload));
        return Ok(());
    }
    ensure_server()?;
    let mut payload = payload;
    let Some(object) = payload.as_object_mut() else {
        return Err(usage_failure(
            "invalid_steps",
            "步骤文件必须是对象或数组",
            "顶层写成 {\"steps\":[...]}，或直接给一个数组",
        ));
    };
    // ：文件里的 stop_on_error 以前用 `entry().or_insert()` 写入，
    // 文件给了值就静默盖掉 --continue-on-error。现在 run/batch 共用同一份合并逻辑。
    let stop_on_error = apply_stop_on_error(object, args.continue_on_error)?;
    let http_timeout = pipeline_http_timeout(&payload);
    let mut value = request("POST", "/run", Some(payload), http_timeout)?;
    // 回显生效值：两种来源合并后到底停不停，调用方不必自己按同样的规则再推一遍。
    echo_stop_on_error(&mut value, stop_on_error);
    print_json(value);
    Ok(())
}

pub(crate) fn cmd_batch(args: BatchArgs, is_async: bool) -> Result<()> {
    // `--async` 是全局 flag：`run` 早就显式拒绝，`batch` 却只写 `let _ = is_async`，
    // 于是同一个 flag 在两条管道上行为不同（一条报错、一条静默同步执行）。
    // 这正是 `reject_async_if_set` 要消灭的形态（v8）。
    reject_async_if_set("batch", is_async)?;
    let payload = load_steps(&args.file, "batch")?;
    // 同 run：`--dry-run` 必须只输出计划。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(plan_json("batch", &payload));
        return Ok(());
    }
    ensure_server()?;
    let mut payload = payload;
    let Some(object) = payload.as_object_mut() else {
        return Err(usage_failure(
            "invalid_steps",
            "步骤文件必须是对象或数组",
            "顶层写成 {\"steps\":[...]}，或直接给一个数组",
        ));
    };
    // `batch --concurrency` 以前静默 `clamp(1,32)`，与 scrape 的 exit 2 口径相反。
    // 同一类参数在两条命令上不能一个报错一个改值，否则调用方永远不知道该信哪个。
    let max_parallel = super::cmd_scrape::MAX_PARALLEL;
    if args.concurrency == 0 || args.concurrency > max_parallel {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "batch --concurrency 必须在 1~{max_parallel} 之间（收到 {}）",
                args.concurrency
            ),
            "与 atb scrape 同一上限；过大会让 daemon 一次性开出过多标签页",
        ));
    }
    object.insert("concurrency".to_string(), json!(args.concurrency));
    // batch 也走同一份 stop_on_error 合并（无 --continue-on-error 开关，
    // 文件给了就沿用文件值，缺省仍是遇错即停），并回显生效值。
    let stop_on_error = apply_stop_on_error(object, false)?;
    // 同 run：超时按步骤预算算，不再写死 600s。
    let http_timeout = pipeline_http_timeout(&payload);
    let mut value = request("POST", "/batch", Some(payload), http_timeout)?;
    echo_stop_on_error(&mut value, stop_on_error);
    print_json(value);
    Ok(())
}

/// 读步骤文件并规范化成 `{steps:[...]}`。两个管道命令共用同一套报错。
fn load_steps(path: &Path, op: &str) -> Result<Value> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        usage_failure(
            "file_not_found",
            format!("读取步骤文件失败: {} ({err})", path.display()),
            "确认路径存在；相对路径按当前工作目录解析",
        )
    })?;
    let mut payload: Value = serde_json::from_str(&raw).map_err(|err| {
        usage_failure(
            "invalid_steps",
            format!("步骤文件不是合法 JSON: {} ({err})", path.display()),
            "用 `atb run --help` 看步骤格式，或先 `atb run steps.json --dry-run` 校验",
        )
    })?;
    if payload.is_array() {
        payload = json!({ "steps": payload });
    }
    if payload.get("steps").and_then(Value::as_array).is_none() {
        return Err(usage_failure(
            "invalid_steps",
            format!("{op}: 步骤文件缺少 steps 数组"),
            "顶层写成 {\"steps\":[{\"op\":\"read\",\"args\":{}}]}",
        ));
    }
    Ok(payload)
}

/// `--dry-run` 的计划输出：字段与真实执行保持一致，Agent 可以直接用同一套解析。
///
/// **每一步都要真的跑一遍 preflight**。`--dry-run` 的对外承诺是「预检失败时命令不会
/// 发到浏览器」，而旧实现只回显 `op`/`args`：选择器写错、op 不认识时照样返回
/// `ok:true`，真实执行才失败——「假绿」比不检查更糟（v8）。
fn plan_json(op: &str, payload: &Value) -> Value {
    let mut steps: Vec<Value> = Vec::new();
    let mut first_problem: Option<Value> = None;
    if let Some(items) = payload.get("steps").and_then(Value::as_array) {
        for (index, step) in items.iter().enumerate() {
            // 步骤形状必须在本地校验：以前只要 preflight 放行就 ok:true，
            // 非对象 step / 缺 op / 未知 op 都会在 dry-run 里「假绿」，真跑时 daemon
            // 直接 422 反序列化失败，调用方根本不知道为什么。
            let mut step_op = String::new();
            let mut step_args = json!({});
            // 每个分支都必定赋值，先不初始化：避免 clippy 的 unused_assignments。
            let problem: Option<preflight::Problem>;
            match step.as_object() {
                None => {
                    problem = Some(
                        preflight::Problem::new(
                            "invalid_steps",
                            format!(
                                "第 {} 步不是 JSON 对象（收到 {}）",
                                index + 1,
                                json_kind(step)
                            ),
                            "每一步写成 {\"op\":\"read\",\"args\":{...}}",
                        )
                        .with_field("steps"),
                    );
                }
                Some(map) => {
                    step_op = map
                        .get("op")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    step_args = map.get("args").cloned().unwrap_or_else(|| json!({}));
                    if step_op.is_empty() {
                        problem = Some(
                            preflight::Problem::new(
                                "invalid_steps",
                                format!("第 {} 步缺少 op", index + 1),
                                format!(
                                    "每个步骤必须写 op；可用：{}",
                                    crate::server::JOB_OPS.join(", ")
                                ),
                            )
                            .with_field("steps"),
                        );
                    } else if !crate::server::JOB_OPS.contains(&step_op.as_str()) {
                        problem = Some(
                            preflight::Problem::new(
                                "unsupported_op",
                                format!("第 {} 步的 op {step_op:?} 不合法", index + 1),
                                format!(
                                    "可用 op：{}；查看某一步的参数用 atb schema",
                                    crate::server::JOB_OPS.join(", ")
                                ),
                            )
                            .with_field("steps"),
                        );
                    } else if !step_args.is_object() {
                        problem = Some(
                            preflight::Problem::new(
                                "invalid_steps",
                                format!(
                                    "第 {} 步的 args 必须是 JSON 对象（收到 {}）",
                                    index + 1,
                                    json_kind(&step_args)
                                ),
                                "写成 \"args\":{...}；没有参数时写 \"args\":{} 或省略",
                            )
                            .with_field("steps"),
                        );
                    } else {
                        problem = preflight::preflight_payload(&step_op, &step_args).err();
                    }
                }
            }
            if let Some(problem) = &problem {
                if first_problem.is_none() {
                    first_problem = Some(problem.to_json());
                }
            }
            steps.push(json!({
                "index": index,
                "op": step_op,
                "args": step_args,
                "ok": problem.is_none(),
                "problem": problem.map(|problem| problem.to_json()).unwrap_or(Value::Null),
            }));
        }
    }
    let mut plan = json!({
        "ok": first_problem.is_none(),
        "dry_run": true,
        "op": op,
        "count": steps.len(),
        "total": steps.len(),
        "stopped_at": Value::Null,
        "steps": steps,
    });
    if let Some(problem) = first_problem {
        // 顶层带上第一个问题，调用方不必自己遍历 steps 就能拿到 error_code/hint。
        plan["error_code"] = problem
            .get("error_code")
            .cloned()
            .unwrap_or_else(|| json!("invalid_steps"));
        plan["error"] = problem.get("error").cloned().unwrap_or(Value::Null);
        if let Some(hint) = problem.get("hint") {
            plan["hint"] = hint.clone();
        }
        plan["problem"] = problem;
    }
    plan
}

/// 给「步骤不是对象」之类的类型错误一个人读的类型名。
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `DRY_RUN` 是进程级全局量：多个测试并行时各自 `store(true)` 再「恢复原值」
    /// 会互相覆盖（恢复出的 `true` 会漏给后面的测试）。这里串行化并固定收尾为
    /// `false`，不依赖任何「进入时的值」。
    fn with_dry_run<T>(f: impl FnOnce() -> T) -> T {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
        DRY_RUN.store(true, Ordering::Relaxed);
        let out = f();
        DRY_RUN.store(false, Ordering::Relaxed);
        drop(guard);
        out
    }

    /// 计划体必须带齐四个判据字段，且 args 原样透出（Agent 用同一套解析）。
    #[test]
    fn async_plan_json_has_contract_shape() {
        let plan = async_plan_json("click", json!({ "target": "#go" }));
        assert_eq!(plan["ok"], json!(true));
        assert_eq!(plan["dry_run"], json!(true));
        assert_eq!(plan["async"], json!(true));
        assert_eq!(plan["would"]["op"], json!("click"));
        assert_eq!(plan["would"]["args"]["target"], json!("#go"));
        // 一旦计划体被误改回「真的提交」，endpoint 会指向真实路径而不是这句说明。
        assert!(plan["would"]["endpoint"]
            .as_str()
            .unwrap()
            .contains("dry-run"));
    }

    /// 长脚本只回显预览，但字节数必须保留，调用方才知道自己传了多大的东西。
    #[test]
    fn async_plan_json_truncates_long_script() {
        let plan = async_plan_json("exec", json!({ "script": "x".repeat(5000) }));
        let preview = plan["would"]["args"]["script"].as_str().unwrap();
        assert_eq!(preview.chars().count(), 2000);
        assert_eq!(plan["would"]["args"]["script_bytes"], json!(5000));
        assert_eq!(plan["would"]["args"]["script_truncated"], json!(true));
    }

    /// ：`--dry-run --async` 必须在发请求前短路。
    #[test]
    fn async_dry_run_plan_short_circuits() {
        let handled =
            with_dry_run(|| async_dry_run_plan("click", &json!({ "target": "#go" }), true));
        assert!(handled.unwrap(), "dry-run + async 必须由计划路径接管");
    }

    /// 同步路径仍由 `dispatch_page` 的 dry-run 分支处理，不能被这个助手抢走。
    #[test]
    fn async_dry_run_plan_leaves_sync_path_alone() {
        let handled =
            with_dry_run(|| async_dry_run_plan("click", &json!({ "target": "#go" }), false));
        assert!(!handled.unwrap());
    }

    /// 防御性第二道闸：即使调用点忘了先走计划路径，submit_job 也不能真提交。
    #[test]
    fn submit_job_returns_plan_under_dry_run() {
        let value = with_dry_run(|| submit_job("scan", json!({ "timeout": 30.0 }))).unwrap();
        assert_eq!(value["dry_run"], json!(true));
        assert_eq!(value["async"], json!(true));
        assert_eq!(value["would"]["op"], json!("scan"));
    }

    /// 多目标 + async 在计划阶段就要拒绝（否则 dry-run 假绿、真跑 exit 2）。
    #[test]
    fn async_dry_run_plan_rejects_multi_target() {
        let handled = with_dry_run(|| {
            async_dry_run_plan(
                "click",
                &json!({ "target": "#go", "switch_tab_id": "all" }),
                true,
            )
        });
        assert!(handled.unwrap());
    }

    // running 任务用 started_at 补算 elapsed_ms；终态优先用 daemon 的 elapsed_ms。
    #[test]
    fn job_elapsed_ms_handles_running_and_finished() {
        let running = json!({ "started_at": 100.0, "elapsed_ms": 0 });
        assert_eq!(job_elapsed_ms_at(&running, 105_500), 5_500);
        let finished = json!({ "started_at": 100.0, "elapsed_ms": 7_000 });
        assert_eq!(job_elapsed_ms_at(&finished, 200_000), 7_000);
        // 时钟回拨（now < started）夹到 0。
        let skewed = json!({ "started_at": 500.0 });
        assert_eq!(job_elapsed_ms_at(&skewed, 1_000), 0);
    }

    // tail 快照字段齐全；progress 存在时才附加。
    #[test]
    fn job_tail_line_shape() {
        let job = json!({
            "status": "running",
            "started_at": 100.0,
            "elapsed_ms": 0,
            "last_heartbeat": 100_000,
            "heartbeat_age_ms": 2_000,
            "stalled": false,
            "progress": { "step": 2, "total": 5 },
        });
        let line = job_tail_line(&job, "job-1", "running", 105_500, &job["progress"]);
        assert_eq!(line["ts"], json!(105_500));
        assert_eq!(line["job_id"], json!("job-1"));
        assert_eq!(line["state"], json!("running"));
        assert_eq!(line["elapsed_ms"], json!(5_500));
        assert_eq!(line["last_heartbeat"], json!(100_000));
        assert_eq!(line["stalled"], json!(false));
        assert_eq!(line["progress"]["step"], json!(2));

        let no_progress = job_tail_line(&job, "job-1", "running", 105_500, &Value::Null);
        assert!(no_progress.get("progress").is_none());
    }

    // 人读行包含 elapsed 与 heartbeat 年龄。
    #[test]
    fn job_tail_human_line_contains_heartbeat() {
        let job = json!({
            "started_at": 100.0,
            "elapsed_ms": 12_300,
            "heartbeat_age_ms": 2_000,
        });
        let line = job_tail_human_line(&job, "running");
        assert!(line.starts_with("[12.3s] running"));
        assert!(line.contains("elapsed=12.3s"));
        assert!(line.contains("heartbeat=2s ago"));
    }
}
