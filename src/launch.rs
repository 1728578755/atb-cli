//! 用指定参数启动浏览器，专门解决一件事：**「已开始调试此浏览器」横幅**。
//!
//! Chrome 只要被 `chrome.debugger.attach` 附加，就会在页面顶部挂一条
//! 「「ATB」已开始调试此浏览器」。它挡内容、暴露自动化，而且**没法在运行时去掉**——
//! 唯一开关是启动参数 `--silent-debugger-extension-api`（Chrome 156 的二进制里仍然存在，
//! 实测有效：同一次 attach，带参数无横幅、不带参数有横幅）。
//!
//! 命令行参数只在进程启动时生效，对已经在跑的浏览器加不了。所以这里做的是
//! 「按正确参数重新拉起一个浏览器」，并且把它做得足够安全：
//! - 默认**不动**任何已在运行的实例，直接给出可执行的提示；
//! - 只有显式 `--restart` 才会温和关闭（`taskkill` 不带 `/F`，等同点窗口的关闭按钮），
//!   并且会轮询确认进程真的退出后再启动，避免新进程把请求转发给旧实例、参数静默失效；
//! - 启动后如果进程立刻退出，判定为「参数没生效」并报错，而不是假装成功。

use anyhow::{anyhow, Context, Result};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 外部命令（tasklist / powershell / ps / which）的最长等待时间。
///
/// 这些命令都在「动手杀进程 / 确认进程是否还在」的关键路径上：一旦卡住（WMI 卡死、
// 杀软拦截），调用方会无限期挂着。默认 5s 上限，超时就当查询失败处理。
// 可用 `ATB_PROC_CMD_TIMEOUT_MS` 调整。
fn command_timeout() -> Duration {
    let millis = crate::settings::proc_cmd_timeout_ms();
    Duration::from_millis(millis)
}

/// 启动后等待「新进程是否还活着」的时间。
///
/// 旧实现硬编码 900ms：慢机 / 冷启动下浏览器还没完成初始化就被判定为「参数没生效」，
/// 或者反之太快把转发误判成失败。默认 800ms，可用 `ATB_LAUNCH_SETTLE_MS` 调整
// 。
fn launch_settle() -> Duration {
    let millis = crate::settings::launch_settle_ms();
    Duration::from_millis(millis)
}

/// 温和关闭后等待进程退出的时间（默认 3s）。
fn close_wait_budget() -> Duration {
    let millis = crate::settings::launch_close_wait_ms();
    Duration::from_millis(millis)
}

/// 强杀后等待进程退出的时间（默认 8s）。
fn kill_wait_budget() -> Duration {
    let millis = crate::settings::launch_kill_wait_ms();
    Duration::from_millis(millis)
}

/// 关闭实例时轮询 `instances()` 的间隔（默认 500ms）。
fn close_poll_interval() -> Duration {
    let millis = crate::settings::launch_poll_ms();
    Duration::from_millis(millis)
}

/// 带超时地跑一个外部命令。
///
/// 用 `try_wait` + 轮询实现（不引入新依赖）：超时立刻 `kill` 并回收子进程，返回
/// `None`；正常退出则把 stdout/stderr 收齐。
///
/// stdout/stderr 各由一个读取线程收走：PowerShell 在进程很多的机器上可能输出几十 KB，
/// 超过管道缓冲区后子进程会阻塞在 write 上、永远不退出，单线程「先等退出再读」会假超时。
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    use std::io::Read;

    fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_all(stdout_pipe));
    let stderr_reader = std::thread::spawn(move || read_all(stderr_pipe));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                break None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    match status {
        Some(status) => Some(std::process::Output {
            status,
            stdout: stdout_reader.join().unwrap_or_default(),
            stderr: stderr_reader.join().unwrap_or_default(),
        }),
        None => {
            // 超时/查询失败：回收子进程即可，读取线程会随着管道关闭自行退出
            // （不 join，避免子进程树里还有活着的孙进程时把调用方再次卡住）。
            let _ = child.wait();
            None
        }
    }
}

/// 关掉调试横幅的启动开关。Chrome / Edge 都认（同一个 Chromium 开关）。
pub const SILENT_DEBUG_SWITCH: &str = "--silent-debugger-extension-api";

/// 浏览器种类：决定可执行文件、默认用户数据目录、进程名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chrome,
    Edge,
    Chromium,
}

impl Kind {
    pub fn parse(raw: &str) -> Result<Kind> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chrome" | "google-chrome" | "googlechrome" => Ok(Kind::Chrome),
            "edge" | "msedge" | "microsoft-edge" => Ok(Kind::Edge),
            "chromium" => Ok(Kind::Chromium),
            other => Err(anyhow!(
                "不认识 --browser {other}（可选 chrome / edge / chromium）"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Chrome => "chrome",
            Kind::Edge => "edge",
            Kind::Chromium => "chromium",
        }
    }

    /// 进程名（Windows 的 Win32_Process.Name；Unix 走 ps 的完整命令行，不用它）
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn process_name(self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            match self {
                Kind::Chrome => "chrome.exe",
                Kind::Edge => "msedge.exe",
                // Chromium 的二进制在 Windows 上也叫 chrome.exe
                // （`resolve_exe` 打开的是 `Chromium\Application\chrome.exe`）。
                // 这里返回 chromium.exe 会让 `instances` 永远空。
                Kind::Chromium => "chrome.exe",
            }
        }
        #[cfg(target_os = "macos")]
        {
            match self {
                Kind::Chrome => "Google Chrome",
                Kind::Edge => "Microsoft Edge",
                Kind::Chromium => "Chromium",
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match self {
                Kind::Chrome => "chrome",
                Kind::Edge => "msedge",
                Kind::Chromium => "chromium",
            }
        }
    }
}

/// 一个已经在运行的浏览器实例。
#[derive(Debug, Clone)]
pub struct Instance {
    pub pid: u32,
    pub command: String,
}

/// 启动计划：先算清楚要做什么，再执行。`--dry-run` 就只输出这个。
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: Kind,
    pub exe: PathBuf,
    pub user_data_dir: PathBuf,
    /// 正在占用这个 profile 的实例（`--restart` 时会被温和关闭，否则只用来报错）
    pub running: Vec<Instance>,
    /// 这些实例是不是**已经**带静默开关在跑（命令行里能看到），是就不用再重启一次
    pub already_silent: bool,
    /// 完整启动参数（不含 exe 本身）
    pub args: Vec<String>,
}

/// 算出启动计划。默认不关闭任何实例，只在 `--restart` 时列进 `close`。
pub fn plan(
    kind: Kind,
    user_data_dir: Option<&Path>,
    restart: bool,
    extra_args: &[String],
    urls: &[String],
    restore_session: bool,
) -> Result<Plan> {
    let exe = resolve_exe(kind)?;
    let dir = match user_data_dir {
        Some(path) => absolutize(path)?,
        None => default_user_data_dir(kind)?,
    };
    let default_dir = default_user_data_dir(kind).ok();
    let running = instances(kind)?;
    // 「占着这个 profile」的实例：带 --user-data-dir 且路径相同，或没带这个参数
    // （= 用默认目录，仅当我们要用的就是默认目录时才算冲突）。
    let using: Vec<Instance> = running
        .into_iter()
        .filter(|item| uses_dir(&item.command, &dir, default_dir.as_deref()))
        .collect();

    let mut args: Vec<String> = vec![
        format!("--user-data-dir={}", dir.display()),
        SILENT_DEBUG_SWITCH.to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    if restart && restore_session {
        args.push("--restore-last-session".to_string());
    }
    args.extend(extra_args.iter().cloned());
    args.extend(urls.iter().cloned());

    let already_silent = !using.is_empty()
        && using
            .iter()
            .all(|item| item.command.contains(SILENT_DEBUG_SWITCH));
    Ok(Plan {
        kind,
        exe,
        user_data_dir: dir,
        running: using,
        already_silent,
        args,
    })
}

/// `execute` 的结果。
#[derive(Debug, Clone)]
pub struct Executed {
    pub pid: u32,
    /// 已在运行的实例接走了这次启动的 URL（新进程只是转发后退出），参数没有生效但
    /// URL 确实打开了——这种情况不是失败。
    pub forwarded: bool,
    /// 关窗口前发现「PID 已被复用、不是浏览器」而跳过的 PID。
    pub skipped_pids: Vec<u32>,
}

/// 启动失败的类别。
///
// `run()` 过去对所有 `execute()` 失败都套同一句「确认装了扩展」的 hint，
/// 但真正的原因可能是枚举进程失败、新进程立刻退出、或关闭旧实例失败。按类别给
/// 「下一步做什么」的提示，才不会再把人往错方向带。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchFailKind {
    /// 关闭旧实例失败（窗口不退出 / 强杀后仍在）。
    CloseFailed,
    /// 启动子进程本身失败（exe 不存在、权限、spawn 报错）。
    SpawnFailed,
    /// 枚举浏览器进程失败或超时，无法确认是否已有实例。
    EnumFailed,
    /// 新进程立刻退出（URL 被旧实例接走），命令行参数没有生效。
    ParamsNotEffective,
}

impl LaunchFailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LaunchFailKind::CloseFailed => "close_failed",
            LaunchFailKind::SpawnFailed => "spawn_failed",
            LaunchFailKind::EnumFailed => "enum_failed",
            LaunchFailKind::ParamsNotEffective => "params_not_effective",
        }
    }
}

/// 带类别的启动失败。
#[derive(Debug)]
pub struct LaunchFail {
    pub kind: LaunchFailKind,
    pub error: anyhow::Error,
}

impl LaunchFail {
    fn new(kind: LaunchFailKind, error: impl Into<anyhow::Error>) -> Self {
        Self {
            kind,
            error: error.into(),
        }
    }
}

impl fmt::Display for LaunchFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for LaunchFail {}

/// 执行计划：温和关闭旧实例（如有）→ 启动 → 确认新进程活着。
///
/// `restart` 由调用方显式传入：不传就绝不关窗口（用户可能正在填表单）。
pub fn execute(
    plan: &Plan,
    restart: bool,
    force: bool,
    urls_requested: bool,
) -> Result<Executed, LaunchFail> {
    let mut skipped_pids = Vec::new();
    if restart && !plan.running.is_empty() {
        skipped_pids = close_instances(plan.kind, &plan.running, force)
            .map_err(|err| LaunchFail::new(LaunchFailKind::CloseFailed, err))?;
    }
    let child =
        spawn_detached(plan).map_err(|err| LaunchFail::new(LaunchFailKind::SpawnFailed, err))?;
    let pid = child.id();

    // 已有一个同 profile 的实例在跑时，新进程会立刻退出（把 URL 转发过去），
    // 命令行参数**静默失效** —— 这正是最容易被误判成「已经加上了开关」的情况。
    std::thread::sleep(launch_settle());
    let running = instances(plan.kind)
        .context("启动后无法确认浏览器进程状态（枚举失败，不能假装成功）")
        .map_err(|err| LaunchFail::new(LaunchFailKind::EnumFailed, err))?;
    let alive = running.iter().any(|item| item.pid == pid);
    if !alive {
        // 旧实例本来就带静默开关：新进程把 URL 转过去后退出，这是成功的转发，
        // 不能报「参数没生效」——否则 `atb launch <url>` 永远打不开页面。
        // 只在「没有要求重启」时成立：重启路径上进程退出说明关闭失败/参数没生效。
        if plan.already_silent && urls_requested && !restart {
            return Ok(Executed {
                pid,
                forwarded: true,
                skipped_pids,
            });
        }
        let hint = if urls_requested {
            "URL 已经转发给正在运行的浏览器，但启动参数没有生效"
        } else {
            "新进程立刻退出：同一 profile 已经有一个实例在跑，启动参数没有生效"
        };
        return Err(LaunchFail::new(
            LaunchFailKind::ParamsNotEffective,
            anyhow!(
                "{hint}。Chrome 只在进程启动时读命令行参数，要生效必须先退出已有窗口（或加 --restart）"
            ),
        ));
    }
    Ok(Executed {
        pid,
        forwarded: false,
        skipped_pids,
    })
}

/// 一次启动请求。由 CLI 组装、daemon 执行 —— 浏览器必须由**常驻进程**拉起，
/// 而不是由一次性命令拉起：很多运行环境（含智能体框架）会把命令放进作业对象，
/// 命令一结束就杀掉整棵进程树，浏览器会跟着消失。
#[derive(Debug, Default)]
pub struct Request {
    pub kind: Option<Kind>,
    pub user_data_dir: Option<PathBuf>,
    pub restart: bool,
    /// 温和关闭无效时是否强杀（Chrome 在 Windows 上会忽略 WM_CLOSE）
    pub force: bool,
    pub restore_session: bool,
    pub extra_args: Vec<String>,
    pub urls: Vec<String>,
}

/// 执行启动请求，返回可直接输出的 JSON（失败也走 JSON，带 error_code）。
pub fn run(req: &Request) -> serde_json::Value {
    use serde_json::json;
    let kind = match req.kind {
        Some(kind) => kind,
        None => match auto_kind() {
            Ok(kind) => kind,
            Err(err) => {
                return json!({
                    "ok": false,
                    "error": err.to_string(),
                    "error_code": "no_browser",
                })
            }
        },
    };
    let plan = match plan(
        kind,
        req.user_data_dir.as_deref(),
        req.restart,
        &req.extra_args,
        &req.urls,
        req.restart && req.restore_session,
    ) {
        Ok(plan) => plan,
        Err(err) => {
            return json!({
                "ok": false,
                "error": err.to_string(),
                "error_code": "launch_failed",
            })
        }
    };
    let running_pids: Vec<u32> = plan.running.iter().map(|item| item.pid).collect();

    // 已经在用这条参数跑（进程命令行里有），再重启一次只会打断用户，直接如实回答。
    // 但**有 URL 时必须继续往下走**：早退会把 `atb launch https://example.com` 的
    // URL 整个丢掉，只返回 launched:false 让调用方以为已打开。旧条件只看
    // extra_args，不看 req.urls。
    if plan.already_silent && req.extra_args.is_empty() && req.urls.is_empty() {
        return json!({
            "ok": true,
            "result": {
                "launched": false,
                "already_running": true,
                "browser": plan.kind.name(),
                "user_data_dir": plan.user_data_dir.display().to_string(),
                "running_pids": running_pids,
                "silent_debug_banner": true,
                "hint": "这个浏览器已经在带 --silent-debugger-extension-api 运行（进程命令行里能看到），不需要重启"
            }
        });
    }

    // 已在带开关运行、只是要打开 URL：让新进程把 URL 转发给旧实例，不要报
    // browser_already_running（否则 `atb launch <url>` 永远开不了页面）。
    // 带 extra_args 时不能转发：那些参数在转发路径上不会生效，必须走重启。
    let forward_to_running =
        plan.already_silent && req.extra_args.is_empty() && !req.urls.is_empty() && !req.restart;
    if !req.restart && !running_pids.is_empty() && !forward_to_running {
        return json!({
            "ok": false,
            "error": format!(
                "{} 已经有一个实例在用这个 profile（pid {}）。命令行参数只在进程启动时读，再启动一次只会转发 URL，横幅照旧",
                plan.kind.name(),
                running_pids.iter().map(|pid| pid.to_string()).collect::<Vec<_>>().join(", ")
            ),
            "error_code": "browser_already_running",
            "hint": "加 --restart：先温和关闭（等同点窗口关闭按钮）再用正确参数启动，默认恢复上次标签页；Chrome 在 Windows 上会忽略关闭信号，必要时再加 --force",
            "result": {
                "browser": plan.kind.name(),
                "exe": plan.exe.display().to_string(),
                "user_data_dir": plan.user_data_dir.display().to_string(),
                "running_pids": running_pids,
            }
        });
    }

    let started = std::time::Instant::now();
    match execute(&plan, req.restart, req.force, !req.urls.is_empty()) {
        Ok(executed) if executed.forwarded => json!({
            "ok": true,
            "result": {
                "launched": false,
                "already_running": true,
                "urls_forwarded": true,
                "browser": plan.kind.name(),
                "exe": plan.exe.display().to_string(),
                "user_data_dir": plan.user_data_dir.display().to_string(),
                "args": plan.args,
                "silent_debug_banner": true,
                "skipped_pids": executed.skipped_pids,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "hint": "浏览器已在带 --silent-debugger-extension-api 运行，URL 已交给它打开（不再重启，避免打断当前窗口）"
            }
        }),
        Ok(executed) => json!({
            "ok": true,
            "result": {
                "launched": true,
                "browser": plan.kind.name(),
                "exe": plan.exe.display().to_string(),
                "pid": executed.pid,
                "user_data_dir": plan.user_data_dir.display().to_string(),
                "args": plan.args,
                "closed_pids": running_pids,
                "skipped_pids": executed.skipped_pids,
                "silent_debug_banner": true,
                // 强杀之后 Chrome 会当成崩溃：它会弹出「要恢复页面吗」而不是自动恢复，
                // 这一点必须写进结果，否则用户以为标签页丢了。
                "restore_hint": if req.force && !running_pids.is_empty() {
                    "Chrome 会显示「要恢复页面吗」，点「恢复」或按 Ctrl+Shift+T 找回刚才的标签页"
                } else {
                    ""
                },
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }
        }),
        Err(fail) => {
            // 按失败类别给「下一步做什么」。三种失败都可能发生在浏览器
            // 已经启动之后：枚举超时/新进程秒退时旧实例往往还在跑，所以提示里必须
            // 包含「先 stop / 检查实例」，否则用户会反复重试并把同一 profile 越搞越乱。
            let hint = match fail.kind {
                LaunchFailKind::CloseFailed => {
                    "旧实例没有退出，启动参数没有生效。先 `atb stop` 停掉 daemon，检查任务管理器里是否还有该 profile 的浏览器；必要时加 --force 或手动关闭窗口后再试"
                }
                LaunchFailKind::SpawnFailed => {
                    "浏览器进程没有成功启动。先 `atb stop` 或检查任务管理器，确认没有残留的同 profile 实例；再核对 exe 路径（ATB_BROWSER_EXE）与执行权限"
                }
                LaunchFailKind::EnumFailed => {
                    "无法确认浏览器进程状态（枚举命令失败或超时）。浏览器可能已经启动：先 `atb stop` 或检查任务管理器，排除残留实例后再重试"
                }
                LaunchFailKind::ParamsNotEffective => {
                    "浏览器可能已经在运行（新进程把请求转发给旧实例后退出）。先 `atb stop` 或关闭该 profile 的窗口，再用 --restart；并确认这个 profile 里装了 ATB 扩展"
                }
            };
            json!({
                "ok": false,
                "error": fail.to_string(),
                "error_code": "launch_failed",
                "reason": fail.kind.as_str(),
                "hint": hint,
                "result": {
                    "browser": plan.kind.name(),
                    "exe": plan.exe.display().to_string(),
                    "user_data_dir": plan.user_data_dir.display().to_string(),
                    "running_pids": running_pids,
                }
            })
        }
    }
}

/// 自动挑一个已安装的浏览器：Chrome 优先，其次 Edge，最后 Chromium。
pub fn auto_kind() -> Result<Kind> {
    for kind in [Kind::Chrome, Kind::Edge, Kind::Chromium] {
        if resolve_exe(kind).is_ok() {
            return Ok(kind);
        }
    }
    Err(anyhow!(
        "没找到已安装的 Chrome / Edge / Chromium，用 --browser 指定或 ATB_BROWSER_EXE 给完整路径"
    ))
}

/// 启动子进程，并尽可能让它摆脱调用方的作业对象（job object）。
///
/// 智能体框架（含本工具的测试环境）常把命令放进 job，命令一结束就杀掉整棵树；
/// 浏览器是被「请求打开」的长期进程，不该跟着死。Windows 上用
/// `CREATE_BREAKAWAY_FROM_JOB` 申请脱离，作业不允许时报错就退回普通 spawn。
fn spawn_detached(plan: &Plan) -> Result<std::process::Child> {
    let mut command = Command::new(&plan.exe);
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不闪控制台；CREATE_BREAKAWAY_FROM_JOB：脱离调用方作业对象。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let detached = command
            .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
            .spawn();
        if let Ok(child) = detached {
            return Ok(child);
        }
        // 作业禁止 breakaway 时会失败，退回普通 spawn。
        let mut retry = Command::new(&plan.exe);
        retry
            .args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        retry
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .with_context(|| format!("启动失败: {}", plan.exe.display()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        command
            .spawn()
            .with_context(|| format!("启动失败: {}", plan.exe.display()))
    }
}

/// 校验 `ATB_BROWSER_EXE`：必须是**普通文件**，而且当前平台能把它当程序执行。
///
// 为什么不能只判 `exists()`/`is_file()`：
/// - 目录、文本文件、符号链接目标缺失的路径都能通过 `exists()`，直到 `spawn` 才失败；
///   那时浏览器会话、临时 profile 都已经准备好，错误位置离真正原因很远；
/// - `is_file()` 只解决「是目录」这一种，Windows 上把 `chrome.txt` 之类的非可执行
///   文件指过来，`spawn` 仍然会报 193（不是有效的 Win32 应用）；
/// - Unix 上没有执行位（例如从 Windows 挂载盘拷过来、或者 `chmod -x`）同样会被
///   `spawn` 拒绝，而且报的是 EACCES，看起来像权限问题而不是配置问题。
///
/// 三档判定依次进行，错误信息里带稳定标记（`not_a_file` / `not_executable`），
/// 方便上层或脚本按标记分支：
/// 1. `metadata` 失败 → 路径不存在 / 不可访问；
/// 2. 不是普通文件 → `not_a_file`（目录、设备文件、悬空符号链接）；
/// 3. 平台可执行性不满足 → `not_executable`（Windows 要求 `.exe` 后缀，Unix 要求
///    属主/组/其他任一执行位）。
fn validate_browser_exe(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        anyhow!(
            "ATB_BROWSER_EXE 指向的文件不存在或不可访问（not_a_file）: {}（{err}）",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "ATB_BROWSER_EXE 指向的不是普通文件（not_a_file）: {}；请指向浏览器可执行文件本身",
            path.display()
        ));
    }
    #[cfg(target_os = "windows")]
    {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !extension.eq_ignore_ascii_case("exe") {
            return Err(anyhow!(
                "ATB_BROWSER_EXE 必须是 .exe（not_executable）: {}；Windows 只能启动可执行文件",
                path.display()
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        // `access(X_OK)` 用**当前进程的真实 uid/gid** 判断，比「任意一个执行位」准确：
        // 0644 root 文件、或 ACL 允许但 mode 位不匹配的文件，旧判定会给出相反结论
        //。拿不到 C 字符串时退回 mode 位近似。
        let executable = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
            Ok(value) => {
                // SAFETY: `value` 以 NUL 结尾且在调用期间存活。
                unsafe { libc::access(value.as_ptr(), libc::X_OK) == 0 }
            }
            Err(_) => {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
        };
        if !executable {
            return Err(anyhow!(
                "ATB_BROWSER_EXE 没有可执行权限（not_executable）: {}；先 chmod +x 再试",
                path.display()
            ));
        }
    }
    Ok(())
}

/// 找到浏览器可执行文件。`ATB_BROWSER_EXE` 优先，便于装在非标准路径时覆盖。
pub fn resolve_exe(kind: Kind) -> Result<PathBuf> {
    if let Some(raw) = crate::settings::browser_exe() {
        let path = PathBuf::from(raw);
        validate_browser_exe(&path)?;
        return Ok(path);
    }
    let mut tried: Vec<String> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let program_files = env::var("ProgramFiles").ok();
        let program_files_x86 = env::var("ProgramFiles(x86)").ok();
        let local_app_data = env::var("LOCALAPPDATA").ok();
        let relative = match kind {
            Kind::Chrome => "Google\\Chrome\\Application\\chrome.exe",
            Kind::Edge => "Microsoft\\Edge\\Application\\msedge.exe",
            Kind::Chromium => "Chromium\\Application\\chrome.exe",
        };
        for base in [program_files, program_files_x86, local_app_data]
            .into_iter()
            .flatten()
        {
            let candidate = Path::new(&base).join(relative);
            tried.push(candidate.display().to_string());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (bundle, binary) = match kind {
            Kind::Chrome => ("Google Chrome", "Google Chrome"),
            Kind::Edge => ("Microsoft Edge", "Microsoft Edge"),
            Kind::Chromium => ("Chromium", "Chromium"),
        };
        let mut bases = vec![PathBuf::from("/Applications")];
        if let Some(home) = env::var_os("HOME") {
            bases.push(Path::new(&home).join("Applications"));
        }
        for base in bases {
            let candidate = base.join(format!("{bundle}.app/Contents/MacOS/{binary}"));
            tried.push(candidate.display().to_string());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // WSL 下 `which` 找到的是 Linux 侧浏览器（通常没装），用户真正想用的是
        // Windows 侧那个（`/mnt/c/...`）。先探测挂载点，再退回 which。
        if is_wsl() {
            for candidate in wsl_browser_candidates(kind) {
                tried.push(candidate.display().to_string());
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        let names: &[&str] = match kind {
            Kind::Chrome => &["google-chrome", "google-chrome-stable"],
            Kind::Edge => &["microsoft-edge", "microsoft-edge-stable"],
            Kind::Chromium => &["chromium", "chromium-browser"],
        };
        for name in names {
            tried.push((*name).to_string());
            let mut command = Command::new("which");
            command.arg(name);
            if let Some(output) = run_with_timeout(&mut command, command_timeout()) {
                if output.status.success() {
                    // 不再 `from_utf8_lossy`：非 UTF-8 路径会被替换成 U+FFFD，随后
                    // spawn 必然报「文件不存在」，错误位置远离真实原因。
                    if let Some(path) = path_from_which_output(&output.stdout) {
                        return Ok(path);
                    }
                }
            }
        }
    }
    Err(anyhow!(
        "找不到 {} 的可执行文件（找过: {}）。可用 ATB_BROWSER_EXE 指定完整路径",
        kind.name(),
        if tried.is_empty() {
            "（无候选）".to_string()
        } else {
            tried.join(" / ")
        }
    ))
}

/// 是否运行在 WSL 里（`/proc/version` 含有 microsoft 标记）。
#[cfg(all(unix, not(target_os = "macos")))]
fn is_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/version")
            .map(|text| text.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// WSL 里 Windows 浏览器的常见安装位置（`/mnt/c/...`）。
///
/// 顺序：Program Files → Program Files (x86) → 当前用户/其它用户的
/// `AppData\Local`。WSL 的 `$USER` 与 Windows 用户名经常不同，所以除了按环境变量
/// 猜一次，还会枚举 `/mnt/c/Users/*`（只读目录，失败就跳过）。
#[cfg(all(unix, not(target_os = "macos")))]
fn wsl_browser_candidates(kind: Kind) -> Vec<PathBuf> {
    let relative = match kind {
        Kind::Chrome => "Google/Chrome/Application/chrome.exe",
        Kind::Edge => "Microsoft/Edge/Application/msedge.exe",
        Kind::Chromium => "Chromium/Application/chrome.exe",
    };
    let mut bases = vec![
        PathBuf::from("/mnt/c/Program Files"),
        PathBuf::from("/mnt/c/Program Files (x86)"),
    ];
    let mut users: Vec<PathBuf> = Vec::new();
    if let Some(name) = env::var_os("USER").or_else(|| env::var_os("USERNAME")) {
        users.push(Path::new("/mnt/c/Users").join(name));
    }
    if let Ok(entries) = std::fs::read_dir("/mnt/c/Users") {
        for entry in entries.flatten() {
            let path = entry.path();
            if !users.contains(&path) {
                users.push(path);
            }
        }
    }
    for user in users {
        bases.push(user.join("AppData/Local"));
    }
    bases.into_iter().map(|base| base.join(relative)).collect()
}

/// 把 `which` 的原始 stdout 转成 PathBuf，全程不经过 UTF-8。
///
/// 旧实现 `String::from_utf8_lossy` 会把非 UTF-8 字节替换成 U+FFFD，得到的路径
// 一定不存在。这里只取第一行并裁掉首尾 ASCII 空白。
#[cfg(all(unix, not(target_os = "macos")))]
fn path_from_which_output(stdout: &[u8]) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let line_end = stdout
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
        .unwrap_or(stdout.len());
    let line = &stdout[..line_end];
    let start = line.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)?;
    if start >= end {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&line[start..end])))
}

/// 该浏览器的默认用户数据目录（Profile 的上一层）。
pub fn default_user_data_dir(kind: Kind) -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local = env::var("LOCALAPPDATA")
            .map_err(|_| anyhow!("读不到 LOCALAPPDATA，无法推断默认用户数据目录"))?;
        let relative = match kind {
            Kind::Chrome => "Google\\Chrome\\User Data",
            Kind::Edge => "Microsoft\\Edge\\User Data",
            Kind::Chromium => "Chromium\\User Data",
        };
        Ok(Path::new(&local).join(relative))
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var("HOME").map_err(|_| anyhow!("读不到 HOME"))?;
        let relative = match kind {
            Kind::Chrome => "Library/Application Support/Google/Chrome",
            Kind::Edge => "Library/Application Support/Microsoft Edge",
            Kind::Chromium => "Library/Application Support/Chromium",
        };
        Ok(Path::new(&home).join(relative))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = env::var("HOME").map_err(|_| anyhow!("读不到 HOME"))?;
        let relative = match kind {
            Kind::Chrome => ".config/google-chrome",
            Kind::Edge => ".config/microsoft-edge",
            Kind::Chromium => ".config/chromium",
        };
        Ok(Path::new(&home).join(relative))
    }
}

/// 把相对路径按**调用进程**的 cwd 绝对化。
///
/// 注意：`/launch` 是由常驻 daemon 执行的，daemon 由 CLI 以 `current_dir(home)`
/// 拉起，所以这里的 cwd 很可能不是用户 shell 的 cwd。CLI 侧下发绝对的
/// `--user-data-dir` 才是最优解；这里绝对化是为了至少不再把相对路径直接塞给浏览器
/// （否则浏览器按自己的 cwd 解析，同一个参数在两次启动里可能指向不同 profile）。
fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(normalize(path));
    }
    let cwd = env::current_dir().context("读取当前目录失败")?;
    Ok(normalize(&cwd.join(path)))
}

/// 去掉尾部分隔符、`.` / `..`，方便做路径比较（Windows 上还要忽略大小写）。
///
/// 栈空时遇到 `..` **必须保留**：`..\profile` 与 `profile` 是两个不同目录，旧实现
/// 直接 `pop` 会把前者悄悄变成后者。
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    // out 里「真实路径段」的数量（根 / 盘符不算）。为 0 时遇到 `..` 不能再弹，
    // 因为 out 里剩下的只可能是开头的 `..`，弹掉就把 `..\x` 变成 `x` 了。
    let mut real_segments = 0usize;
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if real_segments > 0 {
                    out.pop();
                    real_segments -= 1;
                } else if path.is_relative() {
                    out.push("..");
                }
                // 绝对路径已经到根：根之上的 `..` 没有意义，直接丢弃。
            }
            other => {
                if matches!(other, Component::Normal(_)) {
                    real_segments += 1;
                }
                out.push(other.as_os_str());
            }
        }
    }
    out
}

fn same_dir(a: &str, b: &Path) -> bool {
    let left = normalize(Path::new(a));
    let right = normalize(b);
    // Windows 与 macOS 的默认文件系统都大小写不敏感（NTFS / APFS），只按字节比较会
    // 把 `C:/Prof` 与 `C:/prof` 当成两个目录 → 漏判实例冲突。Linux 保留
    // 精确比较（ext4 默认大小写敏感）。
    #[cfg(any(windows, target_os = "macos"))]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right
    }
}

/// 这条命令行是否在用给定的用户数据目录。
fn uses_dir(command: &str, dir: &Path, default_dir: Option<&Path>) -> bool {
    if let Some(explicit) = flag_value(command, "--user-data-dir") {
        return same_dir(&explicit, dir);
    }
    // 没带 --user-data-dir = 用默认目录；只有当我们也要用默认目录时才算冲突。
    default_dir
        .map(|default| same_dir(&default.to_string_lossy(), dir))
        .unwrap_or(false)
}

/// 从命令行里取 `--flag=value` 或 `--flag value` 的值。
fn flag_value(command: &str, flag: &str) -> Option<String> {
    let tokens = tokenize(command);
    let prefix = format!("{flag}=");
    for (index, token) in tokens.iter().enumerate() {
        if let Some(rest) = token.strip_prefix(&prefix) {
            return Some(strip_outer_quotes(rest).to_string());
        }
        if token == flag {
            if let Some(next) = tokens.get(index + 1) {
                return Some(strip_outer_quotes(next).to_string());
            }
        }
    }
    None
}

/// 只剥掉**成对**的外层引号：`"C:/prof"` → `C:/prof`。
///
/// 旧实现用 `trim_matches('"')`，会把内容两端所有引号都剥掉（`"a"b"` 变成 `a"b`），
/// 合法路径里真的含引号时就被改坏了（旧报告 #3）。
fn strip_outer_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// 粗略切分命令行：处理双引号，够用来读路径。
fn tokenize(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ' ' | '\t' if !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 列出正在运行的同类浏览器**主进程**（helper 进程由浏览器自己管理，不当冲突）。
pub fn instances(kind: Kind) -> Result<Vec<Instance>> {
    #[cfg(target_os = "windows")]
    {
        instances_windows(kind)
    }
    #[cfg(not(target_os = "windows"))]
    {
        instances_unix(kind)
    }
}

#[cfg(target_os = "windows")]
fn instances_windows(kind: Kind) -> Result<Vec<Instance>> {
    let name = kind.process_name();
    // 脚本里**一个双引号都不用**：这段文本要作为单个 argv 传给 powershell，
    // 双引号会先被 Windows 的命令行解析吃掉，`-Filter "name='chrome.exe'"`
    // 到 PowerShell 手里就变成了 `-Filter name='chrome.exe'`（语法错误，静默返回空）。
    // 用 Where-Object 过滤，全程单引号，跨 shell 都稳。
    //
    // 输出编码固定 zh-CN 系统默认 cp936，`--user-data-dir` 里的中文会变成
    // mojibake，`same_dir` 比对失败 → 误判「没有实例在跑」。
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; \
         Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq '{name}' }} | \
         Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress -Depth 2"
    );
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags_no_window();
    // 超时：WMI 卡住时宁可如实报「查不到」，也不能让启动流程无限期挂着。
    let Some(output) = run_with_timeout(&mut command, command_timeout()) else {
        return Err(anyhow!(
            "枚举浏览器进程超时（powershell 超过 {:?} 没有返回，ATB_PROC_CMD_TIMEOUT_MS 可调）。无法确认是否已有实例在跑，不能盲目启动",
            command_timeout()
        ));
    };
    // PowerShell 失败时 stdout 一般是空的。旧实现只读 stdout，于是把「查询失败」
    // 当成「没有实例在跑」：`--restart` 不关旧实例、`count_alive` 恒 0，最后报
    // 「启动参数没有生效」，把用户引向完全错误的方向。
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(anyhow!(
            "枚举浏览器进程失败（powershell 退出码 {code}）：{}。无法确认是否已有实例在跑，不能盲目启动",
            detail.chars().take(300).collect::<String>()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // 枚举不出来必须报错，不能返回「没有实例」：后者会让调用方以为可以安全启动，
    // 实际新进程只是把请求转给了正在跑的窗口，参数静默失效。
    parse_windows_processes(&text, kind)
}

#[cfg(not(target_os = "windows"))]
fn instances_unix(kind: Kind) -> Result<Vec<Instance>> {
    let mut command = Command::new("ps");
    command.args(["-ax", "-o", "pid=,command="]);
    let Some(output) = run_with_timeout(&mut command, command_timeout()) else {
        return Err(anyhow!(
            "枚举浏览器进程超时（ps 超过 {:?} 没有返回，ATB_PROC_CMD_TIMEOUT_MS 可调）。无法确认是否已有实例在跑，不能盲目启动",
            command_timeout()
        ));
    };
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ps(&text, kind))
}

/// Unix 上各浏览器的可执行文件名（macOS 包里最后一个路径段也在这张表里）。
#[cfg(not(target_os = "windows"))]
fn unix_exe_names(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Chrome => &[
            "chrome",
            "google-chrome",
            "google-chrome-stable",
            "google chrome",
        ],
        Kind::Edge => &[
            "edge",
            "msedge",
            "microsoft-edge",
            "microsoft-edge-stable",
            "microsoft edge",
        ],
        Kind::Chromium => &["chromium", "chromium-browser"],
    }
}

/// macOS 的 `.app` 包路径带空格，`ps` 不加引号，tokenize 会把路径切碎；
/// 这里按已知完整路径前缀兜底。
#[cfg(not(target_os = "windows"))]
fn unix_bundle_prefixes(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Chrome => &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"],
        Kind::Edge => &["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"],
        Kind::Chromium => &["/Applications/Chromium.app/Contents/MacOS/Chromium"],
    }
}

/// 这条命令行是不是由指定可执行文件启动的。
///
/// 只看 argv[0] 的可执行文件名（或 macOS 包前缀），不做裸子串匹配：旧实现用
/// `command.contains("chromium")`，会把 `python3 -m http.server --directory
/// ~/chromium-downloads` 也当成浏览器并 SIGTERM。
// Windows 上生产代码走 Win32_Process.Name，这里只有单元测试用到；逻辑本身跨平台，
// 所以不加 cfg 门，避免 Unix 的正确性只能靠 CI 验证。
#[cfg_attr(windows, allow(dead_code))]
fn command_uses_exe(command: &str, names: &[&str], bundle_prefixes: &[&str]) -> bool {
    let trimmed = command.trim_start();
    if bundle_prefixes
        .iter()
        .any(|prefix| starts_with_exe_prefix(trimmed, prefix))
    {
        return true;
    }
    let Some(first) = tokenize(trimmed).into_iter().next() else {
        return false;
    };
    let exe = strip_outer_quotes(&first);
    let base = Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(exe);
    names.iter().any(|name| {
        base.eq_ignore_ascii_case(name) || base.eq_ignore_ascii_case(&format!("{name}.exe"))
    })
}

/// 前缀匹配要求后面紧跟空白或结束，避免 `/Applications/Chrome.app/.../ChromeHelper`
/// 这类同前缀的其它二进制被误判。
#[cfg_attr(windows, allow(dead_code))]
fn starts_with_exe_prefix(command: &str, prefix: &str) -> bool {
    match command.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t'),
        None => false,
    }
}

#[cfg(target_os = "windows")]
fn parse_windows_processes(text: &str, kind: Kind) -> Result<Vec<Instance>> {
    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(rename = "ProcessId")]
        pid: u32,
        #[serde(rename = "CommandLine")]
        command: Option<String>,
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // PowerShell 正常返回空 = 真的一个都没在跑。
        return Ok(Vec::new());
    }
    let rows: Vec<Row> = match serde_json::from_str::<Vec<Row>>(trimmed) {
        Ok(value) => value,
        Err(_) => match serde_json::from_str::<Row>(trimmed) {
            Ok(one) => vec![one],
            Err(err) => {
                return Err(anyhow!(
                    "解析浏览器进程列表失败（{err}）：动手前必须先确认有没有实例在跑，否则启动参数会静默失效。原始输出前 200 字符: {}",
                    trimmed.chars().take(200).collect::<String>()
                ))
            }
        },
    };
    let _ = kind;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let command = row.command?;
            // helper 进程（渲染、GPU、utility）都带 --type=，它们不是「另一个实例」。
            if command.contains("--type=") {
                return None;
            }
            Some(Instance {
                pid: row.pid,
                command,
            })
        })
        .collect())
}

#[cfg(not(target_os = "windows"))]
fn parse_ps(text: &str, kind: Kind) -> Vec<Instance> {
    let names = unix_exe_names(kind);
    let bundles = unix_bundle_prefixes(kind);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid, command)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.trim().parse::<u32>() else {
            continue;
        };
        if !command_uses_exe(command, names, bundles) {
            continue;
        }
        if command.contains("--type=") {
            continue;
        }
        out.push(Instance {
            pid,
            command: command.trim().to_string(),
        });
    }
    out
}

/// 温和关闭：Windows 用不带 `/F` 的 taskkill（等于点窗口的关闭按钮），Unix 发 SIGTERM。
/// 之后轮询确认进程真的退出——否则新进程会把请求转发给旧实例，参数静默失效。
///
/// 返回「因 PID 已被复用而跳过」的 PID 列表，供上层如实报告（旧报告 #9）。
fn close_instances(kind: Kind, targets: &[Instance], force: bool) -> Result<Vec<u32>> {
    // 发信号前逐个核对进程身份：PID 会被系统回收，直接按 PID 杀可能杀掉无辜进程。
    let (live, mut skipped) = split_by_identity(kind, targets);
    if live.is_empty() {
        return Ok(skipped);
    }
    let _ = signal_close(kind, &live, false);
    // Chrome 在 Windows 上会忽略 WM_CLOSE（实测：`taskkill /PID` 报「已发送终止信号」，
    // 进程照样活着）。所以先给 3s 的温和窗口，再按调用方是否给了 --force 决定下一步。
    if wait_gone(kind, &live, close_wait_budget())? {
        return Ok(skipped);
    }
    if !force {
        let alive = count_alive(kind, &live)?;
        return Err(anyhow!(
            "还有 {alive} 个浏览器窗口没退出（Chrome 在 Windows 上会忽略关闭信号，也可能停在「未保存内容」确认框）。\
             加 --force 强制结束（未保存的表单会丢；标签页会用 --restore-last-session 恢复），\
             或手动关掉窗口后再运行{}",
            reused_hint(&skipped)
        ));
    }
    // 强杀：从上面那次核对到这里已经过去几秒，PID 更可能被复用。`signal_close` 会在
    // 每个 `/F` 之前**再核一次**身份，临杀前发现不是那个浏览器的直接跳过。
    skipped.extend(signal_close(kind, &live, true));
    if wait_gone(kind, &live, kill_wait_budget())? {
        return Ok(skipped);
    }
    let alive = count_alive(kind, &live)?;
    Err(anyhow!(
        "强杀后仍有 {alive} 个窗口在跑，请手动关闭后重试{}",
        reused_hint(&skipped)
    ))
}

/// 跳过提示：只在真的发生 PID 复用时追加，避免正常路径多一句噪音。
fn reused_hint(skipped: &[u32]) -> String {
    if skipped.is_empty() {
        String::new()
    } else {
        format!(
            "（另有 PID {} 已被系统复用、不是浏览器，已跳过；不会误杀）",
            skipped
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// 按「这个 PID 现在还是不是那个浏览器实例」把目标分成可操作 / 需跳过两组。
///
/// 一次枚举就够了：`instances()` 已经按映像名筛过，再比对命令行里的
/// `--user-data-dir`，不必每个 PID 起一个 tasklist/powershell。
/// 枚举失败时按「全都不是它」处理：宁可跳过、不杀，也不能误杀。
fn split_by_identity(kind: Kind, targets: &[Instance]) -> (Vec<Instance>, Vec<u32>) {
    let running = instances(kind).unwrap_or_default();
    let mut live = Vec::new();
    let mut skipped = Vec::new();
    for item in targets {
        let same = running.iter().any(|candidate| {
            candidate.pid == item.pid && same_instance_command(&item.command, &candidate.command)
        });
        if same {
            live.push(item.clone());
        } else {
            skipped.push(item.pid);
        }
    }
    (live, skipped)
}

/// 同一个实例的判定：PID 相同只是必要条件，命令行里的 `--user-data-dir` 也必须一致。
///
/// 原实例带 profile 参数、现在这个 PID 的命令行却没有（或指向别的目录），说明 PID
/// 已经被系统回收给别的进程，应当当作「已经退出」。
fn same_instance_command(expected: &str, actual: &str) -> bool {
    let Some(expected_dir) = flag_value(expected, "--user-data-dir") else {
        // 原实例本来就用默认目录，命令行里没有可比对的锚点，只能按 PID 认。
        return true;
    };
    match flag_value(actual, "--user-data-dir") {
        Some(actual_dir) => same_dir(&expected_dir, Path::new(&actual_dir)),
        None => false,
    }
}

/// 取指定 PID 的完整命令行（Windows / WMI）。
///
/// 取不到（进程不存在、权限不足、WMI 不可用、超时）一律返回 None，由调用方决定
/// 是否退回按映像名核对。
#[cfg(target_os = "windows")]
fn process_command_line(pid: u32) -> Option<String> {
    // 与 instances_windows 同样的编码处理：zh-CN 系统默认 cp936，中文路径会变 mojibake。
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; \
         $p = Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; \
         if ($p -eq $null) {{ '' }} else {{ [string]$p.CommandLine }}"
    );
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags_no_window();
    let output = run_with_timeout(&mut command, command_timeout())?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 只按映像名核对（拿不到命令行时的退路）。
#[cfg(target_os = "windows")]
fn process_image_is_browser(kind: Kind, pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let mut command = Command::new("tasklist");
    command
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .creation_flags_no_window();
    let Some(output) = run_with_timeout(&mut command, command_timeout()) else {
        return false;
    };
    if !output.status.success() {
        // 查不到就当作「不是它」：宁可跳过，也不能杀错。
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.contains(&kind.process_name().to_ascii_lowercase())
}

/// 发信号前的身份核对：PID 可能已经被别的进程复用。
///
/// 除了映像名，还要比对命令行里的 `--user-data-dir`：同机可能同时开着 Chrome 与
/// Edge、或同一个浏览器的两个 profile，只看进程名不足以确认「还是不是它」。
#[cfg(target_os = "windows")]
fn process_is_browser(kind: Kind, instance: &Instance) -> bool {
    match process_command_line(instance.pid) {
        Some(command) if !command.trim().is_empty() => {
            if !command_uses_exe(&command, &[kind.process_name()], &[]) {
                return false;
            }
            match flag_value(&instance.command, "--user-data-dir") {
                Some(expected) => match flag_value(&command, "--user-data-dir") {
                    Some(actual) => same_dir(&expected, Path::new(&actual)),
                    // 同一个 exe、但命令行里的 profile 没了：多半是 PID 复用。
                    None => false,
                },
                // 原实例本来就跑默认目录，没有可比对的锚点。
                None => true,
            }
        }
        // 取不到命令行（权限不足 / WMI 不可用 / 超时）：退回映像名核对，保持可用，
        // 但要留下原因，别让「为什么没识别出 PID 复用」变成悬案。
        _ => {
            eprintln!(
                "提示: 取不到 PID {} 的命令行，退回按映像名核对进程身份（无法排除 PID 复用）",
                instance.pid
            );
            process_image_is_browser(kind, instance.pid)
        }
    }
}

/// 发信号前的身份核对：`ps` 的 argv[0] 必须还是这个浏览器，且 profile 一致。
#[cfg(not(target_os = "windows"))]
fn process_is_browser(kind: Kind, instance: &Instance) -> bool {
    let mut command = Command::new("ps");
    command.args(["-o", "args=", "-p", &instance.pid.to_string()]);
    let Some(output) = run_with_timeout(&mut command, command_timeout()) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let live = String::from_utf8_lossy(&output.stdout);
    let live = live.trim();
    !live.is_empty()
        && command_uses_exe(live, unix_exe_names(kind), unix_bundle_prefixes(kind))
        && same_instance_command(&instance.command, live)
}

/// 发关闭信号。`force = false` 时是「请正常退出」，`true` 时才是强杀。
///
/// 强杀（`/F` 或 SIGKILL）不可撤销，所以每个 PID 在动手前都**再核一次身份**：从
/// `close_instances` 最初那次核对到这一刻可能已经过去好几秒，PID 完全可能被回收。
/// 返回临杀前发现「已经不是这个浏览器」而跳过的 PID。
fn signal_close(kind: Kind, targets: &[Instance], force: bool) -> Vec<u32> {
    let mut skipped = Vec::new();
    for item in targets {
        if force && !process_is_browser(kind, item) {
            skipped.push(item.pid);
            continue;
        }
        #[cfg(target_os = "windows")]
        {
            let pid = item.pid.to_string();
            let mut command = Command::new("taskkill");
            command.creation_flags_no_window();
            if force {
                command.args(["/F", "/PID", &pid]);
            } else {
                command.args(["/PID", &pid]);
            }
            let _ = run_with_timeout(&mut command, command_timeout());
        }
        #[cfg(not(target_os = "windows"))]
        {
            let signal = if force { "-KILL" } else { "-TERM" };
            let mut command = Command::new("kill");
            command.args([signal, &item.pid.to_string()]);
            let _ = run_with_timeout(&mut command, command_timeout());
        }
    }
    skipped
}

/// 还在跑的实例数：PID 相同 + 命令行里的 profile 一致才算同一个实例。
fn count_alive(kind: Kind, targets: &[Instance]) -> Result<usize> {
    Ok(count_alive_in(&instances(kind)?, targets))
}

/// 在一份已有的枚举结果里比对，避免为每个目标重复启动枚举进程。
fn count_alive_in(running: &[Instance], targets: &[Instance]) -> usize {
    running
        .iter()
        .filter(|item| {
            targets.iter().any(|target| {
                target.pid == item.pid && same_instance_command(&target.command, &item.command)
            })
        })
        .count()
}

/// 轮询到全部退出（true）或超时（false）。
fn wait_gone(kind: Kind, targets: &[Instance], budget: Duration) -> Result<bool> {
    let deadline = Instant::now() + budget;
    loop {
        // 一轮只枚举一次：PowerShell 冷启动本身就要几百毫秒，之前每 200ms 全量枚举
        // 一次既拖慢关闭，也让轮询频率形同虚设。
        let running = instances(kind)?;
        if count_alive_in(&running, targets) == 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(close_poll_interval());
    }
}

#[cfg(target_os = "windows")]
trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

#[cfg(target_os = "windows")]
impl NoWindow for Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_leading_parent_dir() {
        // `..\x` 与 `x` 是两个不同目录，不能被折叠成同一个。
        let normalized = normalize(Path::new("../x"));
        assert!(
            normalized.to_string_lossy().starts_with(".."),
            "{}",
            normalized.display()
        );
        assert!(normalized.to_string_lossy().ends_with('x'));
        // 相对路径里连续上跳也要保留。
        let deep = normalize(Path::new("a/../../b"));
        assert!(
            deep.to_string_lossy().replace('\\', "/").starts_with(".."),
            "{}",
            deep.display()
        );
        // 比较 PathBuf 而不是字符串：Windows 上 `normalize` 产出的是 `a\c`，
        // 而 `Path::new("a/c")` 的 Display 保留正斜杠，直接比字符串会假失败。
        assert_eq!(normalize(Path::new("a/b/../c")), Path::new("a/c"));
    }

    #[test]
    fn same_instance_command_compares_user_data_dir() {
        let original = r#""chrome.exe" --user-data-dir=C:/prof/a --no-first-run"#;
        let same = r#""chrome.exe" --user-data-dir=C:/prof/a --restore-last-session"#;
        let other = r#""chrome.exe" --user-data-dir=C:/prof/b --no-first-run"#;
        // PID 被复用给「同一个 exe、但没有 profile 参数」的进程：视为已退出。
        let without_profile = r#""chrome.exe" --no-first-run"#;
        assert!(same_instance_command(original, same));
        assert!(!same_instance_command(original, other));
        assert!(!same_instance_command(original, without_profile));
        // 原实例没带 profile 参数时只能按 PID 认。
        assert!(same_instance_command(without_profile, other));
    }

    #[test]
    fn count_alive_ignores_reused_pid() {
        let targets = vec![Instance {
            pid: 42,
            command: r#""chrome.exe" --user-data-dir=C:/prof/a"#.to_string(),
        }];
        let running = vec![Instance {
            pid: 42,
            command: r#""chrome.exe" --user-data-dir=C:/prof/b"#.to_string(),
        }];
        assert_eq!(count_alive_in(&running, &targets), 0);
        let same = vec![Instance {
            pid: 42,
            command: r#""chrome.exe" --user-data-dir=C:/prof/a"#.to_string(),
        }];
        assert_eq!(count_alive_in(&same, &targets), 1);
    }

    #[test]
    fn tokenize_keeps_quoted_paths_whole() {
        let command = r#""C:/Program Files/Google/Chrome/Application/chrome.exe" --user-data-dir="C:/Users/me/App Data" --no-first-run"#;
        let tokens = tokenize(command);
        assert_eq!(
            tokens[0],
            r#""C:/Program Files/Google/Chrome/Application/chrome.exe""#
        );
        assert!(tokens.iter().any(|t| t.contains("App Data")));
    }

    #[test]
    fn flag_value_reads_both_forms() {
        let equals = r#"chrome.exe --user-data-dir=C:/prof --no-first-run"#;
        assert_eq!(
            flag_value(equals, "--user-data-dir").as_deref(),
            Some(r"C:/prof")
        );
        let spaced = r#"chrome.exe --user-data-dir "C:/prof with space" --no-first-run"#;
        assert_eq!(
            flag_value(spaced, "--user-data-dir").as_deref(),
            Some("C:/prof with space")
        );
        assert!(flag_value("chrome.exe --no-first-run", "--user-data-dir").is_none());
    }

    #[test]
    fn uses_dir_matches_default_only_when_target_is_default() {
        let default = Path::new("C://Users//me//AppData//Local//Google//Chrome//User Data");
        let command = r#""chrome.exe" --no-first-run"#;
        assert!(uses_dir(command, default, Some(default)));
        assert!(!uses_dir(command, Path::new("E://other"), Some(default)));
    }

    #[test]
    fn uses_dir_honors_explicit_flag() {
        let dir = Path::new("C://temp//atb-lprof");
        let command = r#""chrome.exe" --user-data-dir=C:/temp/atb-lprof --no-first-run"#;
        assert!(uses_dir(command, dir, Some(Path::new("C://default"))));
        let other = r#""chrome.exe" --user-data-dir=E:/another --no-first-run"#;
        assert!(!uses_dir(other, dir, None));
    }

    #[test]
    fn plan_always_carries_silent_debug_switch() {
        // 这条参数是「不出现调试横幅」的唯一开关，任何分支都不能丢。
        let plan = plan(
            Kind::Chrome,
            Some(Path::new("E://nonexistent-profile")),
            false,
            &["--remote-debugging-port=0".to_string()],
            &["about:blank".to_string()],
            false,
        );
        // 机器上有 Chrome 才会有 exe；没有就跳过（CI 上跑 unix 分支）。
        if let Ok(plan) = plan {
            assert!(plan.args.iter().any(|arg| arg == SILENT_DEBUG_SWITCH));
            assert!(plan
                .args
                .iter()
                .any(|arg| arg == "--remote-debugging-port=0"));
            assert_eq!(plan.args.last().map(String::as_str), Some("about:blank"));
        }
    }

    #[test]
    fn strip_outer_quotes_keeps_inner_quotes() {
        assert_eq!(strip_outer_quotes(r#""C:/prof""#), "C:/prof");
        assert_eq!(strip_outer_quotes("plain"), "plain");
        // 只剥一层；内容本身的引号必须保留（旧报告 #3）。
        assert_eq!(strip_outer_quotes(r#""a"b""#), r#"a"b"#);
        assert_eq!(strip_outer_quotes(r#"a"b"#), r#"a"b"#);
        assert_eq!(strip_outer_quotes("\""), "\"");
    }

    #[test]
    fn unix_command_match_requires_real_exe_name() {
        let chrome = ["chrome", "google-chrome"];
        assert!(command_uses_exe(
            "/opt/google/chrome/chrome --user-data-dir=/tmp/p",
            &chrome,
            &[]
        ));
        assert!(command_uses_exe(
            r#""/opt/google/chrome/chrome" --no-first-run"#,
            &chrome,
            &[]
        ));
        // 旧实现按裸子串匹配，会把这行也当成浏览器并 SIGTERM。
        assert!(!command_uses_exe(
            "python3 -m http.server --directory /home/me/chromium-downloads",
            &["chromium"],
            &[],
        ));
        // 参数里出现浏览器名字不算。
        assert!(!command_uses_exe(
            "node /srv/app/chrome.js --user-data-dir=/tmp/p",
            &chrome,
            &[],
        ));
        // macOS `.app` 路径带空格，按包前缀识别。
        assert!(command_uses_exe(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome --no-first-run",
            &["chrome"],
            &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"],
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn chromium_process_name_matches_its_exe() {
        // resolve_exe 打开的是 `Chromium\Application\chrome.exe`，进程名必须一致，
        // 否则 instances 永远为空。
        assert_eq!(Kind::Chromium.process_name(), "chrome.exe");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn same_dir_is_case_insensitive_on_default_filesystems() {
        // NTFS / 默认 APFS 都大小写不敏感：`C:/Prof` 与 `C:/prof` 是同一个目录，
        // 漏判会导致实例冲突检测失败。
        assert!(same_dir(
            "C:/Prof/User Data",
            Path::new("c:/prof/user data")
        ));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn which_output_keeps_non_utf8_paths() {
        // 非 UTF-8 字节不能被替换成 U+FFFD，否则返回的路径一定不存在。
        let mut raw = b"/opt/chr".to_vec();
        raw.push(0xFF);
        raw.extend_from_slice(b"ome\n");
        let path = path_from_which_output(&raw).expect("path");
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(path.as_os_str().as_bytes(), b"/opt/chr\xFFome");
        assert!(path_from_which_output(b"  \n").is_none());
    }

    #[test]
    fn browser_exe_must_be_an_executable_file() {
        // 目录：不是普通文件，必须在启动前就报 not_a_file。
        let dir = std::env::temp_dir();
        let err = validate_browser_exe(&dir).unwrap_err().to_string();
        assert!(err.contains("not_a_file"), "{err}");

        // 不存在的路径同样走 not_a_file，而不是留到 spawn 才失败。
        let missing = dir.join("atb-definitely-missing-browser");
        let err = validate_browser_exe(&missing).unwrap_err().to_string();
        assert!(err.contains("not_a_file"), "{err}");

        // 普通文本文件：Windows 缺 .exe、Unix 缺执行位，都应该是 not_executable。
        let plain = dir.join(format!("atb-browser-probe-{}.txt", std::process::id()));
        std::fs::write(&plain, b"not a browser").unwrap();
        let err = validate_browser_exe(&plain).unwrap_err().to_string();
        assert!(err.contains("not_executable"), "{err}");
        let _ = std::fs::remove_file(&plain);
    }
}
