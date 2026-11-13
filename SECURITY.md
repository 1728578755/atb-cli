# 安全说明

本文件说明 `atb` 的安全边界、已接受的设计取舍，以及你可以做的加固。隐私与数据流见 [PRIVACY.md](./PRIVACY.md)；来源与依赖许可见 [SOURCES.md](./SOURCES.md)。

## 支持版本

只有最新 minor 接受安全修复。当前线是 `0.8.x`，最新版 `0.8.0`。

- 报告前先升级到最新 `0.8.x`，旧补丁版的问题可能在当前版已修复。
- `0.6.x` 及更早不再接受安全修复。
- 扩展版本需与 CLI 配套（当前扩展 `5.5.0`，消息协议 `4`）。

## 报告漏洞

优先用 GitHub Security Advisories 私密报告：在仓库 Security 页面选择「Report a vulnerability」。

- 仓库：<https://github.com/1728578755/atb-cli>
- 若私密报告不可用，开一个 issue，只写「需要私下联系」，不要贴利用细节、PoC 或受影响地址。
- 请附：版本（`atb --version`）、扩展版本、平台、复现步骤、期望与实际结果。
- 首次回复尽力在 3 个工作日内。修复时间取决于影响面与复杂度，本文件不承诺修复 SLA。

## 威胁模型

| 主体 | 信任假设 | 说明 |
| --- | --- | --- |
| 运行 atb 的本机用户 | 可信 | 你能读 `~/.atb`、能改配置，本工具不防你 |
| 同机其他进程与其他系统用户 | 不完全可信 | 能读文件、能占端口、能发本地 HTTP 请求，取决于系统权限 |
| 被操作的页面内容 | 不可信 | 页面脚本可探测、干扰、伪造你看到的内容 |
| 扩展与 daemon 之间的连接 | 本机回环 WebSocket | 无 TLS；四档信任模式（`compat` 默认 / `whitelist` / `token` / `both`），默认档只校验请求形状并在握手回包里带告警头 |

前提：攻击者若已能在本机以你的身份执行代码，atb 不是防线。它能放大这种访问（读登录态、驱动浏览器），但不能阻止它。

这套模型不覆盖：浏览器自身漏洞、被第三方修改过的扩展包、你主动交给 Agent 的凭据。

攻击面：

- `127.0.0.1:18765`：扩展接入的 WebSocket，默认「装了扩展就能连」（`ATB_WS_TRUST_MODE=compat`）。
- `127.0.0.1:18767`：CLI 调 daemon 的 HTTP API，需要 `api-token`；`Host` 头只接受回环名（`127.0.0.1` / `localhost` / `[::1]` / `::1`），其余返回 403 `host_not_allowed`。
- `~/.atb/api-token`：文件权限 0600；Windows 上在写入前收紧 ACL（去继承 + 移除 Everyone / Users 等宽松 ACE + 当前用户 SID 授权）。
- `~/.atb/ws-trust.json`：白名单与握手 token，可选；权限与 `api-token` 同等收紧（Unix 0600、Windows `icacls`，先收紧再写入）。
- `chrome.debugger`：attach 后能读写该标签页的网络与控制台。
- `<all_urls>` 注入：内容脚本在所有帧运行。
- 页面脚本执行：`exec` / `inject` 在页面上下文执行任意 JS。
- `optional_permissions: ["management"]`：只用于 `atb extensions` 读取已安装扩展列表；默认未授予，授权入口在扩展设置页，命令只读（`chrome.management.getAll`），不启用/停用/卸载。
- 用户脚本 GM 存储：`inject --userscript` 的 `GM_setValue` 写入页面自己的 `localStorage`（前缀 `__atb_gm__:<namespace>:`），同源页面脚本可读；不要把凭据放进去。
- 下载落盘：`download` 把文件写进浏览器下载目录，文件名来自站点。

## 已知设计取舍

| 取舍 | 现状 | 后果 |
| --- | --- | --- |
| `trust-extension` 双向校验（配 token 时） | daemon 在 WS upgrade 后立刻下发 `ws_hello`（nonce + HMAC proof）；扩展配了 token 时必须先验 proof 才发 `ext_ready` | 不知道 token 的本机进程无法冒充 daemon 拿到标签数据；**没配 token 时仍是单向**，扩展无法证明对端身份 |
| daemon 空闲退出 | 默认 300 秒无活动且无扩展连接时退出（`ATB_IDLE_TTL`）；扩展断开有 60 秒宽限 | 退出到下次启动之间，18765 可能被同机进程抢占 |
| `debugger` 权限 | `snapshot` 默认、纯 CSS 的 `click`/`fill`、`mouse-click`、`send-keys`、`upload`、`network`/`console`、整页/元素截图、`save-pdf`、`net mock --engine cdp`、挑战求解/点击会 attach | 浏览器顶部出现调试横幅，页面可感知 |
| `<all_urls>` 注入 | 内容脚本在所有站点与所有帧运行 | 页面可探测到本工具的信号面，用 `stealth audit` 量化 |
| optional `management` | 默认未授予；用户必须到扩展设置页点击授权；`atb extensions` 只调用 `getAll` | 授权后 CLI 能读到本机已安装扩展的名称/版本/启用状态；不能启用、停用或卸载 |
| 用户脚本 GM 存储 | `GM_*` 键值写入页面 `localStorage`（`__atb_gm__:<namespace>:`） | 同源页面脚本可读；不要把敏感数据放进 GM 存储，也不要在不可信页面注入来源不明的用户脚本 |
| Cookie 可读 | 能读含 `httpOnly` 的 Cookie | 拿到 CLI 或扩展控制权即可导出登录态 |
| 挑战自动交互 | `challenge` 会点击可交互组件并等待放行；失败时智能体用 `widget` + `click --at` 自行兜底，最后才转人工 | 站点不放行时超时失败；需要视觉/短信验证的仍要人 |

以上取舍是公开的、有意的。不要把「配置了 token」理解成安全边界，它只覆盖一个方向。

### WebSocket 信任模式（`ATB_WS_TRUST_MODE`）

| 档位 | 握手校验 | 适用 | 风险 |
| --- | --- | --- | --- |
| `compat`（默认） | 不校验身份，只保证协议形状；回包带 `x-atb-ws-trust-mode: compat` 与 `x-atb-ws-security-warning` | 单机、单人、装好即用 | 同机任何进程都能连 18765 并接管扩展；`Origin` 是自断言，可伪造 |
| `whitelist` | 扩展 ID 必须在 `trust-extension` 白名单内，缺配置或不在名单一律拒绝（`ws_whitelist_not_configured` / `ws_untrusted_extension`） | 多扩展共存、想限定来源 | 扩展 ID 可由扩展包决定，仍挡不住能读配置的本机进程 |
| `token` | 握手必须带匹配的 `ws-trust.json` token（常量时间比较；`ws_token_not_configured` / `ws_token_invalid`） | 共用机器、最小改动 | 扩展侧只能证明「知道 token」，无法证明对端是真 daemon |
| `both` | 白名单 + token 同时校验 | 共用机器 / 安全敏感 | 需同时维护两份配置 |

切档：

```bash
atb trust-extension <extensionId>            # 加白名单
atb trust-extension --token <value>          # 设置/轮换握手 token
ATB_WS_TRUST_MODE=both atb restart           # 重启后生效（daemon 读环境变量）
```

`whitelist` / `token` / `both` 都是 fail-closed：配置缺失或校验失败直接拒绝握手，不会退回 `compat`。`trust-extension --clear` / `--clear-token` 清空配置后，请同时把 `ATB_WS_TRUST_MODE` 改回 `compat`，否则 daemon 会持续拒绝所有连接。

### 双向握手（`ws_hello`）

`token` / `both` 档位下，daemon 在 WS upgrade 成功后**立即**下发：

```json
{"type":"ws_hello","v":1,"nonce":"<32 hex>","proof":"<64 hex>","auth_required":true}
```

`proof = HMAC-SHA256(key = ws-trust.json 的 token, msg = nonce + "|" + extension_id)`。
扩展侧：

1. 本地配了 `wsToken` 时，`onopen` **不发** `ext_ready`，先等 5 秒内的 `ws_hello`；
2. 用 WebCrypto 复算 HMAC，恒定时间比较；不一致 → 断开并记 `daemon_auth_failed`；
3. 校验通过才发 `ext_ready`（标签列表、版本、后续命令能力都在这一帧之后）。

因此「18765 被本机进程抢占」不再等价于「拿到全浏览器控制」：攻击者不知道 token 就算不出
proof，扩展不会泄露任何标签数据。**没配 token 时** `auth_required:false`，扩展会走旧的
即时 `ext_ready` 路径（兼容档），此时仍没有对端身份证明 —— 共用机器请务必设置 token。

`atb status` 的 `security` 块会显示当前档位与 `mutual_auth` 是否开启；`atb doctor` 在
`compat` 且未配 token 时给出 `ws_trust` 警告。

### HTTP API 的两个前提

- 只接受回环 `Host`：`127.0.0.1` / `localhost` / `[::1]` / `::1`（大小写不敏感）。这是对 DNS rebinding 的纵深防御，不替代 `api-token`。
- `api-token` 仍然必需：`/ping` 是唯一免鉴权端点，其余请求都要 `x-atb-api-token`；未授权的 POST 进不到幂等中间件，不会留下缓存或落盘记录。

### 幂等记录的落盘口径

`~/.atb/idempotency.jsonl` 默认 `ATB_IDEMPOTENCY_PERSIST=redacted`：只写 `{key, scope, at, status}`，**不写响应体**，因此不会把页面正文、Cookie 或网络响应带进日志目录。`full` 才会写入响应体（单条 256KB 上限，60 秒 TTL），`off` 完全不落盘。升级前旧版本写入的带 body 行仍可回放，但新写入的 `redacted` 行只用于审计、不参与回放。

## 加固清单

日常：

- 不要把 `atb stop` 当安全边界：它会释放 18765，扩展仍在重连；要收口就同时禁用扩展或关闭浏览器，或保持 daemon 运行占住端口。
- 保持 CLI 与扩展同版本；`atb status` 的 `compatibility` 不配套时先修版本。
- `atb doctor` 定位端口、token、daemon 状态；`--fix` 自动修复，加 `--force` 才会杀进程。

处理敏感账号时：

- `atb trust-extension <extensionId>` 限定扩展白名单，`--token <value>` 要求握手带 token；两者同时生效需显式设 `ATB_WS_TRUST_MODE=both` 并 `atb restart`（只配置不切档仍按 `compat` 放行）。
- 设 `ATB_NO_DEBUGGER=1`，彻底禁止 attach；需要 CDP 的命令会返回替代方案。
- 用专用的 Chrome Profile，避免日常登录态与自动化任务混在一起。
- 不要在不可信页面执行来源不明的脚本：`exec` / `inject` 拿到的代码会在页面里跑。
- `inject --userscript` 的 GM 存储落在页面 `localStorage`，同源脚本可读；不要把 token / 密码写进 `GM_setValue`。
- `management` 只在需要 `atb extensions` 时授权；用完可在扩展设置页撤销，其它命令不依赖它。
- 检查 `~/.atb` 及其文件的权限，确认只有本人可读：目录 0700、`daemon.log` 0600、`api-token` / `ws-trust.json` 0600（Windows 走 ACL）；`banner.json` 只含 agent、浏览器名与时间，删除即重置提示。
- 扩展 popup 默认把 Cookie 值渲染成 `•••`，点「显示值」后 5 秒内需二次确认；「复制 cookies」写入剪贴板后 30 秒自动清除（`clipboardTtlSec` 可在 popup 里调到 30–300 秒）。共用机器上不要跳过这一步直接复制。
- 幂等落盘保持默认 `redacted`；只有确实需要审计响应体时再用 `ATB_IDEMPOTENCY_PERSIST=full`，并把它当作「会写敏感数据」对待。

多人共用一台机器：

- CLI API 端口固定为 `18767`，不能同时跑两个 daemon。独立系统账号和 `ATB_HOME` 只能隔离配置与文件权限，解决不了端口冲突；需要并行使用请分机器/分容器，或约定同一时间只有一个用户运行 atb。
- 不要共用同一个浏览器 Profile 与同一个扩展实例。
- 需要时给 daemon 端口做系统级访问控制，而不是只依赖 token。

## 发布产物校验

下载 Release 或从 npm 安装后，可以逐层核对拿到的确实是本次构建的产物：

- **GitHub Release**：每个普通 tag 都附 `SHA256SUMS`（各平台二进制、npm tgz、`chrome-extensions.zip`）。
  `sha256sum -c SHA256SUMS` 逐条核对即可。
- **GPG 签名（可选）**：发布环境配置了 `GPG_PRIVATE_KEY` 时额外产出分离签名 `SHA256SUMS.asc`，
  用 `gpg --verify SHA256SUMS.asc SHA256SUMS` 校验后再核对哈希。**没有 `.asc` 说明本次发布未签名**，
  Release notes 会明确标注 unsigned，不要把它当成「签名丢失」。
- **npm 来源证明**：平台包与主包都用 `npm publish --provenance` 发布，npm 包页面上的 Provenance
  可以追溯到具体 workflow 与 commit；`npm audit signatures` 会一并校验。
- **平台包哈希**：主包 `package-lock.json` 里 5 个平台包的 `integrity` 与 registry 的 `dist.integrity`
  一致（发布门禁 `node scripts/check-lock.mjs --strict-integrity --registry` 强制要求）。
  平台包是原生二进制，`npm ci` 不做内容校验，所以这层哈希是唯一的内容锚点。

## 安全相关命令

| 命令 | 作用 | 何时用 |
| --- | --- | --- |
| `atb status` | 健康状态、版本与协议配套 | 每次升级后 |
| `atb doctor` | 诊断端口、token、daemon 版本；`--fix` 修复 | 连不上或行为异常 |
| `atb capabilities` | 扩展版本、daemon 版本、特性门禁 | 怀疑版本不配套 |
| `atb trust-extension <id>` | 扩展白名单；`--token`、`--list`、`--clear` | 多人共用或加固 |
| `atb stealth audit` | 审计页面可读到的痕迹信号，给出评分与发现项 | 怀疑被站点识别 |
| `atb stop` | 停止 daemon 并释放 18765 | 需要彻底停用时，同时禁用扩展或关闭浏览器 |

## 不属于漏洞的现象

- 需要本机代码执行才能利用的本地提权或横向移动。
- 用户在 `exec` / `inject` 中主动执行任意 JavaScript 造成的后果。
- 被操作网站自身的安全问题。
- 挑战页未通过、验证码未被破解导致的超时。
- 浏览器顶部出现调试横幅（attach 的既定行为，可用 `ATB_NO_DEBUGGER=1` 规避）。
- 同机进程在扩展断开、daemon 空闲退出后抢占端口（已知取舍，见上表）。
- `ATB_WS_TRUST_MODE=compat`（默认）下，同机进程伪造 `Origin` 连入 18765：这是默认档的已知取舍，要闭合必须切 `both`（白名单 + token），仅切 `whitelist` 或仅设 token 都留有余地。
- 共用系统账号时多个 atb 实例互相影响配置与端口。

## English summary

Only the latest minor (`0.8.x`) receives security fixes; upgrade before reporting.

Report privately through GitHub Security Advisories. If unavailable, open an issue that only says a private channel is needed, with no exploit details. We aim to reply within 3 business days and do not promise a fix SLA.

The local user is trusted; other local processes and page content are not. Traffic between the CLI, daemon and extension stays on `127.0.0.1`, using WebSocket 18765 (extension) and HTTP 18767 (CLI, `api-token`).

Known trade-offs: `trust-extension` proves the peer only when a token is configured (`ws_hello` + HMAC); without a token the extension cannot verify the daemon. A port window remains after the extension disconnects and the daemon idles out. `chrome.debugger` attach, `<all_urls>` injection, readable cookies, and challenge handling that interacts with the widget, lets the agent click by coordinates as a fallback, and hands over to a human only when needed.

The `management` permission is optional and only used by the read-only `atb extensions` command after the user grants it in extension settings. The GM shim used by `inject --userscript` stores values in the page's own `localStorage` under `__atb_gm__:<namespace>:`, readable by same-origin page scripts; do not put secrets there. `~/.atb/banner.json` stores only the agent name, readable browser name and timestamp for the once-per-10-minutes banner.

WebSocket trust has four modes via `ATB_WS_TRUST_MODE`: `compat` (default; shape-only, warning header), `whitelist`, `token`, and `both` (whitelist + token, fail-closed). `compat` accepts a forged `Origin` from any local process by design; switch to `both` to close that. The HTTP API only accepts loopback `Host` headers and still requires `api-token`.

Idempotency records default to `redacted` (key/scope/time/status only, no response body); `full` is opt-in. `daemon.log` is 0600 in a 0700 directory, `ws-trust.json` gets the same ACL treatment as `api-token`, clipboard copies are cleared after 30 seconds, and the popup masks cookie values until you reveal them.

Do not treat `atb stop` as a security boundary: it frees 18765 while the extension keeps reconnecting. Disable the extension or close the browser to close the window, or keep the daemon running to hold the port. Keep versions matched, and use `trust-extension` with `ATB_WS_TRUST_MODE=both` plus `ATB_NO_DEBUGGER=1` for sensitive work.
