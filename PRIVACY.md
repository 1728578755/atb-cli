# 隐私说明

适用于 `atb` 命令行工具（npm 包 `@g1728578755/atb`）与配套 Chrome 扩展 **ATB**（扩展文件版本 5.5.0）。

安全边界与威胁模型见 [SECURITY.md](./SECURITY.md)；外部来源与依赖许可见 [SOURCES.md](./SOURCES.md)。

## 一句话结论

CLI、daemon、扩展之间的通信都在本机 `127.0.0.1`。工具不含遥测、不上传数据、不加载远程代码，也不需要账号。

## 数据流

```text
Chrome 页面
   ↑↓  内容脚本 / chrome.* API
Chrome 扩展 ATB
   ↑↓  WebSocket  ws://127.0.0.1:18765
本机 daemon（Rust 进程）
   ↑↓  HTTP  http://127.0.0.1:18767（带 api-token）
atb CLI（每次命令一个进程）
```

- daemon 监听 `18765`（扩展）与 `18767`（CLI），两者都只绑回环地址。
- daemon 由 CLI 按需拉起；默认空闲 300 秒且没有扩展连接时退出（`ATB_IDLE_TTL` 可调），扩展连着时不退。
- 扩展是 WebSocket 客户端，只连本机；连不上时不做任何重试以外的动作。
- 配置目录默认 `~/.atb`，可用 `ATB_HOME` 改到别处。

## 本地数据清单

| 路径 | 内容 | 用途 | 删除 |
| --- | --- | --- | --- |
| `~/.atb/config.json` | `{"extension_port":18765}` | 扩展连接端口 | 删除后回默认 18765 |
| `~/.atb/api-token` | 随机 token（Unix 权限 0600；Windows 上由 ACL 收紧，效果等同 Unix 0600） | CLI 调 daemon 的鉴权 | 删除后新 daemon 重新生成 |
| `~/.atb/ws-trust.json` | 扩展白名单与握手 token（权限 0600 / Windows ACL，与 `api-token` 同等） | `trust-extension` 的可选加固 | 删除后回到兼容模式 |
| `~/.atb/idempotency.jsonl` | 幂等记录：默认只存 `key` / `scope` / 时间 / HTTP 状态，**不存响应体** | 60 秒内重发同一 `--idempotency-key` 时回放首次结果 | 删除即清空；不影响后续命令 |
| `~/.atb/net-hooks.json` | 按 `browser:profile` 存的网络规则 | daemon 重启后恢复 `net` 规则 | 删除即清空规则 |
| `~/.atb/agent-tabs.json` | 智能体名到标签页的映射 | 多智能体互不串台 | 删除即清空分配记录 |
| `~/.atb/logs/daemon.log` | daemon 运行日志（文件 0600、目录 0700） | 排查连接与命令失败 | 删除后下次运行重建 |
| `~/.atb/banner.json` | 只存 `{agent, browser, at}`（当前 agent、可读浏览器名、上次提示时间） | `ATB_BROWSER_BANNER=once` 时避免重复提示「正在操作哪个浏览器」 | 删除后下次命令重新提示一次 |
| 浏览器扩展存储 | 扩展与 profile 标识、扩展端口、握手 token、角标开关、`persistTtlSec`（持久注入默认寿命）、`persistInjections`（脚本、`matches`、`createdAt`、`expiresAt`）、最后连接错误、自检状态；`storage.session` 放请求去重（60 秒终态账本）与调试会话恢复等短期状态 | 多浏览器识别、弹窗诊断、刷新后重放注入 | 卸载扩展即删除 |

扩展存储在 Chrome 的扩展数据目录内，不在 `~/.atb`。`storage.session` 的数据随浏览器退出清空。

`idempotency.jsonl` 的落盘档位由 `ATB_IDEMPOTENCY_PERSIST` 控制：

| 档位 | 落盘内容 | 说明 |
| --- | --- | --- |
| `redacted`（默认） | `key`、`scope`、时间、状态 | 只用于审计与去重，不参与跨进程回放 |
| `full` | 上面的元数据 + 响应体（单条 256KB 上限，60 秒 TTL） | 需显式开启；响应体可能含页面正文 / Cookie / 网络响应，请按敏感数据处理 |
| `off` | 不落盘 | 纯内存缓存 |

默认 `ATB_IDEMPOTENCY_PERSIST=redacted` 只存 `{key, scope, at, status}`，不存响应体，因此 daemon 重启后不可回放；只有 `full` 模式才存响应体、重启后可回放。旧版本写入的带 body 行仍可回放；升级后新写入的 `redacted` 行只是元数据。


## 会接触的浏览器数据

| 数据 | 来源 | 使用条件 |
| --- | --- | --- |
| Cookie（含 `httpOnly`） | `chrome.cookies` | `atb cookies` 读取或清除 |
| 页面 DOM 与文本 | 内容脚本、`chrome.scripting` | `read` / `scan` / `snapshot` / `find` / `click` 等页面命令 |
| 网络请求与响应 | `chrome.debugger` 或页面 hook | `net hook` / `net log` / `network` 系列 |
| 控制台输出 | 页面与 `chrome.debugger` | `console start` 期间 |
| 下载记录与文件 | `chrome.downloads` | `downloads` / `download` / `download-wait` |
| 标签页、标签组、窗口 | `chrome.tabs`、`chrome.tabGroups` | `tabs` / `tabtree` / `open` / `close` / `cleanup` / `focus` |
| 已安装扩展列表 | `chrome.management.getAll`（**optional** 权限） | 仅在你于扩展设置页「关于 → 高级权限」手动授权后，`atb extensions` 只读列名/版本/启用状态；不启用、停用、卸载或修改任何扩展 |
| 扩展自身重载 | 扩展自身的 `chrome.runtime.reload()` 与部署指纹（`.atb-build.json`） | 部署新版本后自动/手动重载；不读取其它扩展的配置 |

页面数据只在 CLI（或经 CLI 转发的 Agent）主动下发命令时读取；扩展自身的管理与重载可能由 daemon 自动触发。扩展不会自行采集或外发页面内容。

**页面正文默认不落盘**：常规命令只在内存里组装响应，退出即丢。唯一的例外是你显式开启 `ATB_IDEMPOTENCY_PERSIST=full`：此时幂等记录会把响应体写进 `~/.atb/idempotency.jsonl`，单条上限 256KB、TTL 60 秒，并且该文件不在任何上传/同步路径上。默认的 `redacted` 档只写键、范围、时间与状态。除此之外本地留存的是连接配置、扩展/Profile 标识、诊断状态，以及你显式要求持久化的注入脚本。

扩展声明的完整权限列表以 `assets/atb_bridge/manifest.json` 为准（当前必需为 `cookies` / `tabs` / `tabGroups` / `activeTab` / `debugger` / `scripting` / `alarms` / `storage` / `declarativeNetRequest` / `webNavigation` / `downloads`；`management` 只作为 **optional_permissions**，默认不授予）。`declarativeNetRequest` 只用于启动时清理历史 CSP 规则。

**用户脚本的 GM 存储**：`inject --userscript` 提供最小 GM shim，`GM_setValue` / `GM_getValue` 把键值写进**当前页面自己的 `localStorage`**，键前缀是 `__atb_gm__:<namespace>:`（namespace 只保留 `[A-Za-z0-9._-]`，默认 `atb-userscript`）。它随站点 origin 存在、页面脚本可读，也会随站点数据一起被清除；不会写到 `chrome.storage`，不会上传。`GM_xmlhttpRequest` 明确不支持，不会替你发跨域请求。`GM_addStyle` / `GM_registerMenuCommand` 只影响当前页面 DOM。

## 不收集、不上传

- 无统计、无埋点、无崩溃上报、无广告 SDK。
- 无远程配置、无远程代码：扩展与页面脚本都打包在发行物内，不从网络加载或执行。
- 无账号体系，不需要登录，不采集设备指纹。
- 不向任何服务器发送 Cookie、页面正文、URL 列表或命令记录。

两个边界：

- 你操作的网页仍按站点自身逻辑联网，这部分流量与 atb 无关。
- `atb net fetch` 会按你给的 URL 发起请求，`atb open` 会导航到目标地址。这是命令语义，不是遥测。

## 日志可能包含什么

daemon 日志以排查为目的，可能记录：URL、错误码、错误上下文、扩展版本与协议版本、时间戳、端口与进程信息。

代码未对日志做统一脱敏，因此不要假定日志里一定没有敏感内容。日志可能间接带出页面地址等上下文；分享日志前请自行检查并按需删改。

## 你的控制权

- `atb stop`：停止 daemon，端口关闭，扩展随即断线。
- `atb cookies --clear`：清除当前标签页 URL 下的 Cookie；加 `--domain` 指定域。会退出相关网站的登录状态。
- 扩展 popup 的 Cookie 值默认是 `•••`，必须点「显示值」并在 5 秒内二次确认才会渲染；「复制 cookies」写入剪贴板后 30 秒自动清除（popup 里可调到 30–300 秒），离开页面时再清一次。
- 断开扩展：在扩展弹窗里断开，或直接禁用/卸载扩展。
- 撤销 `management`：扩展设置页「关于 → 高级权限」可撤销；撤销后 `atb extensions` 返回 `permission_required`，其它命令不受影响。
- 清掉用户脚本的 GM 存储：清除对应站点的站点数据（`localStorage`）即可；atb 不额外保留副本。
- 删除 `~/.atb` 整个目录：清掉配置、token、信任配置、幂等记录、规则、分配记录、banner 状态与日志。
- 卸载扩展：删除扩展本地存储，包括标识与持久化注入脚本。

## 第三方

没有。工具不集成第三方分析、推送、崩溃收集或广告服务。二进制依赖的许可见 [SOURCES.md](./SOURCES.md)。

## 变更与联系方式

本文件随功能变化更新，版本变更记录见 [CHANGELOG.md](./CHANGELOG.md)。仓库地址：<https://github.com/1728578755/atb-cli>。

隐私相关问题请开 issue：<https://github.com/1728578755/atb-cli/issues>。安全问题请按 [SECURITY.md](./SECURITY.md) 的私密渠道报告，不要公开贴细节。

## English summary

`atb` and its companion Chrome extension **ATB** talk only to `127.0.0.1` for tool traffic (WebSocket 18765 for the extension, HTTP 18767 for the CLI); the pages you drive still talk to their own sites.

There is no telemetry, no analytics, no ads, no remote configuration and no remote code; everything ships inside the package.

Local state lives in `~/.atb` (`config.json`, `api-token`, `ws-trust.json`, `idempotency.jsonl`, `net-hooks.json`, `agent-tabs.json`, `banner.json`, `logs/daemon.log`) plus the extension's own browser storage. `api-token`, `ws-trust.json` and `daemon.log` are 0600 inside a 0700 directory (Windows ACL for the token files). `banner.json` stores only the agent name, readable browser name and timestamp, and only when `ATB_BROWSER_BANNER=once` needs to avoid repeating the notice.

The `management` permission is **optional**: it is only used by `atb extensions` to read the installed-extension list after you explicitly authorize it in the extension settings, and the command is read-only.

The GM shim used by `inject --userscript` stores `GM_*` values in the page's own `localStorage` under the `__atb_gm__:<namespace>:` prefix; it never writes those values to `chrome.storage` or uploads them. `GM_xmlhttpRequest` is not supported.

Browser data (cookies including `httpOnly`, page DOM, network traffic, console output, downloads, tabs) is touched only when you run a command.

Page bodies are **not** persisted by default. The only exception is an explicit `ATB_IDEMPOTENCY_PERSIST=full`: idempotency records then store the response body in `idempotency.jsonl` (256KB per record, 60-second TTL). The default `redacted` mode stores only key, scope, timestamp and status.

Cookie values in the popup are masked as `•••` until you reveal them, and copied cookies are cleared from the clipboard after 30 seconds.

Daemon logs are written for troubleshooting and may contain URLs and error context; review them before sharing.

Remove everything with `atb stop`, delete `~/.atb`, and uninstall the extension.
