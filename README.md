<div align="center">

# atb

面向 Agent 的浏览器 CLI：Rust daemon + Chrome MV3 扩展桥，操作用户已登录的真实 Chrome / Edge。

标签页扫描 · 页面读取 · JS 执行 · 点击填表 · 用户脚本 · 截图 / PDF · Cookie · 网络拦截 · CDP · 多智能体隔离

<p>
  <a href="https://github.com/1728578755/atb-cli"><img src="https://img.shields.io/badge/CLI-atb-2ea44f" alt="CLI atb"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-green" alt="License MIT"></a>
  <a href="https://github.com/1728578755/atb-cli"><img src="https://img.shields.io/badge/sys-win%2Fmac%2Flinux-0078D6?labelColor=0078D6&color=C0C0C0" alt="sys win/mac/linux"></a>
  <a href="https://github.com/1728578755/atb-cli/releases"><img src="https://img.shields.io/badge/release-v0.8.0-orange" alt="release v0.8.0"></a>
  <a href="https://github.com/1728578755/atb-cli/pulls"><img src="https://img.shields.io/badge/PRs-welcome-brightgreen" alt="PRs welcome"></a>
</p>

[30 秒上手](#30-秒上手) · [安装](#安装) · [能力地图](#能力地图) · [版本与配套](#版本与配套) · [安全与隐私](#安全与隐私) · [文档地图](#文档地图)

中文 | [English](README_EN.md)

</div>

## 它是什么

`atb` 让 Agent 用你已经打开的浏览器：一个常驻的 Rust daemon 通过 Chrome 扩展连上正在运行的 Chrome / Edge，CLI 每次只是一个短进程，往 daemon 发一条命令。

登录态、Cookie、已装的扩展、当前窗口都不变，因为用的就是你那个窗口。一条命令可以同时作用于多个标签页，结果按输入顺序返回 JSON，退出码区分「命令写错」和「稍后重试」，脚本不必解析错误文本。

daemon 在后台常驻；默认空闲 300 秒退出，但只要还有扩展连着就不退，所以连续操作不会每条命令都重新握手。

**不是什么**：不是 Selenium / Playwright（不启动一次性自动化 profile，默认也不接管浏览器；只有显式 `atb launch --restart` 才会重启它）；不是云浏览器（CLI、daemon、扩展之间的通信都在 `127.0.0.1`，你操作的网页仍按站点自身逻辑联网）；不是「隐身浏览器」（只减少本工具自身痕迹，不改指纹）。

## 30 秒上手

```bash
# 1. 安装 CLI，需要 Node >= 18
npm i -g @g1728578755/atb

# 2. 加载扩展：chrome://extensions → 开发者模式 → 加载已解压的扩展程序
#    选 Release 里 chrome-extensions.zip 解压后的目录（该目录直接含 manifest.json）；
#    源码/npm 包内也有同一份：assets/atb_bridge

# 3. 打开一个页面：目标命令会自动拉起 daemon
atb open https://example.com

# 4. 确认扩展桥通了
atb tabs --ids

# 5. 读当前页，或者在页面里执行 JS
atb read --tab current --max-chars 4000
atb exec 'return document.title'

# 需要完整健康检查时（status 不会拉起 daemon，先跑上面的命令）
atb status
```

扩展装好后不需要重启浏览器。命令返回 `ok:false` 时，看 `error_code` 与 `hint`（部分超时类只有 `error` / `reason`）；`atb schema --json` 能给 Agent 顶层命令的参数。

## 与 Selenium / Playwright 的差别

| 维度 | atb | Selenium / Playwright |
| --- | --- | --- |
| 会话来源 | 复用用户已登录的 Chrome / Edge | 启动全新 profile，登录态为空 |
| 并发粒度 | 一条命令并发多个标签页 | 需要自己写并发与连接池 |
| 输出形式 | compact JSON + 结构化 error_code / 退出码 | 语言原生对象与异常 |
| 检测面 | 读 / 执行 / 等待不 attach；snapshot、纯 CSS 的 click/fill、截图、PDF 默认会 attach（被拒时自动回落，见「页面禁止调试器注入时」） | 默认带自动化特征与 debugger |
| 适用场景 | Agent 操作用户自己的浏览器 | 从零构建的端到端测试 |

它不替代测试框架：需要隔离环境、并行跑几百个用例时，Playwright 更合适。

## 能力地图

| 域 | 代表命令 | 说明 |
| --- | --- | --- |
| 目标与环境 | `atb focus` / `atb use` / `atb env` | 切前台、固定默认目标、一次聚合浏览器 / 扩展 / 标签 / 注入状态 |
| 页面读取 | `atb read` | 正文抽取、链接、隐藏节点、iframe，按字符预算截断 |
| 页面操作 | `atb snapshot` / `atb measure` | 生成 `@e1` 引用；量测元素 rect、computed style 与 shadow 路径 |
| 脚本与批处理 | `atb exec` / `atb inject` / `atb userscript` / `atb scrape` | 执行 JS、多文件拼接、GM 兼容注入、油猴安装页、跨标签批量抓取 |
| 网络与调试 | `atb net` / `atb console` / `atb downloads` | 抓请求、mock 响应、console 历史与跟随、下载监听 |
| 挑战与痕迹 | `atb challenge` / `atb stealth` | 挑战页识别与主动求解；痕迹审计与清理 |
| 多浏览器与运维 | `atb browsers` / `atb extensions` / `atb doctor` / `atb logs` | 多浏览器 / Profile 选择、已安装扩展、诊断与日志 |

## 安装

### npm

```bash
npm i -g @g1728578755/atb
atb status
```

预编译二进制按平台从 `optionalDependencies` 安装，不需要 Rust 工具链。新脚本统一写 `atb`。

### 源码构建

```bash
cargo build --release
./target/release/atb status
```

自编译产物可用 `ATB_BIN` 指向，避免和全局安装混用。

### 加载扩展

1. 从 [最新 Release](https://github.com/1728578755/atb-cli/releases/latest) 下载 `chrome-extensions.zip`，解压到一个固定目录（zip 根目录就是扩展文件，`manifest.json` 直接在该目录里），或直接用仓库里的 `assets/atb_bridge`。
2. 打开 `chrome://extensions`（Edge 为 `edge://extensions`），开启开发者模式，点「加载已解压的扩展程序」，选中直接含 `manifest.json` 的那一层目录。
3. 浏览器里至少留一个正常网页标签页，不要只停在 `about:blank` 或 `chrome://`。
4. **本版本需要扩展 5.5.0**。覆盖扩展目录后，daemon 在下次握手时会让扩展自动重载；daemon 未运行就到扩展管理页手动点「重新加载」——只覆盖文件不重载，页面里跑的还是旧代码。
5. 点工具栏的 ATB 图标打开弹窗：顶部是连接状态，下面分「状态 / 数据 / 设置」。右上角齿轮进入**管理页面**（也可从 `chrome://extensions` → 详情 → 扩展程序选项 打开），可改端口、token、信任档位、主题、弹窗宽度与间距，并查看 Cookie、心跳与自检结果。要用 `atb extensions` 时，到管理页「关于 → 高级权限」点击授权 `management`；这是可选权限，未授权时命令返回 `permission_required`。授权成功后扩展会自动重载一次（MV3 Service Worker 需要重载才能拿到 `management` API），页面刷新后即可运行。

### 验证

```bash
atb tabs --ids           # 先跑它：自动拉起 daemon，也能确认扩展桥通了
atb status               # 再做健康检查；直接先跑 status 会看到 daemon_not_running
atb env                  # 浏览器 / Profile / 扩展 / 标签 / 注入一次看
atb doctor               # 连不上时用它定位端口、token、daemon 状态
```

### 平台矩阵

| 平台 | 预编译 | 说明 |
| --- | --- | --- |
| macOS arm64 / x64 | 是 | — |
| Linux glibc x64 / arm64 | 是 | 本机 Chrome / Chromium 需支持安装扩展；WSL 2 也用这个包 |
| Windows x64 | 是 | 原生 Windows；WSL 不要用 win32 包 |
| Windows ARM64 | 否 | 需 `cargo build --release`，用 `ATB_BIN` 指向产物 |
| Alpine / musl | 否 | 同上，需自编译 |

WSL 需 `WSL 2.0.0+`，建议 Windows 11 22H2+ 并在 `.wslconfig` 开 `networkingMode=mirrored`，让 WSL 能连宿主机 `localhost` 上的桥接服务。

## 核心概念

### daemon

`atb` 二进制发现没有 daemon 时会拉起一个常驻进程，监听 `127.0.0.1:18767` 提供 HTTP API，后续命令复用同一会话。默认空闲 300 秒且没有扩展连接时退出；只要还有扩展连着就不退，扩展断开后有 60 秒重连宽限。`ATB_IDLE_TTL` 可改。日志写在 `~/.atb/logs/daemon.log`。

### 扩展桥

扩展以 WebSocket 连到 `127.0.0.1:18765`，端口来自 `~/.atb/config.json` 的 `extension_port`。daemon 下发命令，扩展在标签页里执行后回传 JSON。

`status` / `tabs` / `browsers` 里的 `browser_label` 带品牌与完整版本（如 `Chrome 156.0.8078.79 · Windows · #cac3`），多浏览器并行时可直接区分是哪一个、什么版本。扩展重载或浏览器重启会让该浏览器名下的标签 id 全部失效，此时返回 `session_gone`，重新 `open` 或先 `tabs` 取新列表。`status` 另外给出 `browser` / `browser_label` / `profile` / `browser_summary`，并在 stderr 打一行「→ 正在操作：…」摘要。

### 智能体身份

身份优先级：`--agent` → `ATB_AGENT` → 终端会话标识（`WT_SESSION` / `TMUX_PANE` / `VSCODE_IPC_HOOK_CLI` 等）→ `default`。`open` 只改自己身份的默认目标，`cleanup` 只关自己开出来的标签页，多个 Agent 共用一个 daemon 也不会互相抢页面。

### 目标选择

`--tab` 接受单个 id、`111,222`、`all`、`@1` / `@1-5`、`@latest`、`url:<子串>`、`title:<子串>`、`group:<标题>`、`window:<id>`、`current`。多目标并发执行、按输入顺序返回、单条失败不影响其它；写文件时 `--out` 必须含 `{tab}` 或 `{index}`，否则预检拒绝。

- `@latest` 是 daemon 记录的「最近打开 / 最近连接」标签页；`@1` 仍然是 `tabs` 输出顺序的第 1 个，两者不是一回事。
- `atb use <target>` 把当前智能体的默认目标固定到某个标签页（命中多个会报 `target_ambiguous`，不会静默取第一个）；之后不写 `--tab` 的命令都用它。
- `atb focus --tab @latest` 把标签切到前台；`--no-window` 只激活标签、不抢系统窗口焦点。
- 单目标页面命令的响应顶层新增 `resolved_target`（tab_id / session_key / url / title / browser_id / profile_id / active）、`browser`、`profile`、`warnings`、`hint`。目标在后台时 `warnings` 带 `tab_hidden` / `timers_throttled`，并提示先 `focus`（`ATB_HIDDEN_TAB_WARNING=off` 可关）。
- CLI 默认在 stderr 打一行「→ 正在操作：…」；`ATB_BROWSER_BANNER=off` 关闭，`always` 每条命令都打。

### 协议

三个版本必须配套：CLI ↔ daemon 的 API 协议（当前 **5**）、扩展消息协议（当前 **4**）、扩展文件版本（当前 **5.5.0**）。`atb status` 的 `compatibility` 字段会同时给出期望值与实际值，不配套时带 `fixes[]`。

### 0.8.0 / 扩展 5.5.0 的新增能力

顶层命令从 55 个增加到 **61** 个：新增 `focus`、`use`、`env`、`extensions`、`measure`、`userscript`。

| 能力 | 说明 |
| --- | --- |
| 目标控制 | `focus` 把标签切前台（`--no-window` 不抢窗口焦点）；`use <target>` 固定当前智能体的默认目标；`--tab @latest` 表示最近打开 / 连接（`@1` 仍是列表第 1 个） |
| 目标身份 | 单目标页面命令响应顶层带 `resolved_target` / `browser` / `profile` / `warnings` / `hint`；后台标签带 `tab_hidden` / `timers_throttled`，CLI stderr 打一行「→ 正在操作：…」 |
| 环境与扩展 | `env` 一次输出浏览器 / Profile / 扩展 / 标签摘要 / 持久注入 / 警告，默认目标与激活标签排最前；`extensions` 列出已安装扩展（`management` 是可选权限，需用户点击授权） |
| 元素量测 | `measure --selector` 返回 rect / viewport / computed style / visible / `shadow_path`，支持 `host >>> inner`，不接受 `@e`；成功结果不再套 `js_return` |
| 用户脚本 | `userscript <file-or-url>` 解析 `==UserScript==` 并打开安装页；`inject --userscript` 注入最小 GM shim，支持 `--cleanup-selector` / `--cleanup-file` 清理旧宿主 |
| 截图等待 | `screenshot --settle-ms` / `--wait-selector` / `--network-idle[=ms]` / `--require-cdp`；结果带 `tab_visible` / `captured_tab` / `clip_box` / `degraded` / `warning` / `warnings` |
| tabs 过滤 | `tabs --url-contains` / `--title-contains` / `--filter` / `--format json\|jsonl\|tsv`；大列表附 `suggest` |
| console 历史 | `console history` 取回加载期 error / unhandledrejection（`--settle-ms` / `--types` / `--clear`）；`console tail` 轮询跟随，不自动启动采集 |
| job 进度 | `job tail <id>` 输出 JSONL 进度快照（`elapsed_ms` / `last_heartbeat` / `stalled`）；`job result <exec_id>` 取回超时 exec 的迟到结果 |
| Shadow DOM | `find` / `snapshot` / `read` 递归 open shadow root，结果带 `scope:"shadow"`、`host_selector`、`shadow_path`，选择器写作 `宿主 >>> 相对路径`；closed shadow root 不可达，回包用 `shadow_closed_unreachable` 明示 |
| PDF 分页 | 图片型 PDF 按 14200pt 切片成多页，并先按 `dpr` 归一化，HiDPI 下不再放大；回包给 `pages` / `width_pt` / `height_pt` |
| `--max-height` | CDP 主截图路径与拼图回落都生效，超出截断并标 `truncated:true` |
| 下载名净化 | `download --filename` 取 basename，拒绝 `..` / 分隔符 / Windows 保留名；回包 `filename_sanitized` |
| 下载取消 | `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT=1` 让 `download-wait` 超时时主动取消，`ATB_DOWNLOAD_CANCEL_ERASE=1` 再擦除记录 |
| `broadcast` | `--args` 里的路径按工作目录绝对化，`--dry-run` 在本地出计划 |
| `scrape` | 传输层失败标 `outcome_unknown`（退出码 4）且不重试；`--concurrency` / `--batch` 值域 1..=32 |
| `tabs --limit 0` | 表示不截断；`tabs_count` 仍是原始总数 |
| `logs` | 输出 JSON：`{ok, lines[], path, truncated}` |
| WS 信任档位 | `ATB_WS_TRUST_MODE=compat\|whitelist\|token\|both`，默认 `compat`；共用机器建议 `both` |
| 扩展界面 | popup 分「状态 / 数据 / 设置」，右上角进入管理页面：主题（跟随系统 / 浅色 / 深色）、弹窗宽度（340 / 420 / 560）、间距、角标开关、token 与信任档位、Cookie 与诊断；设置存在 `storage.local`，两个入口实时同步 |

## 常用工作流

### 读一个页面

```bash
atb tabs --ids
atb read --tab <tabId> --max-chars 8000
atb read --include-hidden          # 连 display:none 的内容一起返回
atb scan --fast                    # 只要紧凑文本，大页面也快
```

读之前先确认响应顶层的 `resolved_target`：它告诉你这次实际打到哪个 tab、哪个浏览器 / Profile。如果 `warnings` 里有 `tab_hidden` / `timers_throttled`，说明目标在后台，rAF、IntersectionObserver 与定时器会被暂停或节流；需要懒加载或动画时先 `atb focus --tab <id>`。CLI 还会在 stderr 打一行「→ 正在操作：…」，`ATB_BROWSER_BANNER=off` 可关。

`open` 遇到 `chrome://` / `chrome-extension://` / `about:` 特殊页时默认只用 3 秒等待预算（`ATB_OPEN_SPECIAL_WAIT_SEC`）；不需要等页面条件就写 `--wait-for none`。`chrome-extension://` 也可以打开本工具自己的管理页。

### 填表并提交

```bash
atb snapshot --limit 200           # 生成 @e1 / @e2 引用
atb fill @e2 '张三' --dry-run       # 先预检参数与目标
atb fill @e2 '张三'
atb click @e1
atb wait --selector '.result' --timeout 10
```

`snapshot` 默认、纯 CSS 选择器的 `click` / `fill` 会 attach 调试器；用 `snapshot --js` 和 `@e` 引用可走页面内通道，避免调试横幅。

等框架渲染完不要固定 `sleep`：`atb wait --dom-stable` 等 DOM 连续静默 500ms（`ATB_WAIT_DOM_STABLE_MS`），`atb wait --network-idle` 等在途 fetch / XHR 归零并静默；两者都可写成 `--dom-stable 800` 覆盖窗口。`wait --js` 超时会返回最近 5 次求值的 `last_values`，用来判断条件差在哪。

### 测试用户脚本（Tampermonkey 兼容）

场景：前端脚本在真实站点上不生效，需要注入一段油猴脚本或复现 `GM_*` 环境。

```bash
# 1. 打开目标页并切到前台；后台标签会节流定时器，懒加载可能不触发
atb open https://example.com/app --background
atb focus --tab @latest

# 2. 先解析 ==UserScript== 元数据，确认 @name / @match / @grant
atb userscript ./fix.user.js --parse-only

# 3. 按 GM 环境注入，并清掉旧宿主节点
atb inject --userscript --file ./fix.user.js --gm-namespace fix-demo \
  --cleanup-selector '#legacy-host' --cleanup-selector '.old-panel'

# 4. 断言效果并截图留证
atb exec 'return {host: !!document.querySelector("#fix-host"), flag: window.__FIX__ === true}'
atb screenshot --selector '#fix-host' --out fix-host.png

# 5. 需要跨导航保留时再 --persist；默认 2 小时过期
atb inject --userscript --file ./fix.user.js --persist fix-demo --match 'https://example.com/*' \
  --cleanup-selector '#fix-host'
atb inject --list
atb inject --remove fix-demo
```

- `inject --userscript` 注入最小 GM shim：`GM_getValue` / `GM_setValue` / `GM_deleteValue` / `GM_listValues` / `GM_registerMenuCommand` / `GM_log` / `GM_addStyle` / `GM_info` / `unsafeWindow`。`GM_xmlhttpRequest` 不支持，会直接抛错；`@require` 不会自动加载。
- `inject` 返回 `executed` / `returned` / `threw` / `duration_ms` / `cleanup`；脚本抛错时顶层 `ok:false` + `error_code:"inject_script_threw"`，但 `executed:true` 说明副作用可能已经发生。
- `--persist` 默认 7200 秒（2 小时）过期，扩展设置页的 `persistTtlSec` 参与默认值，`--ttl` 可覆盖。永久驻留必须同时写 `--persist-forever --i-know-this-is-long-lived`；`--list` 会给出 `expires_in_sec`、`warning` 与条目总数。
- 页面里已经装了 Tampermonkey 版本时，两边会各插一份宿主。先用 `--cleanup-selector` 清旧节点，或让用户临时停用安装版；排查见 [troubleshooting.md](./skills/atb/references/troubleshooting.md)。
- `userscript <file-or-url>` 只是解析元数据并打开安装页：本地文件按 `file://` 打开，由 Tampermonkey / Violentmonkey 接管；`--parse-only` 只解析本地文件、不打开页面，URL 需先下载到本地。

### 批量抓取

```bash
atb scrape --urls urls.txt --out out.jsonl
atb scrape --urls urls.txt --js-file grab.js --out out.jsonl --batch 6 --concurrency 6 --retry 2
```

`--urls` 收文件（每行一个）或逗号分隔列表；缺省脚本取 `{title, url, text}`。全部在 CLI 侧编排，复用 daemon 的 open / exec / close。

长任务用 `job tail` 看进度，超时的 `exec` 用 `job result` 取回迟到结果：

```bash
atb --async scan --text-only
atb job tail <jobId>                  # JSONL 快照：elapsed_ms / last_heartbeat / stalled
atb job result <exec_id> --wait 60    # exec 超时错误里给出的 exec_id
```

`job tail` 每个状态变化输出一行 JSONL，终态退出码 `0` / `1` / `130`；跟踪超时是 `3`。`job result` 在等待窗口内找不到结果返回 `exec_result_not_found`（等待超时退出码 `3`）；迟到结果默认保留 3600 秒，可用 `ATB_EXEC_RESULT_TTL_SEC` 调整。

`exec` 支持 `--file a.js --file b.js` 按顺序拼接、`--file -` 读 stdin、`--max-chars`（`--max-bytes` 的可见别名）截断返回值。本地 JS 配平检查默认 `strict`（`ATB_JS_CHECK=off|warn|strict`）；确认是误报时用 `--no-preflight`。脚本文件读不到或超限会带绝对路径、大小与 `invalid_encoding` / `script_file_too_large`。

### 多标签并发

```bash
atb open https://a.com https://b.com --background
atb read --tab all --max-chars 2000
atb exec --tab @1-3 'return location.href'
atb focus --tab @2
atb tabs --url-contains example --format tsv
atb screenshot --tab all --out 'shots/{index}-{tab}.png'
```

`tabs --format` 支持 `json`（默认）/ `jsonl`（一行一个标签页）/ `tsv`（`tab_id`、标题、URL）；`--url-contains` / `--title-contains` / `--filter` 在 daemon 侧过滤。标签很多时响应会附 `suggest`，提示用 `--fields` / `--limit` 或过滤条件收窄。

截图前要等渲染稳定时，用等待参数而不是固定 `sleep`：

```bash
atb screenshot --wait-selector '#chart' --network-idle --settle-ms 200 --out chart.png
atb screenshot --full-page --require-cdp --out full.png
```

`--network-idle` 不带值表示等到在途 fetch / XHR 归零并静默 500ms（`ATB_SCREENSHOT_NETWORK_IDLE_MS`），也可写 `--network-idle 800`；`--settle-ms` 默认 `ATB_SCREENSHOT_SETTLE_MS`（0）。`--require-cdp` 让 CDP 不可用时直接报 `cdp_required`，不再静默回落 `captureVisibleTab` 或整页拼接。结果里的 `tab_visible` / `captured_tab` 说明实际截到哪个标签，`clip_box` 回显 CSS px 裁剪框，`degraded` / `warning` / `warnings` 标注降级通道。

整页截图用 `captureBeyondViewport` 时，`position:fixed` / `sticky` 元素仍可能在页面底部重复出现；这是浏览器渲染限制，不是裁剪错误。需要干净长图时先临时改成 `position:absolute`（用 `inject`）或改用逐屏拼接通道，并接受固定元素每屏重复。

#### 页面禁止调试器注入时（自动降级）

Chrome 不允许 `chrome.debugger` attach 到「内嵌了其它扩展 iframe」的页面（助手类扩展常这么做）。
以前这类页面上的截图/PDF 直接报错；现在自动降级，不需要你先去关扩展：

| 命令 | 降级后的通道 | 返回标记 |
| --- | --- | --- |
| `screenshot` | 可见区域 `chrome.tabs.captureVisibleTab`（不 attach 调试器） | `via:"captureVisibleTab"` |
| `screenshot --full-page` | 逐屏滚动 + OffscreenCanvas 拼长图 | `via:"captureFullPageStitched"`、`stitched:true`、`slices`、`truncated` |
| `save-pdf` | 整页长图包成单页 PDF（文字不可选中） | `via:"stitched-image-pdf"`、`text_selectable:false` |
| `mouse-click` | 页面内合成鼠标事件 | `synthetic:true`、`trusted:false` |

`--max-height` 限制长图高度（设备像素）：CLI 不传即不截断，扩展整页拼图通道的默认上限是 20000；超出按上限截断并标 `truncated:true`。拼接方案的固有限制：`position:fixed/sticky` 元素会在每一屏重复出现；`save-pdf` 的文字不可检索。
需要真实输入事件或可检索 PDF 时，再考虑临时关闭注入 iframe 的扩展；需要确定性通道时加 `--require-cdp`，CDP 不可用会直接失败而不是静默降级。

### 多智能体

```bash
atb --agent reviewer open https://a.com
ATB_AGENT=reviewer atb read
atb cleanup --dry-run      # 先看会关哪些
atb cleanup                # 只关本智能体 open 的标签页
```

### 挑战页

```bash
atb open https://example.com                 # 默认 --challenge auto：命中后先主动求解，再等放行
atb challenge status                         # 看是否仍在挑战页、组件位置与求解结论
atb challenge solve --timeout 120            # 手动再跑一次主动求解
atb challenge widget                         # 求解失败：拿组件坐标
atb challenge click --at 320,410             # 智能体自己点，不需要人工
atb challenge focus                          # 只有图片/短信等必须人做的验证才转人工
```

`open` 与 `challenge solve` 不是干等：它们会定位挑战组件、用带轨迹的可信鼠标事件点击，再等 token 或跳转。求解失败时，智能体可以用 `widget` + `click --at` 自己兜底；`challenge wait` 才是纯等待，`focus` 是最后的人工通道。细节见 [docs/ANTI_DETECTION_AND_CHALLENGE.md](./docs/ANTI_DETECTION_AND_CHALLENGE.md)。

## 命令速查

顶层命令共 **61** 个；下面是按场景挑出的高频用法，完整清单以 `atb schema --json` 为准。

```bash
# 目标与环境
atb focus --tab @latest                    # 把最近打开 / 连接的标签切到前台
atb use @latest                            # 固定为当前智能体的默认目标
atb env                                    # 浏览器 / Profile / 扩展 / 标签 / 注入一次看
atb extensions                             # 已安装扩展（需先授权 management）
atb measure --selector 'my-app >>> .card' --styles display,position,width,height
atb userscript ./fix.user.js --parse-only  # 只解析 ==UserScript==

# 读取
atb tabs --ids                            # 精简列表，方便选目标
atb tabs --url-contains github --format tsv
atb read --max-chars 8000 --links
atb snapshot --limit 200                  # 元素引用 @e1 / @e2
atb find --text '登录' --limit 5

# 操作
atb open https://example.com --background
atb click @e1
atb fill @e2 'text'
atb select '#city' --value sh
atb upload 'input[type=file]' ./a.pdf
atb wait --selector '.result' --timeout 10
atb close --tab <tabId>

# 脚本与批处理
atb exec 'return document.title'
atb exec --file pre.js --file main.js     # 按顺序拼接；--file - 读 stdin
atb inject --userscript --file fix.user.js --cleanup-selector '#old-host'
atb run steps.json                        # 一个进程跑多步
atb batch steps.json --concurrency 8
atb scrape --urls urls.txt --out out.jsonl
atb --async scan --text-only              # 立刻返回 job_id
atb job tail <jobId>                      # JSONL 进度快照
atb job wait <jobId> --timeout 300
atb job result <exec_id> --wait 60        # 取回超时 exec 的迟到结果

# 网络与调试
atb net fetch https://api.example.com/me
atb net hook --capture-bodies
atb net mock /api/user --status 200 --body '{"id":1}'
atb console start
atb console history --types error,rejection   # 加载期报错一次取回
atb console tail --duration 10                # 轮询跟随，不自动 start
atb downloads --limit 10

# 挑战与痕迹
atb challenge status
atb stealth audit
atb stealth scrub

# 运维
atb status
atb env                                   # 一次拿齐运行环境
atb extensions                            # 已安装扩展
atb doctor --fix
atb capabilities
atb schema --json                         # 顶层命令的参数 schema（子命令细节用 --help）
atb logs --tail 100
```

完整参数以 `atb schema --json` 为准，按域整理的表格在 [skills/atb/references/commands.md](./skills/atb/references/commands.md)。

### 退出码

| 码 | 含义 | 怎么处理 |
| --- | --- | --- |
| `0` | 成功 | — |
| `1` | 执行失败 | 看 `error_code`；可重试的按 `retry_after_ms` 等待 |
| `2` | 命令写错 | 改选择器 / JS / 参数 / 目标，不要重试 |
| `3` | `job wait` / `job tail` / `job result --wait` / `download-wait` 超时 | 加 `--timeout`，或稍后再查询；普通 `wait` 超时是 `1` |
| `4` | 结果未知 | 动作可能已执行；带同一个 `--idempotency-key` 重发 |
| `130` | 任务被取消 | `job wait` 等到 `cancelled` |

0.8.0 新增错误码（都带 `hint`）：

| error_code | 含义 | 怎么处理 |
| --- | --- | --- |
| `focus_failed` | 标签切前台失败 | `tabs --ids` 确认标签仍在；扩展过旧就重载扩展 |
| `permission_required` | `extensions` 未获 `management` 权限 | 扩展设置页「关于 → 高级权限」由用户点击授权；授权成功后扩展会自动重载，刷新后重试 |
| `daemon_outdated` | 常驻 daemon 还是旧二进制，不认识新路由 | `atb restart` 后重试 |
| `syntax_error` | 本地 JS 语法检查失败 | 按提示行列修正；确认无误时用 `--no-preflight` 或 `ATB_JS_CHECK=off` |
| `inject_script_threw` | 脚本已执行但自身抛错 | 看 `executed` / `threw`；这是脚本异常，不是注入通道失败 |
| `unsupported_selector` | 选择器形态不支持（如 `measure` 传 `@e`） | `find` 拿真实 CSS；shadow DOM 用 `host >>> inner` |
| `exec_result_not_found` | 超时 exec 的迟到结果不存在或已过期 | 确认 `exec_id`；长任务加大 `ATB_EXEC_RESULT_TTL_SEC` 或改用 `--async` + `job wait` |
| `cdp_required` | 指定 `--require-cdp` 但 CDP 不可用 | 去掉该 flag，或关闭挡住调试器的扩展 |
| `invalid_encoding` | 脚本文件不是 UTF-8 | 转成 UTF-8，或用 `--file -` 从 stdin 传入 |
| `script_file_too_large` | 脚本文件超过上限 | 拆成多个 `--file`，或提高 `ATB_SCRIPT_FILE_MAX_MB` |

## 版本与配套

| 项 | 值 |
| --- | --- |
| CLI / daemon | `0.8.0` |
| CLI ↔ daemon API 协议 | `5` |
| 扩展消息协议 | `4` |
| 扩展文件版本 | `5.5.0` |
| 最低 Chrome | `120` |

- 当前版本：`0.8.0`
- 配套扩展：`5.5.0`（显示名 `ATB`）

```bash
atb status
# compatibility: { ok, cli_protocol, daemon_protocol,
#                  extension_protocol_expected, extension_protocol_connected, fixes[] }
```

| 症状 | 处理 |
| --- | --- |
| `compatibility.ok:false`，两个协议号不等 | `atb restart`；仍旧则结束占用 `18767` 的进程 |
| `error_code:"extension_outdated"` | 用 `assets/atb_bridge` 覆盖扩展目录，再到扩展管理页点「重新加载」 |
| `status` 里 `upgrade_recommended:true` | 核心命令仍可用，按 `next_steps` 更新扩展 |
| `stealth` 返回 `extension_ok:false` | 扩展低于 5.0.0，或覆盖后没重载；按 `fixes` 处理后复核 |

升级注意：CLI 与 daemon 协议不一致时新版 CLI 会直接拒绝连旧 daemon 并给出修复步骤；配置目录有意保持 `~/.atb` 不变，改目录会让新旧 daemon 各占一套端口。

## 运维

```bash
atb status                       # ok 跟随 healthy，不健康时退出码非 0
atb doctor                       # 诊断端口、token、daemon 版本
atb doctor --fix                 # 自动修复；加 --force 才会杀进程
atb logs --tail 100
atb restart                      # 协议或配置变更后重启 daemon
atb stop                         # 停止 daemon；会释放 18765，扩展仍在时会继续重连
atb set-extension-port 18766     # 改扩展端口，写 config.json 并重启 daemon
atb install-skill --dry-run      # 先看会把 skill 装到哪
atb install-skill                # 主目录 ~/.agents/skills/atb；五个 Agent 目录建软链接
```

`atb env` 一次聚合 `browser` / `profile` / `extension` / `tabs` / `injections` / `warnings`：`tabs.shown` 最前面是默认目标与当前激活标签，并带 `is_default`；`warnings` 会提示扩展过期、默认目标在后台、持久注入数量等。`atb extensions` 依赖可选的 `management` 权限，未授权时返回 `permission_required`，按提示到扩展设置页「关于 → 高级权限」授权。

`18767` 是 CLI 的 HTTP API 端口，固定不可改，也不能拿来当扩展端口。`ATB_EXIT0=1` 可让 `status` 恢复旧版恒返回 0 的行为，仅在有脚本依赖旧行为时使用。

升级 CLI：`npm i -g @g1728578755/atb@latest`，然后按「加载扩展」一节更新扩展文件。完整版本记录见 [CHANGELOG.md](./CHANGELOG.md)，交给 AI 执行的升级流程见 [AI_INSTALL.md](./AI_INSTALL.md)。

## 环境变量

所有阈值、档位与开关都能用 `ATB_*` 显式调参；旧前缀 `AGENT_BROWSER_CLI_*` 仍兼容，新名优先。下表是用户可见的 70 个，测试/CI 专用的 9 个单列在最后。

### 身份与运行位置

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_AGENT` | 终端会话标识 → `default` | 调用方身份，决定默认目标与 `cleanup` 范围 |
| `ATB_HOME` | `~/.atb` | 配置目录；只影响配置与 token，不影响 `~` 展开 |
| `ATB_BIN` | 自动解析 | 指向自编译的 atb 二进制 |
| `ATB_EXIT0` | 关 | `status` 恢复旧的恒 0 退出码，值须为 `1` / `true` |
| `ATB_NO_DEBUGGER` | 关 | 彻底禁止 `chrome.debugger` attach |
| `ATB_BROWSER_BANNER` | `once` | CLI stderr 的「→ 正在操作：…」提示：`off` / `once` / `always` |

### 结果语义与超时

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_RESULT_STRICT` | `warn` | 组合命令严格度：`off` / `warn` / `strict` |
| `ATB_HIDDEN_TAB_WARNING` | `true` | 目标标签在后台时是否附 `tab_hidden` / `timers_throttled` 警告与 `focus` 提示 |
| `ATB_OPEN_SPECIAL_WAIT_SEC` | 3 | `chrome://` / `chrome-extension://` / `about:` 特殊页的 `open --wait-for` 预算（0.5–60 秒） |
| `ATB_WAIT_DOM_STABLE_MS` | 500 | `wait --dom-stable` 的 DOM 静默窗口（50–10000 毫秒） |
| `ATB_WAIT_MAX_TOTAL_SEC` | 600 | `wait` / `--wait-js` 整条命令总预算（1–3600 秒） |
| `ATB_WAIT_MIN_INTERVAL_MS` | 50 | 轮询下限，防 `interval=0` 忙轮询 |
| `ATB_HTTP_SLACK_SEC` | 10 | CLI HTTP 超时在业务超时上追加的余量 |
| `ATB_CHALLENGE_MAX_IFRAMES` | 8 | 单次挑战探测处理的 iframe 数（1–64） |
| `ATB_CHALLENGE_CLICK_TIMEOUT_SEC` | 8 | 挑战坐标点击整条链的硬上限 |
| `ATB_DIAG_DEPTH` | 3 | 诊断里保留的底层错误条数（0–20） |

### 文件与产物

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_IO_OVERWRITE` | `backup` | `--out` 已存在时：`backup` / `force` / `ask` / `append` |
| `ATB_IO_SANITIZE_FILENAME` | `strict` | 下载名净化；`compat` 只告警不改名 |
| `ATB_IO_MAX_OUTPUT_MB` | 50 | 单文件 / 内存缓冲上限（1–512MB） |
| `ATB_SCRIPT_FILE_MAX_MB` | 32 | `exec --file` / `inject --file` 单个脚本文件上限（1–256MB） |
| `ATB_SCREENSHOT_SETTLE_MS` | 0 | `screenshot` 未给 `--settle-ms` 时的额外等待（0–10000 毫秒） |
| `ATB_SCREENSHOT_NETWORK_IDLE_MS` | 500 | `screenshot --network-idle` 的静默窗口（50–10000 毫秒） |
| `ATB_CAPTURE_MAX_PIXELS` | 80000000 | 拼接截图 / 图片 PDF 的像素总量上限（100 万–5 亿） |

### 隐私与幂等

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_IDEMPOTENCY_PERSIST` | `redacted` | 幂等记录落盘：`redacted`（只存元数据）/ `full`（含响应体）/ `off` |
| `ATB_IDEMPOTENCY_FILE_MAX_MB` | 32 | `idempotency.jsonl` 运行期上限，超限后台压缩（1–512MB） |

### 会话与多智能体

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_SESSION_ISOLATION` | `legacy` | `strict` 时具名 agent 无默认目标直接 `target_lost` |
| `ATB_MULTI_SCOPE` | `agent` | `--tab all` 的默认归属范围；`all` 才跨 agent |
| `ATB_PREFERRED_MISS_LIMIT` | 3 | 首选默认目标连续未命中多少次后放弃 |
| `ATB_PREFERRED_MISS_TTL_SEC` | 120 | 首选默认目标未命中持续多久后放弃（1–86400 秒） |

### 脚本、注入与任务

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_JS_CHECK` | `strict` | 本地 JS 配平检查：`off` / `warn` / `strict`；`exec` / `inject` 的 `--no-preflight` 只跳过单条命令 |
| `ATB_EXEC_RESULT_TTL_SEC` | 3600 | 超时 exec 的迟到结果保留时长（60–86400 秒） |
| `ATB_INJECT_PERSIST_DEFAULT_TTL_SEC` | 7200 | `inject --persist` 未给 `--ttl` 时的默认寿命（60–2592000 秒） |
| `ATB_INJECT_PERSIST_MAX_TTL_SEC` | 2592000 | `inject --ttl` 允许的最大值（60–31536000 秒） |
| `ATB_CONSOLE_HISTORY_SETTLE_MS` | 300 | `console history` 挂上监听后的采样窗口（50–5000 毫秒） |
| `ATB_JOB_STALL_SEC` | 60 | `job list` / `job tail` 判定 `stalled` 的心跳超时（5–3600 秒） |
| `ATB_TABS_LARGE_WARNING` | 50 | `tabs` 超过该条数时附 `suggest`（10–500） |

### 下载

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_DOWNLOAD_AUTO_CLAIM` | `true` | `download-wait` 无 `--id` 时是否自动认领 |
| `ATB_DOWNLOAD_CLAIM_TTL_SEC` | 600 | 自动认领的「近期完成」窗口（30–900 秒） |
| `ATB_DOWNLOAD_REGISTRY_DEPTH` | 8 | 每个 agent 记住的下载 id 条数（1–64） |
| `ATB_DOWNLOAD_WAIT_LIMIT` | 200 | `download-wait` 每轮查询条数（1–500） |
| `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT` | 关 | 超时是否向浏览器发起取消 |
| `ATB_DOWNLOAD_CANCEL_ERASE` | 关 | 取消后是否擦除下载记录 |

### 安全与网络边界

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_WS_TRUST_MODE` | `compat` | WS 信任档位：`compat` / `whitelist` / `token` / `both` |
| `ATB_WS_OUTBOUND_MAX` | 512 | 扩展下行控制通道容量（8–65536） |
| `ATB_FILE_HOSTS` | 空 | 允许的 `file://host/...` 主机，逗号分隔（默认只允许 localhost） |
| `ATB_HTTP_NO_PROXY` | `true` | CLI 对回环地址跳过系统代理 |

### 网络与监控

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_NET_LOG_ITEM_CHARS` | 8192 | 单条 url/body/headers 字符上限（256–1M） |
| `ATB_NET_RULE_MAX_BYTES` | 2MiB | 单条规则 body 字节上限（1KiB–16MiB） |
| `ATB_NET_RULE_MAX_RULES` | 64 | 每 profile 规则条数上限（1–500） |
| `ATB_NET_MAX_ENTRIES` | 5000 | network 缓冲上限（100–100000） |
| `ATB_CONSOLE_MAX_ENTRIES` | 5000 | console 缓冲上限（100–100000） |
| `ATB_MONITOR_SAMPLES` | 50 | `--monitor` 保留的样本条数（1–500） |

### 启动与锁

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_LOCK_RETRIES` | 40 | 锁文件重试次数 |
| `ATB_LOCK_RETRY_MS` | 25 | 锁重试间隔（毫秒） |
| `ATB_PROC_CMD_TIMEOUT_MS` | 5000 | 进程枚举 / `tasklist` 类命令超时 |
| `ATB_LAUNCH_SETTLE_MS` | 800 | `launch` 后等待窗口稳定的时间 |
| `ATB_LAUNCH_CLOSE_WAIT_MS` | 3000 | 温和关闭浏览器的最长等待 |
| `ATB_LAUNCH_KILL_WAIT_MS` | 8000 | 强杀后的最长等待 |
| `ATB_LAUNCH_POLL_MS` | 500 | 启动期进程轮询间隔 |
| `ATB_BROWSER_EXE` | 自动探测 | 指定浏览器可执行文件路径 |

### 挑战与 HTML

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_HTML_KEEP_SCRIPTS` | 关 | HTML 优化保留 `<script>`（调试用） |
| `ATB_CHALLENGE_SRCDOC_MIN_AREA` | 2000 | srcdoc / about: iframe 纳入候选的最小面积（px²） |
| `ATB_CHALLENGE_FOCUS_ON_NEED_HUMAN` | `true` | 挑战需要人工时自动切前台 |
| `ATB_EXTENSION_ID` | 自动探测 | 多份扩展共存时用于识别自身 iframe |

### 空闲与并发

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_IDLE_TTL` | 300 | 无扩展连接时的空闲退出秒数 |
| `ATB_MAX_RUNNING_JOBS` | 32 | 同时运行的 job 上限，硬淘汰前先 abort |
| `ATB_REQUIRE_TAB` | 关 | 写类命令缺 `--tab` 时直接退出 2 |
| `ATB_FORCE_ASSUME_ATB` | `strict` | `restart --force` / `doctor --fix` 的身份判据；`loose` 放宽 |
| `ATB_ALLOW_RELATIVE_OUT` | 关 | `broadcast` 输出路径保留旧的相对路径行为 |
| `ATB_BROWSERS_SNAPSHOT_TTL_SEC` | 5 | `browsers` 快照短 TTL，`force_refresh` 可绕过 |

### 测试与 CI 专用（不面向日常使用）

| 变量 | 作用 |
| --- | --- |
| `ATB_E2E` / `ATB_E2E_BIN` / `ATB_E2E_AGENT` | 端到端脚本的场景选择、二进制路径与 agent 名 |
| `ATB_EXT_DIR` | 扩展目录覆盖（仅部署/测试脚本） |
| `ATB_PACKAGE_DIR` | npm 包目录覆盖 |
| `ATB_PLATFORM_OVERRIDE` | 强制平台 key（`npm/bin/atb.js` 选包用） |
| `ATB_SKIP_POSTINSTALL_CHECK` | 跳过 postinstall 的二进制探测 |
| `ATB_ALLOW_VERSION_MISMATCH` | 允许平台子包与主包版本不一致（仅告警） |
| `ATB_LOCK_ALLOW_MISSING_INTEGRITY` | 锁检查允许 `integrity` 缺失（发版窗口用） |

## 安全与隐私

- CLI、daemon、扩展之间的通信都在 `127.0.0.1`；无遥测、无远程代码、无广告 SDK。你操作的网页仍会按站点自身逻辑联网。
- `trust-extension` 配了 token 就是双向的：daemon 在 WS upgrade 后立刻下发 `ws_hello`（nonce + HMAC proof），扩展验过才发 `ext_ready`；没配 token 时（默认 `compat`）扩展无法验证对端身份。
- daemon 在没有扩展连接且空闲 300 秒后退出；只 `atb stop` 会释放 18765 而扩展仍会重连，要收口就同时禁用扩展或关闭浏览器。
- 扩展声明的权限：`cookies`、`tabs`、`tabGroups`、`activeTab`、`debugger`、`scripting`、`alarms`、`storage`、`declarativeNetRequest`、`webNavigation`、`downloads`（完整列表以 `assets/atb_bridge/manifest.json` 为准）。`management` 是**可选权限**：只有用户在扩展设置页「关于 → 高级权限」点击授权后才启用，用于 `atb extensions`；未授权时命令返回 `permission_required`，不会退回「只列本工具扩展」。页面数据只在你或 CLI 主动下命令时读取；`blockingExtensions` 只回报挡路扩展的 id / 来源，不再反查扩展名。
- WS 信任默认是 `compat`：未配 token 时任何本机进程都能连 18765。在 `ws-trust.json` 配上 token 后，握手即双向鉴权（`ws_hello` + HMAC proof），不依赖切档；`ATB_WS_TRUST_MODE` 的 `whitelist` / `token` / `both` 另控制「谁能连」，需显式切档才生效。共用机器请配白名单 + token 并按需设为 `both` 再 `atb restart`。`atb status` 的 `security.warning` 会提示当前档位。
- 幂等记录默认 `ATB_IDEMPOTENCY_PERSIST=redacted`，只存 key / scope / 时间 / 状态，不存响应体；`daemon.log` 为 0600、目录 0700，`ws-trust.json` 与 `api-token` 同等收紧。
- `challenge` 会主动与页面上的验证组件交互（点击并等待）。

报告漏洞见 [SECURITY.md](./SECURITY.md)，本地数据清单与删除方式见 [PRIVACY.md](./PRIVACY.md)。两者都只承诺代码能保证的事，其余写作「边界」。

## 平台与限制

支持：macOS arm64 / x64、Linux glibc x64 / arm64、Windows x64、WSL 2（用 Linux 包）、Chrome 与 Edge 120 及以上。

明确的事：

- 调试横幅是 Chrome 自己的 UI，运行时去不掉；唯一开关是启动参数，只能通过 `atb launch --restart` 重开浏览器生效。
- 不接管 Firefox / Safari，只支持 Chromium 系浏览器加扩展。
- 指纹伪装（改 UA / canvas / WebGL / 硬件值）尚未实现，列入未来工作；stealth 当前只清除本工具写入页面的内容。
- 能自动处理的是 JS 挑战和可点击的 Turnstile；图片点选、滑块、短信/邮箱验证尚未实现，列入未来工作。
- 后台标签页会暂停 `requestAnimationFrame` / `IntersectionObserver` 并节流定时器；懒加载、动画、可见性判断需要先 `atb focus`，命令响应里的 `tab_hidden` / `timers_throttled` 就是提示。
- 整页截图使用 CDP `captureBeyondViewport` 时，`position:fixed/sticky` 元素仍可能在底部重复；逐屏拼接则会在每屏重复。这是浏览器渲染限制，不是裁剪错误。
- 扩展桥断开时无法操作任何标签页。

## 文档地图

| 读者 | 文档 | 内容 |
| --- | --- | --- |
| 想让 AI 代装 | [AI_INSTALL.md](./AI_INSTALL.md) | 可复制的 prompt、分步流程、失败分支、卸载回滚 |
| 写 Agent 的人 | [skills/atb/SKILL.md](./skills/atb/SKILL.md) | 标准循环、按意图选命令、失败处理 |
| 查参数的人 | [commands.md](./skills/atb/references/commands.md) | 按域的命令与关键参数表 |
| 找配方的人 | [recipes.md](./skills/atb/references/recipes.md) | 可直接照抄的场景脚本 |
| 排障的人 | [troubleshooting.md](./skills/atb/references/troubleshooting.md) | 症状 → 判断 → 处理 |
| 深入用法 | [docs/BEST_PRACTICES.md](./docs/BEST_PRACTICES.md) | 选择器、等待、token 预算等实践 |
| 长任务 | [docs/ASYNC_BACKGROUND.md](./docs/ASYNC_BACKGROUND.md) | 异步 job、后台标签页与限制 |
| 检测与挑战 | [docs/ANTI_DETECTION_AND_CHALLENGE.md](./docs/ANTI_DETECTION_AND_CHALLENGE.md) | 痕迹审计、清理、挑战页处理 |
| Edge 加载失败 | [docs/EDGE_TROUBLESHOOTING.md](./docs/EDGE_TROUBLESHOOTING.md) | 扩展加载报错逐条排查 |
| 功能状态 | [docs/FEATURE_STATUS.md](./docs/FEATURE_STATUS.md) | 已实现 / 下一步 / 性能与边界 |
| 版本记录 | [CHANGELOG.md](./CHANGELOG.md) | 0.4.0 以来的变更摘要 |
| 历史记录 | [docs/CHANGELOG_ARCHIVE.md](./docs/CHANGELOG_ARCHIVE.md) | 0.3.7 及更早、重构前的逐条记录 |
| 参与开发 | [CONTRIBUTING.md](./CONTRIBUTING.md) | 分层、测试矩阵、不能破坏的契约 |
| 社区准则 | [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md) | 期望行为、举报渠道 |
| 安全 / 隐私 / 来源 | [SECURITY.md](./SECURITY.md) · [PRIVACY.md](./PRIVACY.md) · [SOURCES.md](./SOURCES.md) | 威胁模型、本地数据、上游与依赖 |

## 贡献

改代码前先读 [CONTRIBUTING.md](./CONTRIBUTING.md)：退出码、`error_code`、JSON 字段与两个协议都是对外契约，不能随手改。提交前跑 `cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`node --test tests/*.test.mjs`。

文档改动同 PR，中文与英文 README 必须同步，版本字符串由 `npm run sync-version` 统一写入。

## 致谢

浏览器控制能力提取并改造自 [GenericAgent](https://github.com/lsdefine/GenericAgent) 的 Web 工具链，包括 `TMWebDriver`、`simphtml` 与 `atb_bridge` 扩展的思路与实现。上游来源与依赖许可见 [SOURCES.md](./SOURCES.md)。

## 许可证

MIT License，Copyright (c) 2026 1728578755 / sleepinginsummer / lsdefine。见 [LICENSE](./LICENSE)。
