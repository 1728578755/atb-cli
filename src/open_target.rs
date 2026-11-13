use path_clean::PathClean;
use serde_json::{json, Value};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use url::{Host, Url};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Web,
    LocalResource,
}

#[derive(Debug, Clone)]
pub struct ResolvedOpenTarget {
    pub navigation_url: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone)]
pub struct OpenTargetError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl OpenTargetError {
    fn new(code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }
}

impl fmt::Display for OpenTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenTargetError {}

/// `open` 解析器接受的显式 scheme。
///
/// `chrome-extension` 是新增的：本工具/浏览器自己的扩展页面可以由
/// `chrome.tabs.create` 打开；其它扩展的页面由 Chrome 拒绝，daemon 原样回包
/// 并附 hint。`chrome://` / `edge://` / `about:` / `devtools://` 仍不在列表里。
pub const SUPPORTED_OPEN_SCHEMES: &[&str] = &["http", "https", "file", "chrome-extension"];

/// 浏览器保留页面：扩展无权创建，必须用户手动打开后再用 `atb tabs` 选目标。
pub const BROWSER_RESERVED_SCHEMES: &[&str] = &["chrome", "edge", "about", "devtools"];

/// 命中浏览器保留 scheme 时统一附加的 hint（error.details.hint 与 CLI 共用）。
pub const BROWSER_RESERVED_SCHEME_HINT: &str =
    "chrome:// / edge:// / about: / devtools:// 页面不能由扩展创建；请手动打开后用 `atb tabs` 选择目标；chrome-extension:// 仅支持本工具自己的页面";

// 该输入是否属于「特殊页面」scheme。
///
/// 只做前缀判断、不做 URL 归一化：`chrome://` 一旦被改写成 http 就再也打不开。
pub fn special_page_scheme(input: &str) -> Option<&'static str> {
    let lower = input.trim_start().to_ascii_lowercase();
    if lower.starts_with("chrome-extension://") {
        return Some("chrome-extension");
    }
    if lower.starts_with("chrome://") {
        return Some("chrome");
    }
    if lower.starts_with("edge://") {
        return Some("edge");
    }
    if lower.starts_with("devtools://") {
        return Some("devtools");
    }
    if lower.starts_with("about:") {
        return Some("about");
    }
    None
}

/// `open` 的目标是否落在特殊页面上（用于自动跳过 wait / 3s 等待预算）。
pub fn is_special_page(input: &str) -> bool {
    special_page_scheme(input).is_some()
}

pub fn resolve_open_target(
    input: &str,
    cwd: Option<&Path>,
) -> Result<ResolvedOpenTarget, OpenTargetError> {
    if input.is_empty() || input.trim().is_empty() {
        return Err(OpenTargetError::new(
            "invalid_open_target",
            "Open Target must not be empty",
            json!({ "open_target": input }),
        ));
    }
    if let Some(cwd) = cwd {
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(OpenTargetError::new(
                "invalid_cwd",
                "cwd must be an absolute existing directory",
                json!({ "open_target": input, "cwd": cwd }),
            ));
        }
    }

    if let Some(platform) = foreign_path_platform(input) {
        return Err(OpenTargetError::new(
            "foreign_platform_path",
            format!(
                "The Open Target looks like a {platform} path, but the CLI is running on {}",
                std::env::consts::OS
            ),
            json!({
                "open_target": input,
                "host_platform": std::env::consts::OS,
                "detected_platform": platform,
            }),
        ));
    }

    if cfg!(windows) && is_explicit_path(input) {
        return resolve_path_input(input, cwd);
    }

    // `C:report.html` 是 Windows 的「驱动器相对路径」，不是 scheme `c`。
    // 旧逻辑把它交给 `explicit_scheme`，于是报 unsupported_scheme。
    #[cfg(windows)]
    if is_windows_drive_relative(input) {
        return resolve_path_input(input, cwd);
    }

    if let Some(scheme) = explicit_scheme(input) {
        // `scheme:数字` 有两种可能：真的 scheme（`tel:110`）或省略了 `//` 的
        // `host:port`（`localhost:3000` / `example.com:8080`）。旧实现把两者都
        // 塞进 `looks_like_host_port`，导致 `tel:123` 被改写成 `https://tel:123`，
        // 而 `example.com:99999` 报 unsupported_scheme。
        if let Some((_, rest)) = input.split_once(':') {
            if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
                if rest.parse::<u16>().is_err() {
                    return Err(OpenTargetError::new(
                        "invalid_port",
                        format!("Port out of range: {rest}"),
                        json!({ "open_target": input, "port": rest }),
                    ));
                }
                // 端口合法，但前缀必须真的像主机（localhost / IP / 含点 / 带括号），
                // 否则 `tel:123` 这类就是 scheme:数字，不能静默改写成 HTTP URL。
                if !looks_like_host_name(&scheme) {
                    return Err(OpenTargetError::new(
                        "unsupported_scheme",
                        format!("Unsupported Open Target scheme: {scheme}"),
                        json!({ "scheme": scheme, "supported_schemes": SUPPORTED_OPEN_SCHEMES }),
                    ));
                }
                return resolve_implicit_web(input);
            }
        }
        return match scheme.as_str() {
            "http" | "https" => resolve_explicit_web(input, &scheme),
            "file" => resolve_file_url(input),
            "chrome-extension" => resolve_extension_url(input),
            // 浏览器保留页面：明确说明「扩展不能创建」，并给出可执行的替代路径。
            _ if BROWSER_RESERVED_SCHEMES.contains(&scheme.as_str()) => Err(OpenTargetError::new(
                "unsupported_scheme",
                format!("Unsupported Open Target scheme: {scheme}（浏览器保留页面不能由扩展创建）"),
                json!({
                    "scheme": scheme,
                    "supported_schemes": SUPPORTED_OPEN_SCHEMES,
                    "hint": BROWSER_RESERVED_SCHEME_HINT,
                }),
            )),
            _ if looks_like_host_port(input) => resolve_implicit_web(input),
            _ => Err(OpenTargetError::new(
                "unsupported_scheme",
                format!("Unsupported Open Target scheme: {scheme}"),
                json!({ "scheme": scheme, "supported_schemes": SUPPORTED_OPEN_SCHEMES }),
            )),
        };
    }

    if is_explicit_path(input) {
        return resolve_path_input(input, cwd);
    }

    if let Some(cwd) = cwd {
        if cwd.join(input).exists() {
            return resolve_path_input(input, Some(cwd));
        }
    }

    // 裸文件名不存在时不能静默改写成 `https://demo.html`：那条 URL 必然打不开，
    // 用户只会在浏览器里看到一个与输入毫不相关的错误页。只有明确像域名时
    // 才回退到 https。
    if looks_like_web_target(input) {
        return resolve_implicit_web(input);
    }

    Err(OpenTargetError::new(
        "file_not_found",
        format!("Open Target looks like a local file but does not exist: {input}"),
        json!({
            "open_target": input,
            "cwd": cwd.map(|path| path.display().to_string()),
            "did_you_mean": suggest_local_files(input, cwd),
        }),
    ))
}

/// Windows 驱动器相对路径（`C:report.html`）：单字母 + `:`，且后面不是 `//`、`/`、`\`。
///
/// 它相对的是「该驱动器的当前目录」，既不是进程 cwd，也不是 scheme。
#[cfg(windows)]
fn is_windows_drive_relative(input: &str) -> bool {
    let bytes = input.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    !matches!(bytes.get(2), Some(b'/') | Some(b'\\'))
}

/// 常见文件扩展名：用来区分「`demo.html` 这种文件名」和「`example.com` 这种域名」。
///
/// 没有这张表就只能靠「有没有点号」判断，`demo.html` 会被误当成域名。
const FILE_EXTENSIONS: &[&str] = &[
    "html", "htm", "xhtml", "txt", "md", "markdown", "pdf", "png", "jpg", "jpeg", "gif", "webp",
    "svg", "css", "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "jsonl", "xml", "csv", "tsv",
    "yaml", "yml", "toml", "ini", "log", "zip", "gz", "tar", "7z", "rar", "mp3", "mp4", "webm",
    "mov", "wav", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "exe", "msi", "dmg", "sh", "bat",
    "ps1", "py", "rb", "go", "rs", "java", "c", "h", "cpp", "hpp",
];

/// 这个输入看起来是「网页」而不是「本地文件名」吗？
///
/// 判定偏保守：有显式端口、是 localhost、或点号后不是常见扩展名时才算网页。
fn looks_like_web_target(input: &str) -> bool {
    if explicit_port(input).is_some() {
        return true;
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or(input);
    if authority.starts_with('[') {
        // IPv6 字面量（`[::1]` / `[::1]:8080`）。
        return true;
    }
    if authority.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let mut labels = authority.rsplit('.');
    let Some(tld) = labels.next() else {
        return false;
    };
    // 没有点号 = 本地文件名（`demo`），不当作单标签主机名。
    if labels.next().is_none() {
        return false;
    }
    let tld = tld.to_ascii_lowercase();
    tld.len() >= 2
        && tld.chars().all(|ch| ch.is_ascii_alphabetic())
        && !FILE_EXTENSIONS.contains(&tld.as_str())
}

/// 给 `file_not_found` 附带同目录下的相近候选，避免用户/智能体只能靠猜。
fn suggest_local_files(input: &str, cwd: Option<&Path>) -> Vec<String> {
    let Some(cwd) = cwd else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(cwd) else {
        return Vec::new();
    };
    let (stem, extension) = match input.rsplit_once('.') {
        Some((stem, extension)) => (
            stem.to_ascii_lowercase(),
            Some(extension.to_ascii_lowercase()),
        ),
        None => (input.to_ascii_lowercase(), None),
    };
    let mut candidates: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            let same_stem = !stem.is_empty() && lower.contains(&stem);
            let same_extension = extension
                .as_deref()
                .is_some_and(|extension| lower.ends_with(&format!(".{extension}")));
            same_stem || same_extension
        })
        .collect();
    candidates.sort();
    candidates.truncate(3);
    candidates
}

fn resolve_explicit_web(
    input: &str,
    expected_scheme: &str,
) -> Result<ResolvedOpenTarget, OpenTargetError> {
    let parsed = Url::parse(input).map_err(|err| {
        OpenTargetError::new(
            "invalid_http_url",
            format!("Invalid {} URL: {err}", expected_scheme.to_uppercase()),
            json!({ "open_target": input, "scheme": expected_scheme }),
        )
    })?;
    if parsed.host().is_none() {
        return Err(OpenTargetError::new(
            "invalid_http_url",
            format!(
                "Invalid {} URL: host is missing",
                expected_scheme.to_uppercase()
            ),
            json!({ "open_target": input, "scheme": expected_scheme }),
        ));
    }
    Ok(ResolvedOpenTarget {
        navigation_url: input.to_string(),
        kind: TargetKind::Web,
    })
}

/// `chrome-extension://<扩展ID>/<路径>`：只做形状校验，原样交给扩展。
///
/// 是否「本工具自己的页面」由 Chrome 判定：外来扩展页面会被 `chrome.tabs.create`
// 拒绝，daemon 把扩展的错误原样回包并附 hint。这里不猜扩展 ID，
/// 也不做 URL 归一化（扩展页面的路径/查询串必须逐字保留）。
fn resolve_extension_url(input: &str) -> Result<ResolvedOpenTarget, OpenTargetError> {
    let parsed = Url::parse(input).map_err(|err| {
        OpenTargetError::new(
            "invalid_extension_url",
            format!("Invalid chrome-extension URL: {err}"),
            json!({ "open_target": input }),
        )
    })?;
    if !parsed.scheme().eq_ignore_ascii_case("chrome-extension") {
        return Err(OpenTargetError::new(
            "invalid_extension_url",
            format!("Not a chrome-extension URL: {}", parsed.scheme()),
            json!({ "open_target": input, "scheme": parsed.scheme() }),
        ));
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return Err(OpenTargetError::new(
            "invalid_extension_url",
            "chrome-extension URL must include an extension id (host)",
            json!({ "open_target": input }),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        return Err(OpenTargetError::new(
            "invalid_extension_url",
            "chrome-extension URL must not contain credentials or a port",
            json!({ "open_target": input }),
        ));
    }
    Ok(ResolvedOpenTarget {
        navigation_url: input.to_string(),
        kind: TargetKind::Web,
    })
}

fn resolve_implicit_web(input: &str) -> Result<ResolvedOpenTarget, OpenTargetError> {
    let probe = Url::parse(&format!("https://{input}")).map_err(|err| {
        OpenTargetError::new(
            "invalid_open_target",
            format!("Open Target is neither a valid local path nor a valid web address: {err}"),
            json!({ "open_target": input }),
        )
    })?;
    let host = probe.host().ok_or_else(|| {
        OpenTargetError::new(
            "invalid_open_target",
            "Open Target is missing a web host",
            json!({ "open_target": input }),
        )
    })?;
    let scheme = match explicit_port(input) {
        Some(80) => "http",
        Some(443) => "https",
        _ if host_defaults_to_http(host) => "http",
        _ => "https",
    };
    Ok(ResolvedOpenTarget {
        navigation_url: format!("{scheme}://{input}"),
        kind: TargetKind::Web,
    })
}

fn host_defaults_to_http(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let lower = domain.to_ascii_lowercase();
            lower == "localhost"
                || lower.ends_with(".localhost")
                || lower.ends_with(".local")
                || !lower.contains('.')
        }
        Host::Ipv4(ip) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Host::Ipv6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
        }
    }
}

/// `file:` 后面是否是一段绝对路径（而不是 `file:demo.html` 这种相对写法）。
///
/// 必须在 `Url::parse` **之前**判断：URL 解析器会把 `file:demo.html` 规范化成
/// `file:///demo.html`（POSIX）或带 host 的形式（Windows），等到解析后再看
/// `path()` 已经看不出「用户写的是相对路径」。旧实现只在 Unix 上补了这条判断，
/// 于是同一个输入在 Linux 报 `invalid_file_url`、在 Windows 走成文件不存在，
/// 两端错误码不一致。
fn file_url_is_absolute(input: &str) -> bool {
    let rest = input
        .get("file:".len()..)
        .unwrap_or_default()
        .trim_start_matches([' ', '\t']);
    if rest.starts_with('/') || rest.starts_with('\\') {
        return true;
    }
    // Windows 盘符绝对路径：`file:C:/dir/a.html` / `file:C://dir//a.html`。
    let bytes = rest.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn resolve_file_url(input: &str) -> Result<ResolvedOpenTarget, OpenTargetError> {
    if !input
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
        || !file_url_is_absolute(input)
    {
        return Err(OpenTargetError::new(
            "invalid_file_url",
            "Relative file URLs are not supported; pass a relative filesystem path instead",
            json!({ "open_target": input }),
        ));
    }
    // 先解析再判绝对性：`file:/path` 在浏览器里等价于
    // `file:///path`，旧实现只认 `file://` 前缀，把绝对路径误报成「Relative file URLs」。
    let mut parsed = Url::parse(input).map_err(|err| {
        OpenTargetError::new(
            "invalid_file_url",
            format!("Invalid file URL: {err}"),
            json!({ "open_target": input }),
        )
    })?;
    if !parsed.scheme().eq_ignore_ascii_case("file") {
        return Err(OpenTargetError::new(
            "invalid_file_url",
            format!("Not a file URL: {}", parsed.scheme()),
            json!({ "open_target": input, "scheme": parsed.scheme() }),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        return Err(OpenTargetError::new(
            "invalid_file_url",
            "File URL must not contain credentials or a port",
            json!({ "open_target": input }),
        ));
    }

    let host = parsed.host_str().unwrap_or_default().to_string();
    // file://host/... 在 Unix 上映射到 `//host/...`，在 Windows 上映射到 UNC 路径
    // `\\host\share\...`，访问即触发 SMB 外连。除空 host / localhost 外
    // 一律拒绝，除非用户用 `ATB_FILE_HOSTS=host1,host2` 显式信任。
    if !file_host_allowed(&host) {
        return Err(OpenTargetError::new(
            "unsupported_file_host",
            format!(
                "File URL host is not in the allowlist: {host}；如确需访问，设置 ATB_FILE_HOSTS={host}"
            ),
            json!({
                "open_target": input,
                "host": host,
                "host_platform": std::env::consts::OS,
                "file_hosts_env": "ATB_FILE_HOSTS",
            }),
        ));
    }
    if host.eq_ignore_ascii_case("localhost") {
        parsed.set_host(None).map_err(|_| {
            OpenTargetError::new(
                "invalid_file_url",
                "Could not normalize localhost file URL",
                json!({ "open_target": input }),
            )
        })?;
    }

    let query = parsed.query().map(str::to_string);
    let fragment = parsed.fragment().map(str::to_string);
    parsed.set_query(None);
    parsed.set_fragment(None);
    let path = parsed.to_file_path().map_err(|_| {
        OpenTargetError::new(
            "invalid_file_url",
            "File URL must be absolute and map to a path on this platform",
            json!({ "open_target": input }),
        )
    })?;
    if !path.is_absolute() {
        return Err(OpenTargetError::new(
            "invalid_file_url",
            "Relative file URLs are not supported; pass a relative filesystem path instead",
            json!({ "open_target": input }),
        ));
    }
    resolve_local_path(path, input, query.as_deref(), fragment.as_deref())
}

/// file URL 的 host 是否允许（空 / localhost / `ATB_FILE_HOSTS` 逗号白名单）。
fn file_host_allowed(host: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    crate::settings::file_hosts()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry.eq_ignore_ascii_case(host))
}

fn resolve_path_input(
    input: &str,
    cwd: Option<&Path>,
) -> Result<ResolvedOpenTarget, OpenTargetError> {
    let path = expand_current_user_home(input)?;
    #[cfg(windows)]
    let path = if is_windows_drive_relative(input) {
        // `C:report.html` 相对「C: 盘的当前目录」，不能按进程 cwd 拼接（会得到
        // `cwd\C:report.html`）。`std::path::absolute` 走 GetFullPathNameW，能按
        // Windows 自己的语义展开。
        std::path::absolute(&path).map_err(|err| {
            OpenTargetError::new(
                "invalid_open_target",
                format!(
                    "Could not resolve drive-relative path {}: {err}",
                    path.display()
                ),
                json!({ "open_target": input }),
            )
        })?
    } else {
        path
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        let cwd = validate_cwd(cwd, input)?;
        cwd.join(path)
    }
    .clean();
    resolve_local_path(absolute, input, None, None)
}

fn validate_cwd<'a>(cwd: Option<&'a Path>, input: &str) -> Result<&'a Path, OpenTargetError> {
    let cwd = cwd.ok_or_else(|| {
        OpenTargetError::new(
            "missing_cwd",
            "A working directory is required for a relative Open Target",
            json!({ "open_target": input }),
        )
    })?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(OpenTargetError::new(
            "invalid_cwd",
            "cwd must be an absolute existing directory",
            json!({ "open_target": input, "cwd": cwd }),
        ));
    }
    Ok(cwd)
}

fn resolve_local_path(
    path: PathBuf,
    input: &str,
    query: Option<&str>,
    fragment: Option<&str>,
) -> Result<ResolvedOpenTarget, OpenTargetError> {
    if !path.exists() {
        return Err(OpenTargetError::new(
            "local_resource_not_found",
            format!("Local Resource does not exist: {}", path.display()),
            json!({ "open_target": input, "resolved_path": path }),
        ));
    }
    let canonical = dunce::canonicalize(&path).map_err(|err| {
        OpenTargetError::new(
            "local_resource_not_found",
            format!("Could not resolve Local Resource {}: {err}", path.display()),
            json!({ "open_target": input, "resolved_path": path }),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|err| {
        OpenTargetError::new(
            "local_resource_not_found",
            format!(
                "Could not inspect Local Resource {}: {err}",
                canonical.display()
            ),
            json!({ "open_target": input, "resolved_path": canonical }),
        )
    })?;
    if !metadata.is_file() {
        return Err(OpenTargetError::new(
            "local_resource_not_file",
            format!(
                "Local Resource must be a regular file: {}",
                canonical.display()
            ),
            json!({
                "open_target": input,
                "resolved_path": canonical,
                "actual_type": if metadata.is_dir() { "directory" } else { "non_regular" },
            }),
        ));
    }
    let mut url = Url::from_file_path(&canonical).map_err(|_| {
        OpenTargetError::new(
            "invalid_file_url",
            "Could not convert Local Resource to a file URL",
            json!({ "open_target": input, "resolved_path": canonical }),
        )
    })?;
    url.set_query(query);
    url.set_fragment(fragment);
    Ok(ResolvedOpenTarget {
        navigation_url: url.to_string(),
        kind: TargetKind::LocalResource,
    })
}

fn expand_current_user_home(input: &str) -> Result<PathBuf, OpenTargetError> {
    if input == "~" || input.starts_with("~/") || (cfg!(windows) && input.starts_with("~\\")) {
        // 统一走 config::home_dir()：配置目录、`~` 展开、daemon cwd
        // 必须用同一个 `dirs::home_dir()`，否则 Windows Git-Bash / WSL 下会分叉。
        let home = crate::config::home_dir().ok_or_else(|| {
            OpenTargetError::new(
                "home_directory_unavailable",
                "Could not determine the current user's home directory",
                json!({ "open_target": input }),
            )
        })?;
        if input == "~" {
            return Ok(home);
        }
        return Ok(home.join(&input[2..]));
    }
    Ok(PathBuf::from(input))
}

fn explicit_scheme(input: &str) -> Option<String> {
    let (scheme, _) = input.split_once(':')?;
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

fn looks_like_host_port(input: &str) -> bool {
    explicit_port(input).is_some()
}

/// `scheme:数字` 里的前缀是否像主机名（决定它能不能被当作 `host:port`）。
///
/// 只认 localhost、IP 字面量、带点号的域名/主机名。单标签主机（`printer:631`）
/// 与真正的 scheme（`tel:123`）无法区分，按报告要求保守处理：不自动改写，用户可写
/// `http://printer:631`。
fn looks_like_host_name(prefix: &str) -> bool {
    prefix.eq_ignore_ascii_case("localhost")
        || prefix.parse::<std::net::IpAddr>().is_ok()
        || (prefix.starts_with('[') && prefix.ends_with(']'))
        || prefix.contains('.')
}

fn explicit_port(input: &str) -> Option<u16> {
    let authority = input.split(['/', '?', '#']).next()?;
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        return authority.get(end + 1..)?.strip_prefix(':')?.parse().ok();
    }
    let (host, port) = authority.rsplit_once(':')?;
    (!host.is_empty()).then(|| port.parse().ok()).flatten()
}

fn is_explicit_path(input: &str) -> bool {
    input == "~"
        || input.starts_with("~/")
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with(".\\")
        || input.starts_with("..\\")
        || Path::new(input).is_absolute()
}

#[cfg(not(windows))]
fn foreign_path_platform(input: &str) -> Option<&'static str> {
    let bytes = input.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    // `C:report.html`（驱动器相对路径）同样是 Windows 语法，不是 scheme `c`。
    let drive_relative = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes.get(2), Some(b'/') | Some(b'\\'));
    let unc = input.starts_with("\\\\");
    let relative = input.starts_with(".\\") || input.starts_with("..\\");
    let home = input.starts_with("~\\");
    (drive || drive_relative || unc || relative || home).then_some("windows")
}

#[cfg(windows)]
fn foreign_path_platform(input: &str) -> Option<&'static str> {
    (input.starts_with("/Users/") || input.starts_with("/home/") || input.starts_with("/tmp/"))
        .then_some("unix")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-browser-open-target-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn existing_bare_path_wins_over_web() {
        let dir = temp_dir();
        fs::write(dir.join("example.com"), "ok").unwrap();
        let resolved = resolve_open_target("example.com", Some(&dir)).unwrap();
        assert_eq!(resolved.kind, TargetKind::LocalResource);
        assert!(resolved.navigation_url.starts_with("file://"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_bare_file_is_reported_as_file_not_found() {
        // 旧行为是静默改写成 `https://demo.html`，浏览器里必然打不开。
        let dir = temp_dir();
        fs::write(dir.join("demo-old.html"), "ok").unwrap();
        let err = resolve_open_target("demo.html", Some(&dir)).unwrap_err();
        assert_eq!(err.code, "file_not_found");
        let suggestions = err
            .details
            .get("did_you_mean")
            .and_then(Value::as_array)
            .expect("did_you_mean");
        assert!(
            suggestions
                .iter()
                .any(|item| item.as_str() == Some("demo-old.html")),
            "suggestions: {suggestions:?}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_dotted_name_that_looks_like_a_domain_still_falls_back_to_web() {
        let dir = temp_dir();
        assert_eq!(
            resolve_open_target("example.com", Some(&dir))
                .unwrap()
                .navigation_url,
            "https://example.com"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_relative_path_is_a_path_not_a_scheme() {
        assert!(is_windows_drive_relative("C:report.html"));
        assert!(is_windows_drive_relative(r"C:reports\a.html"));
        assert!(!is_windows_drive_relative("C:/report.html"));
        assert!(!is_windows_drive_relative("c://example.com"));
        assert!(!is_windows_drive_relative("https://example.com"));

        // 关键是不能报 unsupported_scheme：它必须走本地路径分支。
        let err = resolve_open_target("C:definitely-missing-report.html", None).unwrap_err();
        assert_ne!(err.code, "unsupported_scheme");
        assert_eq!(err.code, "local_resource_not_found");
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        let dir = temp_dir();
        let err = resolve_open_target("./missing.html", Some(&dir)).unwrap_err();
        assert_eq!(err.code, "local_resource_not_found");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directories_are_rejected() {
        let dir = temp_dir();
        let err = resolve_open_target(dir.to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.code, "local_resource_not_file");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_web_url_is_preserved() {
        let input = "HTTPS://EXAMPLE.com:443/a/../b?q=a%2Fb";
        let resolved = resolve_open_target(input, None).unwrap();
        assert_eq!(resolved.navigation_url, input);
    }

    #[test]
    fn local_hosts_default_to_http_and_public_hosts_to_https() {
        assert_eq!(
            resolve_open_target("localhost:3000", None)
                .unwrap()
                .navigation_url,
            "http://localhost:3000"
        );
        assert_eq!(
            resolve_open_target("printer.local", None)
                .unwrap()
                .navigation_url,
            "http://printer.local"
        );
        assert_eq!(
            resolve_open_target("example.com", None)
                .unwrap()
                .navigation_url,
            "https://example.com"
        );
    }

    #[test]
    fn standard_ports_override_host_defaults() {
        assert_eq!(
            resolve_open_target("example.com:80", None)
                .unwrap()
                .navigation_url,
            "http://example.com:80"
        );
        assert_eq!(
            resolve_open_target("localhost:443", None)
                .unwrap()
                .navigation_url,
            "https://localhost:443"
        );
    }

    #[test]
    fn unsupported_schemes_are_rejected() {
        let err = resolve_open_target("mailto:user@example.com", None).unwrap_err();
        assert_eq!(err.code, "unsupported_scheme");
        // 错误里必须告诉调用方 chrome-extension 是可用的 scheme。
        let supported = err
            .details
            .get("supported_schemes")
            .and_then(Value::as_array)
            .expect("supported_schemes");
        assert!(supported.iter().any(|item| item == "chrome-extension"));
    }

    // chrome-extension:// 允许进入 open 流程；外来扩展页面由 Chrome 拒绝。
    #[test]
    fn chrome_extension_urls_are_accepted_verbatim() {
        for raw in [
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop/options.html",
            "chrome-extension://abc/popup.html?x=1#y",
        ] {
            let resolved = resolve_open_target(raw, None).expect(raw);
            assert_eq!(resolved.navigation_url, raw);
            assert!(matches!(resolved.kind, TargetKind::Web));
        }
        // 缺扩展 ID（host）与带端口都不接受。
        for raw in [
            "chrome-extension:///options.html",
            "chrome-extension://abc:99/x",
        ] {
            let err = resolve_open_target(raw, None).unwrap_err();
            assert_eq!(err.code, "invalid_extension_url", "input: {raw}");
        }
    }

    // 浏览器保留页面仍然拒绝，并给出「手动打开 + atb tabs 选择」的 hint。
    #[test]
    fn browser_reserved_schemes_are_rejected_with_hint() {
        for raw in [
            "chrome://settings",
            "edge://settings",
            "about:blank",
            "devtools://devtools/bundled/inspector.html",
        ] {
            let err = resolve_open_target(raw, None).unwrap_err();
            assert_eq!(err.code, "unsupported_scheme", "input: {raw}");
            let hint = err
                .details
                .get("hint")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(hint.contains("atb tabs"), "hint: {hint}");
            assert!(hint.contains("chrome-extension://"), "hint: {hint}");
        }
        // special_page_scheme 覆盖 5 类特殊页面（含 chrome-extension）。
        assert_eq!(special_page_scheme("chrome://settings"), Some("chrome"));
        assert_eq!(special_page_scheme("edge://settings"), Some("edge"));
        assert_eq!(special_page_scheme("about:blank"), Some("about"));
        assert_eq!(
            special_page_scheme("devtools://devtools/bundled/inspector.html"),
            Some("devtools")
        );
        assert_eq!(
            special_page_scheme("chrome-extension://abc/x.html"),
            Some("chrome-extension")
        );
        assert!(!is_special_page("https://example.com"));
    }

    #[cfg(unix)]
    #[test]
    fn path_hash_and_question_mark_are_filename_characters() {
        let dir = temp_dir();
        let filename = "report#1?draft.html";
        fs::write(dir.join(filename), "ok").unwrap();
        let resolved = resolve_open_target(&format!("./{filename}"), Some(&dir)).unwrap();
        assert!(resolved.navigation_url.contains("report%231%3Fdraft.html"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relative_file_urls_are_rejected() {
        let err = resolve_open_target("file:demo.html", None).unwrap_err();
        assert_eq!(err.code, "invalid_file_url");
    }

    #[cfg(unix)]
    #[test]
    fn absolute_file_url_without_double_slash_is_accepted() {
        // `file:/path`（单斜杠）是合法的绝对 file URL，旧实现误报成相对 URL。
        let dir = temp_dir();
        let file = dir.join("demo.html");
        fs::write(&file, "ok").unwrap();
        let input = format!("file:{}", file.display());
        let resolved = resolve_open_target(&input, None).unwrap();
        assert!(resolved.navigation_url.starts_with("file://"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_url_with_remote_host_is_rejected_by_default() {
        // 非空 host 会触发 UNC/SMB 外连，默认必须拒绝。
        let err = resolve_open_target("file://server/share/demo.html", None).unwrap_err();
        assert_eq!(err.code, "unsupported_file_host");
        assert_eq!(
            err.details.get("file_hosts_env").and_then(Value::as_str),
            Some("ATB_FILE_HOSTS")
        );
    }

    #[test]
    fn numeric_scheme_is_not_rewritten_to_http() {
        // `tel:123` 是 scheme:数字，不是 host:port；旧实现会改写成 https://tel:123。
        let err = resolve_open_target("tel:123", None).unwrap_err();
        assert_eq!(err.code, "unsupported_scheme");
        // 端口超出 u16 时报 invalid_port，并带上原始 port 值。
        let err = resolve_open_target("example.com:99999", None).unwrap_err();
        assert_eq!(err.code, "invalid_port");
        assert_eq!(
            err.details.get("port").and_then(Value::as_str),
            Some("99999")
        );
    }

    #[test]
    fn relative_paths_require_a_valid_absolute_cwd() {
        let err = resolve_open_target("./demo.html", None).unwrap_err();
        assert_eq!(err.code, "missing_cwd");
        let err = resolve_open_target("./demo.html", Some(Path::new("relative"))).unwrap_err();
        assert_eq!(err.code, "invalid_cwd");
        let err =
            resolve_open_target("https://example.com", Some(Path::new("relative"))).unwrap_err();
        assert_eq!(err.code, "invalid_cwd");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_resolve_to_the_real_file() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let real = dir.join("real.html");
        let link = dir.join("current.html");
        fs::write(&real, "ok").unwrap();
        symlink(&real, &link).unwrap();
        let resolved = resolve_open_target(link.to_str().unwrap(), None).unwrap();
        assert_eq!(
            Url::parse(&resolved.navigation_url)
                .unwrap()
                .to_file_path()
                .unwrap(),
            dunce::canonicalize(real).unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_url_preserves_query_and_fragment() {
        let dir = temp_dir();
        let file = dir.join("demo.html");
        fs::write(&file, "ok").unwrap();
        let mut url = Url::from_file_path(&file).unwrap();
        url.set_query(Some("theme=dark"));
        url.set_fragment(Some("intro"));
        let resolved = resolve_open_target(url.as_str(), None).unwrap();
        assert!(resolved.navigation_url.ends_with("?theme=dark#intro"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_paths_are_rejected_on_unix() {
        for input in [
            r"C:\Users\Alice\demo.html",
            r"\\server\share\demo.html",
            r".\demo.html",
            r"~\demo.html",
        ] {
            let err = resolve_open_target(input, None).unwrap_err();
            assert_eq!(err.code, "foreign_platform_path", "input: {input}");
        }
    }
}
