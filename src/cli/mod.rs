//! `atb` 命令行前端。
//!
//! 结构与 daemon 侧一致：参数定义（`args*`）、HTTP 传输（`http`）、退出码与输出契约
//! （`exit`）、每个命令族的实现（`cmd_*`），以及只做分发的 [`run`]。

use crate::launch;
use crate::preflight;
use crate::target_spec::TargetSpec;
use crate::{config, server};
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fs2::FileExt;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

const HOST: &str = "127.0.0.1";
// API 端口只有一个真相：`config::CLI_API_PORT`。
/// 以前这里的字面量与 config 各自维护，改端口时容易只改一处。
pub(crate) const PORT: u16 = config::CLI_API_PORT;

/// 默认 compact 输出（省 token），`--pretty` 时才缩进。
static PRETTY_OUTPUT: AtomicBool = AtomicBool::new(false);
/// 记录本次调用的失败退出码：`ok:false` → 1，job 等待超时 → 3。
static FAILURE_EXIT_CODE: AtomicI32 = AtomicI32::new(0);
/// `--dry-run`：预检通过就输出计划，不产生任何页面副作用。
static DRY_RUN: AtomicBool = AtomicBool::new(false);
/// 调用方身份：多智能体同时使用时隔离默认标签页与清理范围。
static AGENT_ID: OnceLock<String> = OnceLock::new();
/// `--idempotency-key`：进程内缓存，`request` 统一加进请求头，
/// daemon 侧对同 key 的 POST 在 60s 内回放首次响应，保证动作只执行一次。
static IDEMPOTENCY_KEY: OnceLock<String> = OnceLock::new();
/// HTTP 头名必须和 daemon 侧一致。
const AGENT_HEADER: &str = "x-agent-browser-agent";
const EXIT_FAILURE: i32 = 1;
/// 参数 / 预检错误：命令本身写错了，改命令就能解决，和「执行失败」分开。
const EXIT_USAGE: i32 = 2;
const EXIT_JOB_TIMEOUT: i32 = 3;
/// 4 = 结果未知：POST 已发出但响应没回来，动作可能已执行。
/// 和 1（明确失败，可重试）分开，智能体不应盲目重试动作类命令。
const EXIT_UNKNOWN: i32 = 4;

pub(crate) mod agent;
pub(crate) mod args;
pub(crate) mod args_more;
pub(crate) mod args_net;
pub(crate) mod cmd_action;
pub(crate) mod cmd_env;
pub(crate) mod cmd_io;
pub(crate) mod cmd_measure;
pub(crate) mod cmd_net;
pub(crate) mod cmd_open;
pub(crate) mod cmd_page;
pub(crate) mod cmd_pipeline;
pub(crate) mod cmd_schema;
pub(crate) mod cmd_scrape;
pub(crate) mod cmd_status;
pub(crate) mod cmd_tabs;
pub(crate) mod cmd_target;
pub(crate) mod exit;
pub(crate) mod http;
pub(crate) mod trust;

pub(crate) use agent::*;
pub(crate) use args::*;
pub(crate) use args_more::*;
pub(crate) use args_net::*;
pub(crate) use cmd_action::*;
pub(crate) use cmd_io::*;
pub(crate) use cmd_net::*;
pub(crate) use cmd_open::*;
pub(crate) use cmd_page::*;
pub(crate) use cmd_pipeline::*;
pub(crate) use cmd_status::*;
pub(crate) use cmd_tabs::*;
pub(crate) use exit::*;
pub(crate) use http::*;
pub(crate) use trust::*;

pub fn run() -> Result<()> {
    let Some(cli) = parse_cli()? else {
        // `--help` / `--version` 已经按 clap 的原样打印完毕，退出码 0。
        return Ok(());
    };
    let is_async = cli.r#async;
    let _ = AGENT_ID.set(resolve_agent_id(cli.agent.as_deref()));
    // 幂等键 trim 后为空视同未传：避免空 header 让 daemon 侧缓存出一个空 key。
    if let Some(key) = cli
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        let _ = IDEMPOTENCY_KEY.set(key.to_string());
    }
    PRETTY_OUTPUT.store(cli.pretty, Ordering::Relaxed);
    DRY_RUN.store(cli.dry_run, Ordering::Relaxed);
    // 分发层统一拦截：两者都在执行前判定，避免「flag 写了但被静默忽略」。
    // 同时给出 --dry-run 与 --async 时不支持任一者，报第一个（--dry-run）即可。
    reject_dry_run_if_unsupported(&cli.command)?;
    reject_async_if_unsupported(&cli.command, is_async)?;
    match cli.command {
        CommandKind::Daemon => cmd_open::cmd_daemon(is_async),
        CommandKind::Tabs(args) => cmd_tabs::cmd_tabs(args, is_async),
        CommandKind::Focus(args) => cmd_target::cmd_focus(args, is_async),
        CommandKind::Use(args) => cmd_target::cmd_use(args, is_async),
        CommandKind::Tabtree(args) => cmd_tabs::cmd_tabtree(args, is_async),
        CommandKind::Lookup(args) => run_lookup_command(args),
        CommandKind::ProfileLabel(args) => run_profile_label_command(args),
        CommandKind::Scan(args) => cmd_page::cmd_scan(args, is_async),
        CommandKind::Exec(args) => cmd_page::cmd_exec(args, is_async),
        CommandKind::Cookies(args) => cmd_page::cmd_cookies(args, is_async),
        CommandKind::Upload(args) => cmd_page::cmd_upload(args, is_async),
        CommandKind::Snapshot(args) => cmd_page::cmd_snapshot(args, is_async),
        CommandKind::Click(args) => run_target_command("/click", "click", args, is_async),
        CommandKind::MouseClick(args) => cmd_action::cmd_mouse_click(args, is_async),
        CommandKind::Fill(args) => run_fill_command(args, is_async),
        CommandKind::SendKeys(args) => run_send_keys_command(args, is_async),
        CommandKind::Screenshot(args) => run_screenshot_command(args, is_async),
        CommandKind::SavePdf(args) => run_save_pdf_command(args, is_async),
        CommandKind::Network(args) => run_network_command(args),
        CommandKind::Console(args) => run_console_command(args),
        CommandKind::Job(args) => run_job_command(args),
        CommandKind::Run(args) => cmd_pipeline::cmd_run(args, is_async),
        CommandKind::Batch(args) => cmd_pipeline::cmd_batch(args, is_async),
        CommandKind::Read(args) => cmd_page::cmd_read(args, is_async),
        CommandKind::Measure(args) => cmd_measure::cmd_measure(args, is_async),
        CommandKind::Find(args) => cmd_page::cmd_find(args, is_async),
        CommandKind::Hover(args) => run_action_command("hover", args, is_async),
        CommandKind::Select(args) => run_action_command("select", args, is_async),
        CommandKind::Check(args) => run_action_command("check", args, is_async),
        CommandKind::Scroll(args) => run_action_command("scroll", args, is_async),
        CommandKind::Inject(args) => cmd_page::cmd_inject(args, is_async),
        CommandKind::Userscript(args) => cmd_measure::cmd_userscript(args, is_async),
        CommandKind::Browsers => cmd_open::cmd_browsers(is_async),
        CommandKind::Launch(args) => run_launch(args),
        CommandKind::Broadcast(args) => run_broadcast(args),
        CommandKind::Downloads(args) => run_downloads(args),
        CommandKind::Download(args) => run_download(args),
        CommandKind::DownloadWait(args) => run_download_wait(args),
        CommandKind::Net(cmd) => run_net_command(cmd),
        CommandKind::Frames(args) => cmd_page::cmd_frames(args, is_async),
        CommandKind::Wait(args) => cmd_action::cmd_wait(args, is_async),
        CommandKind::Challenge(args) => cmd_action::cmd_challenge(args, is_async),
        CommandKind::Stealth(args) => cmd_action::cmd_stealth(args, is_async),
        CommandKind::Back(args) => {
            run_navigation_command("back", "history.back(); return 'back'", args, is_async)
        }
        CommandKind::Reload(args) => run_navigation_command(
            "reload",
            "setTimeout(() => location.reload(), 50); return 'reload'",
            args,
            is_async,
        ),
        CommandKind::Open(args) => cmd_open::cmd_open(args, is_async),
        CommandKind::Cleanup(args) => cmd_open::cmd_cleanup(args, is_async),
        CommandKind::Close(args) => cmd_open::cmd_close(args, is_async),
        CommandKind::Scrape(args) => cmd_scrape::cmd_scrape(args, is_async),
        CommandKind::Status => cmd_status::cmd_status(is_async),
        CommandKind::Env(args) => cmd_env::cmd_env(args, is_async),
        CommandKind::Extensions(args) => cmd_env::cmd_extensions(args, is_async),
        CommandKind::Logs(args) => print_logs(args.tail),
        CommandKind::Schema(args) => cmd_schema::cmd_schema(args, is_async),
        CommandKind::Capabilities => cmd_status::cmd_capabilities(is_async),
        CommandKind::Doctor(args) => cmd_status::cmd_doctor(args, is_async),
        CommandKind::InstallSkill(args) => install_skill(args),
        CommandKind::SetExtensionPort(args) => cmd_open::cmd_set_extension_port(args, is_async),
        CommandKind::TrustExtension(args) => run_trust_extension(args),
        CommandKind::Stop => cmd_open::cmd_stop(is_async),
        CommandKind::Restart(args) => cmd_open::cmd_restart(args, is_async),
    }
}

/// 解析命令行，并把 clap 的错误也纳入输出契约。
///
/// 旧实现直接 `Cli::parse()`：clap 会把纯文本错误打到 stderr 并自己 `process::exit(2)`，
/// 智能体的 JSON 解析器拿到的是一段人读文本，退出码也绕过了 `exit_code()`。
/// 现在：`--help` / `--version` 保持原样（正常打印、退出 0），其余一律
/// `{"ok":false,"error_code":"invalid_argument",...}` + 退出码 2。
fn parse_cli() -> Result<Option<Cli>> {
    let err = match Cli::try_parse() {
        Ok(cli) => return Ok(Some(cli)),
        Err(err) => err,
    };
    use clap::error::ErrorKind;
    match err.kind() {
        // 只有显式的 --help / --version 算「正常输出」；缺子命令等仍是用法错误（exit 2）。
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = err.print();
            Ok(None)
        }
        _ => {
            // `err.to_string()` 含 "error: ..." 与 usage 两段，去掉尾部空白即可。
            let message = err.to_string().trim().to_string();
            Err(usage_failure(
                "invalid_argument",
                message,
                "atb <cmd> --help 查看该命令的用法；atb schema 可拿到全部命令的机器可读定义",
            ))
        }
    }
}

/// `--dry-run` 只有真正实现了「只出计划」的命令才支持。
/// 以前全局限定在这十几条命令上是**静默无效**的：`atb net mock ... --dry-run` 照样
/// 装上 mock 规则、`atb download --dry-run` 照样开始下载——flag 的承诺是「不做动作」，
/// 静默照做比报错更危险。这里在分发前统一判定，返回退出码 2。
/// `install-skill` 已改为真正支持 dry-run（见 `cmd_io::install_skill`），不在此列。
fn reject_dry_run_if_unsupported(command: &CommandKind) -> Result<()> {
    if !DRY_RUN.load(Ordering::Relaxed) {
        return Ok(());
    }
    let name = match command {
        // `net log` / `net routes` 只读，照旧放行。
        CommandKind::Net(command) => match command.action {
            NetAction::Fetch(_) => "net fetch",
            NetAction::Hook(_) => "net hook",
            NetAction::Mock(_) => "net mock",
            NetAction::Unroute(_) => "net unroute",
            NetAction::Reset(_) => "net reset",
            NetAction::Log(_) | NetAction::Routes(_) => return Ok(()),
        },
        // `list` / `detail` / `clear` 不产生页面副作用，只拦会装监听/停监听的 start/stop。
        CommandKind::Network(command) => match command.action {
            NetworkAction::Start(_) => "network start",
            NetworkAction::Stop(_) => "network stop",
            _ => return Ok(()),
        },
        CommandKind::Console(command) => match command.action {
            ConsoleAction::Start(_) => "console start",
            ConsoleAction::Stop(_) => "console stop",
            _ => return Ok(()),
        },
        CommandKind::Download(_) => "download",
        // `broadcast --dry-run` 已由 `cmd_io::run_broadcast` 实现本地计划
        // （只解析 op/args 并输出将要广播的内容，不拉 daemon、不碰页面），
        // 不能再在这里 exit 2 把它的 dry-run 挡掉。
        CommandKind::ProfileLabel(_) => "profile-label",
        // `launch` 会真正启动/重启浏览器（`--restart` 还会关掉现有实例），
        // 与 `--dry-run` 的「不做动作」承诺直接冲突，一并拒绝。
        CommandKind::Launch(_) => "launch",
        _ => return Ok(()),
    };
    Err(usage_failure(
        "unsupported_flag",
        format!("{name} 不支持 --dry-run（该命令会立即产生页面/浏览器副作用）"),
        "去掉 --dry-run；先用 atb schema --json 或 status 做计划确认",
    ))
}

/// `--async` 只在一部分命令上实现了后台任务。
///
/// 其余命令以前会**静默忽略**这个全局 flag（照常同步跑、也不返回 job_id），
/// 调用方以为拿到了 job_id 却读不到。这里统一按用法错误拒绝。
/// 清单与 `cmd_*` 里各 handler 的实现保持一致：真正支持 --async 的（scan/fill/click…）
/// 不在清单内，行为不变。
fn reject_async_if_unsupported(command: &CommandKind, is_async: bool) -> Result<()> {
    let name = match command {
        CommandKind::Lookup(_) => "lookup",
        CommandKind::ProfileLabel(_) => "profile-label",
        CommandKind::Network(_) => "network",
        CommandKind::Console(_) => "console",
        CommandKind::Job(_) => "job",
        CommandKind::Downloads(_) => "downloads",
        CommandKind::Download(_) => "download",
        CommandKind::DownloadWait(_) => "download-wait",
        CommandKind::Broadcast(_) => "broadcast",
        CommandKind::Launch(_) => "launch",
        CommandKind::Logs(_) => "logs",
        // net 各子动作都没有 job 实现，统一在分发层拒绝。
        CommandKind::Net(_) => "net",
        // 这两条命令既不接收也不实现 --async。以前落到 `_ => Ok(())`，
        // 全局 flag 被静默吞掉：调用方以为拿到了 job_id，实际拿到的是同步结果。
        CommandKind::InstallSkill(_) => "install-skill",
        CommandKind::TrustExtension(_) => "trust-extension",
        // 2026-11-13：这些新命令不走 job 通道，显式拒绝而不是静默吞掉 flag。
        CommandKind::Focus(_) => "focus",
        CommandKind::Use(_) => "use",
        CommandKind::Env(_) => "env",
        CommandKind::Extensions(_) => "extensions",
        CommandKind::Measure(_) => "measure",
        CommandKind::Userscript(_) => "userscript",
        _ => return Ok(()),
    };
    reject_async_if_set(name, is_async)
}

/// 本次调用的退出码（0 成功 / 1 执行失败 / 2 用法错误 / 3 job 超时 / 4 结果未知）。
///
/// `ATB_EXIT0`（旧名 `AGENT_BROWSER_CLI_EXIT0`）保留兼容：以前只要**设置了**就恒 0，
/// 于是 `ATB_EXIT0=0` / `ATB_EXIT0=` 也被当成开启。现在要求 trim 后是 `1` 或 `true`。
pub fn exit_code() -> i32 {
    let code = FAILURE_EXIT_CODE.load(Ordering::Relaxed);
    if code != 0 && exit0_compat() {
        return 0;
    }
    code
}

fn exit0_compat() -> bool {
    crate::settings::exit0_enabled()
}
