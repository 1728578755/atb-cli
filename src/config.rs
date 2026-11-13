use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_EXTENSION_PORT: u16 = 18765;
pub const CLI_API_PORT: u16 = 18767;
pub const API_TOKEN_HEADER: &str = "x-agent-browser-token";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub extension_port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            extension_port: DEFAULT_EXTENSION_PORT,
        }
    }
}

/// 只读加载配置：文件不存在就返回默认值，**不创建、不写盘**。
///
/// 只读命令（`status` 等）必须用这个，不要用 `load_or_create`：
/// 旧实现会在只读 HOME / 容器里因为写盘失败让整条只读命令直接失败，也会给只读
/// 命令留下创建 `~/.atb/config.json` 的副作用。需要「首次运行就落盘」的入口
/// （`set-extension-port`、daemon 启动）才用 `load_or_create`。
// 旧注释声称 cmd_status 尚未切换到本函数，实际 cmd_status.rs 已改用 config::load()
// 过期的 allow(dead_code) 连同该注释一并删除，避免误导后来者。
pub fn load() -> Result<AppConfig> {
    let path = config_path()?;
    match read_json_with_backup::<AppConfig>(&path)? {
        Some(config) => {
            validate_port(config.extension_port)?;
            Ok(config)
        }
        // 文件与 .bak 都不存在 = 从没配置过，按默认值工作但不落盘。
        None => Ok(AppConfig::default()),
    }
}

/// 加载配置；首次运行（目标与 `.bak` 都不存在）时创建默认配置。
///
/// 只在**确实要写配置**的入口使用。只读命令请用 `load`。
pub fn load_or_create() -> Result<AppConfig> {
    let path = config_path()?;
    let blank = is_blank_file(&path);
    match read_json_with_backup::<AppConfig>(&path) {
        Ok(Some(config)) => {
            validate_port(config.extension_port)?;
            Ok(config)
        }
        // 目标与 `.bak` 都不存在 = 首次运行。
        Ok(None) => {
            let config = AppConfig::default();
            save(&config)?;
            Ok(config)
        }
        // 空文件更可能是「上次写入被截断」，不是「首次运行」：先备份再重建。
        // 旧实现直接 `save(default)`，会把用户自定义的 extension_port 静默重置；
        // 而 `.bak` 里若是完整配置，上面那条分支已经优先恢复它了。
        Err(_) if blank => {
            quarantine_corrupt(&path)?;
            let config = AppConfig::default();
            save(&config)?;
            Ok(config)
        }
        // 目标有内容但写坏了、且没有可用备份：如实报错，不覆盖用户数据。
        Err(err) => Err(err),
    }
}

/// 文件存在但内容全是空白 → 视为「被截断」，不视为「首次运行」。
fn is_blank_file(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(content) => content.trim().is_empty(),
        Err(_) => false,
    }
}

/// 把损坏的文件挪到一边（`config.json.corrupt-<时间戳>`）。
///
/// 只在确定要重建的写入口调用；挪不走就报错，绝不直接覆盖原文件。
fn quarantine_corrupt(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let backup = path.with_file_name(format!("{name}.corrupt-{}", unique_nonce()));
    fs::rename(path, &backup)
        .with_context(|| format!("备份损坏的配置文件失败（不覆盖原文件）: {}", path.display()))?;
    Ok(())
}

pub fn load_existing() -> Result<AppConfig> {
    let path = config_path()?;
    let config = read_json_with_backup::<AppConfig>(&path)?
        .ok_or_else(|| anyhow!("配置文件不存在: {}", path.display()))?;
    validate_port(config.extension_port)?;
    Ok(config)
}

pub fn save(config: &AppConfig) -> Result<()> {
    validate_port(config.extension_port)?;
    let path = config_path()?;
    let content = serde_json::to_string_pretty(config)? + "\n";
    // 走带跨进程锁的写入：并发 save 不会互相截断，替换期间也不会让读方
    // 读到写了一半的 JSON。
    write_file_locked(&path, content.as_bytes(), None)
}

pub fn set_extension_port(port: u16) -> Result<AppConfig> {
    validate_port(port)?;
    let mut config = load_or_create()?;
    config.extension_port = port;
    save(&config)?;
    Ok(config)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(user_config_dir()?.join("config.json"))
}

/// 读取环境变量：新前缀 `ATB_` 优先，旧前缀 `AGENT_BROWSER_CLI_` 继续生效。
///
/// 命令改名为 `atb` 之后，用户已经把 `AGENT_BROWSER_CLI_*` 写进 shell 配置，
/// 直接换前缀等于静默失效（尤其是 NO_DEBUGGER 这种安全开关，失效了反而更危险）。
/// 所以两套前缀都读，文档只宣传新的。
pub fn env_compat(suffix: &str) -> Option<std::ffi::OsString> {
    if let Some(value) = env::var_os(format!("ATB_{suffix}")) {
        return Some(value);
    }
    env::var_os(format!("AGENT_BROWSER_CLI_{suffix}"))
}

// 用户主目录的**唯一**解析入口。
///
/// 旧实现有三套口径：配置用 `HOME`/`USERPROFILE`、`~` 展开与 daemon cwd 用
/// `dirs::home_dir()`。Windows Git-Bash / WSL 下 `HOME` 可能指向 `/home/...`，
/// 与 Known Folder 不一致，`~` 和 `install-skill` 会落到错误位置。这里统一走
/// `dirs::home_dir()`（Windows 用 Known Folder API，Unix 用 `$HOME`），调用方
/// 只应在明确需要「用户主目录」时使用它。
pub(crate) fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn user_config_dir() -> Result<PathBuf> {
    // `ATB_HOME` 直接指定配置目录：测试桩（mock 扩展自检）必须能跑一份**独立**的
    // daemon，否则会撞上真实 daemon 的 18765/18767。Windows 上 `dirs::home_dir()`
    // 走的是 Known Folder API，改 HOME/USERPROFILE 不生效，只能靠显式环境变量。
    if let Some(dir) = crate::settings::atb_home() {
        return Ok(dir);
    }
    let home = home_dir().ok_or_else(|| anyhow!("无法定位用户主目录，不能创建 atb 配置文件"))?;
    Ok(home.join(".atb"))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(user_config_dir()?.join("logs"))
}

pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(log_dir()?.join("daemon.log"))
}

pub fn api_token_path() -> Result<PathBuf> {
    Ok(user_config_dir()?.join("api-token"))
}

/// `net` 规则（driver.net_hooks）的落盘路径，与 ws-trust.json 同目录。
///
/// 结构：`{ "<browser:profile>": <net 配置 Value> }`。daemon 重启后读回，
/// 避免「页面里的 hook 还在、`net routes` 却读到空」的配置漂移。
pub fn net_hooks_path() -> Result<PathBuf> {
    Ok(user_config_dir()?.join("net-hooks.json"))
}

/// 读取全部 net 规则；文件不存在视为空表。
pub fn load_net_hooks() -> Result<HashMap<String, serde_json::Value>> {
    load_net_hooks_from(&net_hooks_path()?)
}

fn load_net_hooks_from(path: &Path) -> Result<HashMap<String, serde_json::Value>> {
    // 用 JSON 版本的回退：内容被截断成非法 JSON 时也要读 `.bak`。
    match read_json_with_backup::<HashMap<String, serde_json::Value>>(path) {
        Ok(Some(map)) => Ok(map),
        Ok(None) => Ok(HashMap::new()),
        // `{err:#}` 展开整条 error chain：只包一层 context 会让调用方丢掉
        // 「解析失败」这个根因（原本靠 to_string 判断错误类型的代码会误判）。
        Err(err) => Err(anyhow!("读取 net-hooks 失败: {}：{err:#}", path.display())),
    }
}

/// 覆盖写入全部 net 规则（临时文件 + 原子替换，权限 0600）。
pub fn save_net_hooks(map: &HashMap<String, serde_json::Value>) -> Result<()> {
    save_net_hooks_to(&net_hooks_path()?, map)
}

fn save_net_hooks_to(path: &Path, map: &HashMap<String, serde_json::Value>) -> Result<()> {
    let content = serde_json::to_string_pretty(map)? + "\n";
    write_file_locked(path, content.as_bytes(), Some(0o600))?;
    // 0o600 只在 Unix 生效；Windows 上由 write_file_locked 内部的 ACL 收紧负责，
    // 这里再对最终文件和 `.bak` 补一次。
    #[cfg(windows)]
    if let Err(err) = restrict_file_and_backup_acl(path) {
        eprintln!("警告: {err}");
    }
    Ok(())
}

pub fn load_api_token() -> Result<Option<String>> {
    let path = api_token_path()?;
    // token 文件**不**走通用 `.bak` 回退。`replace_file_atomically` 的
    // 退化路径会先把旧目标改名成 `.bak`，如果读方在真空期回退去读 `.bak`，就会拿到
    // 轮换前的旧 token，随后被 daemon 拒绝（表现为随机的 401）。读方只信 target：
    // 缺失就返回 `None`，由调用方重新生成/重读。
    //
    // 与 `cleanup_stale_backup` 共用同一把跨进程锁：写方持有排他锁时读方会等待，
    // 不会再读到「目标刚被移走、备份马上被删」的中间态。
    let content = read_token_target_locked(&path)
        .with_context(|| format!("读取 API token 失败: {}", path.display()))?;
    let token = content.trim();
    if token.is_empty() {
        // 空 token 文件等价于「尚未生成 token」，不要让整条命令直接失败。
        return Ok(None);
    }
    Ok(Some(token.to_string()))
}

/// 在共享锁下读 token 目标文件；不存在返回空串（等价于 `Ok(None)`）。
fn read_token_target_locked(path: &Path) -> std::io::Result<String> {
    // 锁文件打不开（目录不可写等）不阻断读取：拿不到锁只意味着不能与写方互斥，
    // 比让只读命令直接失败更合理。
    let lock = open_lock_file(&lock_path(path)).ok();
    if let Some(lock) = &lock {
        let _ = fs2::FileExt::lock_shared(lock);
    }
    let result = match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err),
    };
    if let Some(lock) = &lock {
        let _ = fs2::FileExt::unlock(lock);
    }
    result
}

pub fn save_api_token(token: &str) -> Result<()> {
    if token.is_empty() {
        return Err(anyhow!("API token 不能为空"));
    }
    let path = api_token_path()?;
    let mut content = token.as_bytes().to_vec();
    content.push(b'\n');
    write_file_locked(&path, &content, Some(0o600))?;
    // Unix 上 0o600 就够了；Windows 上文件默认继承磁盘 ACL，同机其他用户可以直接读
    // `~/.atb/api-token` 并劫持本地 HTTP 端口（旧报告 #7）。这里用系统自带 icacls
    // 收紧权限，比为了改 ACL 引入 windows-sys 依赖便宜得多。`.bak` 同样处理。
    #[cfg(windows)]
    if let Err(err) = restrict_file_and_backup_acl(&path) {
        eprintln!("警告: {err}");
    }
    Ok(())
}

/// 确保 daemon 日志文件存在，并把它收紧到「只有当前用户可读写」。
///
/// `daemon.log` 会接住 daemon 的全部 stdout/stderr（URL、错误上下文、会话信息）。
/// 旧实现用默认权限创建：Unix 上是 0644（同机其他用户可读），Windows 上继承目录
/// ACL。这里与 `api-token` 保持一致：目录 0700、文件 0600（Unix）+ ACL（Windows）。
pub fn ensure_log_file() -> Result<PathBuf> {
    let path = daemon_log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建日志目录失败: {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = fs::set_permissions(parent, fs::Permissions::from_mode(0o700)) {
                eprintln!(
                    "警告: 无法把日志目录收紧到 0700（{}）: {err}",
                    parent.display()
                );
            }
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("创建日志文件失败: {}", path.display()))?;
    drop(file);
    // 旧版本创建的文件可能已经是宽松权限：每次启动都再收紧一次。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            eprintln!(
                "警告: 无法把 daemon.log 收紧到 0600（{}）: {err}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    if let Err(err) = restrict_windows_acl(&path) {
        eprintln!("警告: {err}");
    }
    Ok(path)
}

/// 进程内的写串行化。
///
/// 文件锁负责**跨进程**互斥，这把 mutex 负责**同进程多线程**：同进程多个线程同时
/// 打开同一个锁文件再抢 OS 锁，在 Windows 上会踩到「锁文件刚被别的线程打开」的
/// 瞬时拒绝访问，既没必要也不稳定。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 写文件的统一入口：跨进程排他锁 → 唯一临时文件 → 替换。
///
/// 锁解决的是并发写互相截断（同进程两个线程共用 `.config-{pid}.tmp`）和
/// **Windows 上 rename 组合的真空期**：先 `rename(target → .bak)` 再
/// `rename(temp → target)`，中间目标不存在。锁不能把这两步变成真原子，但能把
/// 「别的写方在真空期读到目标缺失」的窗口关掉。不引入 Win32 API（ReplaceFileW /
/// MoveFileEx）是为了不新增依赖。
fn write_file_locked(target: &Path, content: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("路径缺少父目录: {}", target.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "创建配置目录失败（请检查该目录的写权限）: {}",
            parent.display()
        )
    })?;

    // 先拿进程内锁，避免同进程线程之间去抢同一个锁文件。
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let lock_path = lock_path(target);
    let lock = open_lock_file(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("获取写锁失败: {}", lock_path.display()))?;

    let temp_path = temp_path_for(target);
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        // create_new：临时文件名已经带唯一后缀，万一撞上也不复用别人的半成品。
        options.create_new(true).write(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        // Windows 上没有 unix mode：0o600 的语义（只有当前用户可读写）改用 ACL 收紧。
        // 必须在写入内容**之前**完成：旧实现先 write/sync 再收紧，中间存在
        // 一段「临时文件已是明文、ACL 还是默认继承」的窗口，同机其他用户可读走 token。
        // 改名会保留文件自己的安全描述符，所以这里收紧后就不会再变回宽松。
        #[cfg(windows)]
        let restrict_acl = mode == Some(0o600);
        let mut file = options
            .open(&temp_path)
            .with_context(|| format!("创建临时文件失败: {}", temp_path.display()))?;
        #[cfg(windows)]
        if restrict_acl {
            if let Err(err) = restrict_windows_acl(&temp_path) {
                eprintln!("警告: {err}");
            }
        }
        file.write_all(content)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        // Windows 上打开中的文件无法被 rename，先显式关闭句柄。
        drop(file);
        replace_file_atomically(&temp_path, target)
            .with_context(|| format!("写入文件失败: {}", target.display()))?;
        Ok(())
    })();

    // 无论成败都解锁；锁文件本身留在目录里（下一次写复用）。
    let _ = fs2::FileExt::unlock(&lock);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// 打开（必要时创建）锁文件。
///
/// Windows 上另一个进程刚打开/关闭同名文件时，`CreateFileW` 偶尔返回
/// `ERROR_ACCESS_DENIED`；短暂重试即可，不该让一次配置保存直接失败。
fn open_lock_file(path: &Path) -> Result<fs::File> {
    // 默认 40 次 × 25ms ≈ 1s（旧实现 10 × 20ms ≈ 200ms）：杀软扫描 / 慢盘上
    // 200ms 就放弃会让 token 写入失败、daemon 起不来。可用
    // `ATB_LOCK_RETRIES` / `ATB_LOCK_RETRY_MS` 调整。
    let retries = crate::settings::lock_retries();
    let retry_ms = crate::settings::lock_retry_ms();
    let mut last_error = None;
    for _ in 0..retries {
        match OpenOptions::new()
            .create(true)
            // 锁文件只用于加排他锁，内容无关；显式声明「不截断」，
            // 避免不同 OpenOptions 组合在不同平台产生歧义（clippy 提示）。
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                last_error = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(retry_ms));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("打开写锁失败: {}", path.display()))
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("打开写锁失败"))
        .context(format!("打开写锁失败: {}", path.display())))
}

/// 进程内递增的临时文件计数器（配合时间戳保证同一纳秒内也不冲突）。
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 唯一后缀：pid + 线程 + 纳秒时间 + 进程内计数器。
///
/// 旧实现只用 pid（`.config-{pid}.tmp`）：同进程两个线程并发 save 会打开同一个
/// 临时文件，互相截断对方的 JSON。
fn unique_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let thread = format!("{:?}", std::thread::current().id())
        .replace(['(', ')'], "")
        .replace(' ', "");
    format!("{}-{}-{}-{}", std::process::id(), thread, nanos, counter)
}

fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!(".{name}-{}.tmp", unique_nonce()))
}

/// 与目标同目录的跨进程锁文件（`api-token` → `api-token.lock`）。
fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!("{name}.lock"))
}

/// Windows：去掉继承权限与已知宽松 ACE，只保留当前用户可读写。
///
/// 身份优先用当前进程的 SID（`whoami /user` 解析出 `S-1-5-21-...`，用 icacls 的
// `*S-1-...` 形式授权），拿不到才退回 `USERDOMAIN\USERNAME`。
/// 旧实现只用可伪造的 `USERNAME`，且在已有显式 ACE 时 `grant:r` 不会移除那些宽松项。
///
/// 失败返回 `Err` 交由调用方打印警告：权限收紧是加固动作，不该让 daemon 起不来，
/// 但也绝不能静默失败。
#[cfg(windows)]
fn restrict_windows_acl(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let identity = current_user_identity().ok_or_else(|| {
        format!(
            "读不到当前用户身份，无法收紧 {} 的权限（同机其他用户可能可读）",
            path.display()
        )
    })?;

    let no_window = |command: &mut std::process::Command| {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    };

    // 1) 去掉继承来的 ACE（不展开成显式 ACE）。
    let mut inheritance = std::process::Command::new("icacls");
    inheritance.arg(path).arg("/inheritance:r");
    no_window(&mut inheritance);
    let inheritance_status = inheritance.status();

    // 2) 移除显式宽松 ACE：Everyone / Users / Authenticated Users / INTERACTIVE。
    //    用 well-known SID 而不是本地化组名，避免非英文 Windows 上匹配不到。
    let mut remove = std::process::Command::new("icacls");
    remove.arg(path).args([
        "/remove:g",
        "*S-1-1-0",
        "*S-1-5-32-545",
        "*S-1-5-11",
        "*S-1-5-4",
    ]);
    no_window(&mut remove);
    let remove_status = remove.status();

    // 3) 只给当前用户完全控制。
    let grant = format!("{identity}:F");
    let mut command = std::process::Command::new("icacls");
    command.arg(path).args(["/grant:r", &grant]);
    no_window(&mut command);
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "收紧 {} 的权限失败（icacls 退出码 {:?}，继承清理 {:?} / 移除宽松 ACE {:?}），\
             同机其他用户可能能读到这个文件",
            path.display(),
            status.code(),
            inheritance_status.as_ref().ok().and_then(|s| s.code()),
            remove_status.as_ref().ok().and_then(|s| s.code())
        )),
        Err(err) => Err(format!(
            "收紧 {} 的权限失败（无法执行 icacls: {err}），同机其他用户可能能读到这个文件",
            path.display()
        )),
    }
}

/// 当前用户身份：优先 `*S-1-...`（icacls 的 SID 形式），否则 `DOMAIN\User`。
#[cfg(windows)]
fn current_user_identity() -> Option<String> {
    if let Some(sid) = current_user_sid() {
        return Some(format!("*{sid}"));
    }
    let user = env::var("USERNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    match env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => Some(format!("{}\\{}", domain.trim(), user)),
        _ => Some(user),
    }
}

/// 从 `whoami /user /fo csv /nh` 的输出里取当前用户 SID。
#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    let output = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split(|ch: char| ch == '"' || ch == ',' || ch.is_whitespace())
        .map(str::trim)
        .find(|token| token.starts_with("S-1-") && token.len() > 4)
        .map(str::to_string)
}

/// 给目标文件和它的 `.bak` 都收紧权限。
///
/// 只收紧目标是不够的：`.bak` 里可能是上一版 token / 配置，权限宽松等于白收紧
// 主文件。`pub(crate)` 是给 `cli/trust.rs` 复用：
/// 对 `.tmp`、最终文件、`.bak` 都可直接调用。
#[cfg(windows)]
pub(crate) fn restrict_file_and_backup_acl(path: &Path) -> Result<(), String> {
    let mut first_error = None;
    if let Err(err) = restrict_windows_acl(path) {
        first_error = Some(err);
    }
    let backup = backup_path(path);
    if backup.exists() {
        if let Err(err) = restrict_windows_acl(&backup) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 原子替换时使用的备份路径（同目录，`api-token` → `api-token.bak`）。
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// 读取文件；目标不存在或为空时回退读 `.bak`。
///
/// Windows 上的原子替换是「备份 → 替换 → 删备份」三步，若进程在替换与删备份之间退出，
/// 只有 `.bak` 里还留着旧内容。回退读保证崩溃/并发之后 token 与配置不会凭空消失。
///
/// 公开给 `server.rs`：`ws-trust.json` 的读取也必须走这条路径，否则「目标暂时不存在、
/// 备份里还有旧配置」会被当成「从没配过」，安全白名单静默退化成兼容模式（fail-open）。
/// JSON 文件请优先用 `read_json_with_backup`：那条路径连「读到了但解析失败」也会回退。
pub fn read_with_backup(path: &Path) -> std::io::Result<String> {
    match fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => Ok(content),
        // 空文件 = 写入过程中被截断，优先用备份。
        Ok(_) => match fs::read_to_string(backup_path(path)) {
            Ok(backup) => Ok(if backup.trim().is_empty() {
                String::new()
            } else {
                backup
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err),
        },
        Err(err) if err.kind() == ErrorKind::NotFound => fs::read_to_string(backup_path(path)),
        // 权限等其他错误照旧上抛，避免掩盖真实问题。
        Err(err) => Err(err),
    }
}

/// 读 JSON 文件，带 `.bak` 回退。
///
/// - 目标不存在、内容为空、或解析失败（写到一半 / 被截断）→ 回退 `.bak`；
/// - 两者都不存在 → `Ok(None)`；
/// - 只在「确实没有任何可用副本」时才报错。
///
/// 读路径**不写盘**：只读命令（status）不能因为修数据而产生副作用。
/// `.bak` 里的旧内容会被当作有效结果返回，下一次 `save` 时自然会把它写回目标。
pub fn read_json_with_backup<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let mut target_error: Option<String> = None;
    match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => {
            target_error = Some("文件为空（可能刚被截断）".to_string());
        }
        Ok(content) => match serde_json::from_str::<T>(&content) {
            Ok(value) => return Ok(Some(value)),
            Err(err) => target_error = Some(err.to_string()),
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("读取文件失败: {}", path.display())),
    }

    let backup = backup_path(path);
    match fs::read_to_string(&backup) {
        Ok(content) if content.trim().is_empty() => {}
        Ok(content) => match serde_json::from_str::<T>(&content) {
            Ok(value) => return Ok(Some(value)),
            Err(err) => {
                return Err(anyhow!(
                    "解析配置文件失败: {}（{}）；备份 {} 也解析失败（{err}）",
                    path.display(),
                    target_error.unwrap_or_else(|| "目标不存在".to_string()),
                    backup.display()
                ))
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("读取备份文件失败: {}", backup.display()))
        }
    }

    match target_error {
        Some(err) => Err(anyhow!("解析配置文件失败: {}（{err}）", path.display())),
        None => Ok(None),
    }
}

/// 用已经写好的临时文件替换目标文件。
///
/// 调用方需保证临时文件与目标在**同一目录**（跨盘 rename 会失败），并已把内容
/// `sync_all` 落盘。`ws-trust.json` 等信任相关配置也走这里，避免「先删后改名」
/// 在 Windows 上把安全配置删成「未配置」而 fail-open。
///
/// Windows 上仍然是 `rename` 组合（不是真正的原子替换），真空期由调用方持有的
/// `write_file_locked` 跨进程锁覆盖；不引入 Win32 API 是为了不新增依赖。
pub fn replace_file_atomically(temp_path: &Path, target: &Path) -> std::io::Result<()> {
    // Unix: rename 直接覆盖目标，本身就原子。
    #[cfg(not(windows))]
    {
        fs::rename(temp_path, target)
    }
    // Windows: 先用 Win32 的 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` —— 同一卷上它是
    // 原子替换，读方任何时刻看到的要么是旧文件、要么是新文件，不会出现「目标不存在」的
    // 真空期。
    #[cfg(windows)]
    {
        if let Err(err) = move_file_replace(temp_path, target) {
            // 目标被其它进程短暂占用（杀软扫描、编辑器持有句柄）时 MoveFileExW 会失败。
            // 这时退回「备份 + 改名」：牺牲原子性换可用性，但绝不让目标文件消失。
            // 打一行日志，避免「为什么这条路径没走原子替换」变成悬案；带上路径和错误码，
            // 出问题时能直接定位是哪个文件、哪种 Win32 错误。
            eprintln!(
                "原子替换失败（错误码 {:?}，{err}），退回备份+改名：{} -> {}",
                err.raw_os_error(),
                temp_path.display(),
                target.display()
            );
            return replace_file_with_backup(temp_path, target);
        }
        // 原子替换成功：目标已经是新内容，上一轮退化路径留下的 `.bak` 属于陈旧副本，
        // 现在清掉，避免它一直躺在配置目录里（里面可能是旧 token / 旧配置）。
        cleanup_stale_backup(target);
        Ok(())
    }
}

/// 原子替换成功后清理**陈旧**的 `.bak`。
///
/// 只在目标文件已经写入成功时调用：此时 `.bak` 里是更旧的内容，读路径不会再需要它。
/// 删不掉不影响结果（下次写入还会尝试），但必须留警告，别让残留副本悄无声息地留着。
///
/// 锁：本函数在 `write_file_locked` 的排他锁内被调用；token 读方
/// （`read_token_target_locked`）在同一把 `*.lock` 上取共享锁，因此不会出现
// 「读方刚发现目标缺失、备份就被删掉」的竞态。若有调用方直接调用
/// `replace_file_atomically`，需自行持有同一把锁。
#[cfg(windows)]
fn cleanup_stale_backup(target: &Path) {
    let backup = backup_path(target);
    if !backup.exists() {
        return;
    }
    if let Err(err) = fs::remove_file(&backup) {
        if err.kind() != ErrorKind::NotFound {
            eprintln!(
                "警告: 写入成功但无法清理陈旧备份 {}: {err}（同目录残留旧内容，请手工删除）",
                backup.display()
            );
        }
    }
}

/// Windows 上的原子替换：`MoveFileExW` + `MOVEFILE_REPLACE_EXISTING`。
///
/// `std::fs::rename` 在 Windows 上不能覆盖已存在的目标，这是它必须走 Win32 API 的原因。
/// `MOVEFILE_WRITE_THROUGH` 保证返回时元数据已落盘（断电后不会出现「文件在但内容空」）。
#[cfg(windows)]
fn move_file_replace(temp_path: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 两个指针都指向以 NUL 结尾、在调用期间存活的宽字符串。
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 退路：备份 → 改名 → 删备份。非原子，但任何一步失败都能把旧文件放回去。
#[cfg(windows)]
fn replace_file_with_backup(temp_path: &Path, target: &Path) -> std::io::Result<()> {
    let backup = backup_path(target);
    let had_target = target.exists();
    if had_target {
        // 旧实现 `let _ = fs::remove_file(&backup)` 完全静默：备份删不掉时 rename
        // (target → backup) 会跟着失败，调用方只看到一个含糊的 rename 错误，查不到
        // 根因。兜底路径可以不让整体报错，但不能再无声。NotFound 属正常情况
        // （首次写入时还没有 .bak），不算失败。
        if let Err(err) = fs::remove_file(&backup) {
            if err.kind() != ErrorKind::NotFound {
                eprintln!(
                    "警告: 无法清理旧备份 {}: {err}（后续备份改名可能失败，请手工检查）",
                    backup.display()
                );
            }
        }
        fs::rename(target, &backup)?;
    }
    if let Err(err) = fs::rename(temp_path, target) {
        if had_target {
            // 回滚是把旧文件放回去。这里**必须**上抛：旧实现 `let _ =` 会把回滚失败
            // 彻底吞掉，用户只看到「替换失败」，却不知道旧内容只剩 `.bak` 这一份
            // （而且下次写入可能又把它覆盖掉）。两个路径都要写进错误里。
            if let Err(rollback) = fs::rename(&backup, target) {
                return Err(std::io::Error::new(
                    err.kind(),
                    format!(
                        "替换 {} 失败（{err}）；回滚 rename({} -> {}) 也失败（{rollback}），\
                         旧内容目前只在 {}",
                        target.display(),
                        backup.display(),
                        target.display(),
                        backup.display()
                    ),
                ));
            }
        }
        return Err(err);
    }
    if had_target {
        // 同上：替换已成功，删不掉残留备份不影响结果，但必须留警告让人能发现
        // 同目录下多出来的旧 `.bak`（里面可能是旧 token / 旧配置）。
        if let Err(err) = fs::remove_file(&backup) {
            if err.kind() != ErrorKind::NotFound {
                eprintln!(
                    "警告: 写入成功但无法清理备份 {}: {err}（同目录残留旧内容，请手工删除）",
                    backup.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_port(port: u16) -> Result<()> {
    if port == 0 {
        return Err(anyhow!("extension_port 必须是 1-65535"));
    }
    if port == CLI_API_PORT {
        return Err(anyhow!("18767 是 atb API 端口，插件端口请换一个"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = env::temp_dir().join(format!(
            "abc-config-test-{}-{}-{}",
            tag,
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn net_hooks_store_load_roundtrip() {
        let dir = temp_dir("net-hooks-roundtrip");
        let path = dir.join("net-hooks.json");
        let mut map = HashMap::new();
        map.insert(
            "chrome:profile-1".to_string(),
            serde_json::json!({
                "persist": true,
                "routes": [{ "match": "/api" }],
                "cdpEnabled": true,
                "cdpRules": [],
            }),
        );

        save_net_hooks_to(&path, &map).expect("save net hooks");
        let loaded = load_net_hooks_from(&path).expect("load net hooks");
        assert_eq!(loaded, map);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn net_hooks_reset_clears_persisted_entry() {
        let dir = temp_dir("net-hooks-reset");
        let path = dir.join("net-hooks.json");
        let mut map = HashMap::new();
        map.insert(
            "chrome:profile-1".to_string(),
            serde_json::json!({ "persist": true }),
        );
        map.insert(
            "chrome:profile-2".to_string(),
            serde_json::json!({ "persist": false }),
        );
        save_net_hooks_to(&path, &map).expect("seed");

        // reset 语义：删掉目标 profile 后整体覆盖写回，其它 profile 必须保留。
        map.remove("chrome:profile-1");
        save_net_hooks_to(&path, &map).expect("save after reset");
        let loaded = load_net_hooks_from(&path).expect("load after reset");
        assert_eq!(loaded.len(), 1);
        assert!(!loaded.contains_key("chrome:profile-1"));
        assert!(loaded.contains_key("chrome:profile-2"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn net_hooks_broken_file_does_not_panic() {
        let dir = temp_dir("net-hooks-broken");
        let path = dir.join("net-hooks.json");
        fs::write(&path, "{ not json").expect("seed broken file");

        let err = load_net_hooks_from(&path).expect_err("broken file must be an error");
        assert!(err.to_string().contains("解析"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_file_leaves_new_content_and_no_backup() {
        let dir = temp_dir("replace");
        let target = dir.join("api-token");
        fs::write(&target, "old\n").expect("seed target");
        let temp = dir.join("api-token.tmp");
        fs::write(&temp, "new\n").expect("seed temp");

        replace_file_atomically(&temp, &target).expect("replace");
        assert_eq!(fs::read_to_string(&target).expect("read target"), "new\n");
        assert!(!backup_path(&target).exists());
        assert!(!temp.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_falls_back_to_backup_only_when_target_missing() {
        let dir = temp_dir("backup");
        let target = dir.join("api-token");
        fs::write(backup_path(&target), "old\n").expect("seed backup");

        assert_eq!(read_with_backup(&target).expect("backup read"), "old\n");
        fs::write(&target, "new\n").expect("seed target");
        assert_eq!(read_with_backup(&target).expect("target read"), "new\n");

        fs::remove_file(&target).expect("remove target");
        fs::remove_file(backup_path(&target)).expect("remove backup");
        assert_eq!(
            read_with_backup(&target).expect_err("missing").kind(),
            ErrorKind::NotFound
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_json_falls_back_to_backup() {
        let dir = temp_dir("json-backup");
        let path = dir.join("config.json");
        fs::write(backup_path(&path), "{\"extension_port\": 19999}\n").expect("seed backup");
        // 读到了、但解析失败（写到一半）：旧实现只在 NotFound 时回退。
        fs::write(&path, "{\"extension_port\":").expect("seed truncated target");

        let config = read_json_with_backup::<AppConfig>(&path)
            .expect("read")
            .expect("config");
        assert_eq!(config.extension_port, 19999);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_saves_keep_valid_json() {
        let dir = temp_dir("concurrent-save");
        let path = dir.join("net-hooks.json");
        let mut handles = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut map = HashMap::new();
                map.insert(
                    format!("profile-{index}"),
                    serde_json::json!({ "persist": true }),
                );
                save_net_hooks_to(&path, &map).expect("save");
            }));
        }
        for handle in handles {
            handle.join().expect("join");
        }
        // 不管最后是谁写的，文件必须是完整的 JSON（旧实现会互相截断）。
        let map = load_net_hooks_from(&path).expect("load");
        assert_eq!(map.len(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    /// 环境变量是进程级的，`ATB_HOME` 相关用例必须串行执行。
    fn with_home<T>(home: &Path, action: impl FnOnce() -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let previous = env::var_os("ATB_HOME");
        env::set_var("ATB_HOME", home);
        let result = action();
        match previous {
            Some(value) => env::set_var("ATB_HOME", value),
            None => env::remove_var("ATB_HOME"),
        }
        result
    }

    #[test]
    fn load_is_read_only_and_does_not_create_config() {
        let dir = temp_dir("load-readonly");
        let config = with_home(&dir, || load().expect("load"));
        assert_eq!(config.extension_port, DEFAULT_EXTENSION_PORT);
        // 只读命令不能有写副作用。
        assert!(!dir.join("config.json").exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blank_config_with_valid_backup_keeps_user_settings() {
        // 目标被截断成空文件、但 `.bak` 还完整：必须优先用备份，不能重置成默认值。
        let dir = temp_dir("blank-with-backup");
        let path = dir.join("config.json");
        fs::write(backup_path(&path), "{\"extension_port\": 19001}\n").expect("seed backup");
        fs::write(&path, "").expect("seed blank target");

        let config = with_home(&dir, || load_or_create().expect("load or create"));
        assert_eq!(config.extension_port, 19001);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blank_config_is_quarantined_instead_of_overwritten() {
        let dir = temp_dir("blank-config");
        let path = dir.join("config.json");
        fs::write(&path, "  \n").expect("seed blank config");

        let config = with_home(&dir, || load_or_create().expect("load or create"));
        assert_eq!(config.extension_port, DEFAULT_EXTENSION_PORT);
        assert!(path.exists(), "应当重建一份默认配置");

        // 原文件必须被挪到 .corrupt-*，而不是直接拿默认值覆盖。
        let quarantined = fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(quarantined, "空文件应先备份再重建");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_api_token_never_falls_back_to_backup() {
        // token 轮换时会短暂出现「目标被移走、旧内容只在 .bak」的窗口。读方如果回退
        // 到 .bak 就会拿到旧 token，所以这里必须返回 None。
        let dir = temp_dir("token-no-backup");
        let path = dir.join("api-token");
        fs::write(backup_path(&path), "stale-token\n").expect("seed backup");
        assert!(load_api_token_with(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_api_token_prefers_target_over_backup() {
        let dir = temp_dir("token-target-wins");
        let path = dir.join("api-token");
        fs::write(&path, "fresh-token\n").expect("seed target");
        fs::write(backup_path(&path), "stale-token\n").expect("seed backup");
        assert_eq!(load_api_token_with(&dir).as_deref(), Some("fresh-token"));
        fs::remove_dir_all(&dir).ok();
    }

    fn load_api_token_with(home: &Path) -> Option<String> {
        with_home(home, || load_api_token().expect("load token"))
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("log-private");
        let path = with_home(&dir, || ensure_log_file().expect("ensure log"));
        let file_mode = fs::metadata(&path).expect("stat log").permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "daemon.log 必须是 0600");
        let dir_mode = fs::metadata(dir.join("logs"))
            .expect("stat log dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "日志目录必须是 0700");
        fs::remove_dir_all(&dir).ok();
    }
}
