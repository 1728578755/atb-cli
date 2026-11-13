---
name: atb
description: 在用户已登录的真实 Chrome/Edge 里操作网页：读页面、点击填表、截图/PDF、Cookie、网络拦截、批量抓取、多标签并发、用户脚本注入与调试、多智能体隔离。当任务需要打开网页、抓取数据、填写后台表单、调试前端请求、注入/调试 Tampermonkey 兼容脚本、处理验证码等待页，或排查浏览器自动化失败时使用。
---

# atb

控制用户**已登录的真实 Chrome / Edge**，沿用现有 Cookie、Profile 和扩展。Rust daemon + Chrome MV3 扩展桥；不是 Selenium/Playwright，不启动一次性自动化 Profile。

**一次命令 = 一个进程。** 能并发的事放进一条命令，不要用 shell 循环起 N 条。

## 何时用 / 何时不用

用：

- 目标站点需要登录态、短信/扫码后的会话，或必须带用户自己的扩展与浏览器环境。
- 要同时处理多个标签页，或让多个智能体并行、互不抢页面。
- 要在真实页面里读 DOM、跑 JS、看网络请求、截图或存 PDF。

不用：

- 需要无头集群、容器内并发、可重复的干净环境：用 Playwright/Puppeteer。
- 只是解析本地 HTML/JSON：不需要浏览器。

## 三条硬规则

1. **批量放进一条命令。** `atb read --tab 111,222,333`、`atb exec --tab all '...'`、`atb scrape`。一条命令只建一次连接，daemon 内部并发；shell 循环把延迟乘以 N。
2. **改动页面之前先 `--dry-run`。** 只解析目标和校验参数，不发往浏览器。重点看 `targets`：多出来的标签页就是误伤风险。
3. **不要先跑 `status`。** 目标命令会自动拉起 daemon。只有命令失败，且输出指向连接、端口、扩展或「没有标签页」时，才看 `status` / `doctor` / `logs`。

## 标准循环

```bash
atb open https://example.com --background --wait-for 'selector:main'
atb read --max-chars 4000
atb snapshot --interactive-only
atb fill @e15 'hello'
atb click @e16
atb wait --text '提交成功' --timeout 10
atb read --selector '.result'
atb cleanup --dry-run
atb cleanup
```

| 阶段 | 做什么 | 不要做 |
| --- | --- | --- |
| open | 新标签成为本智能体的默认目标，可直接跟后续命令 | 不要假设用户当前标签就是目标 |
| 确认目标 | 先看响应顶层 `resolved_target` / `browser` / `profile`；`warnings` 有 `tab_hidden` / `timers_throttled` 就先 `atb focus` | 不要在后台标签上等懒加载 / 动画 |
| 读 | 先 `read` / `scan` 拿正文，再决定是否 `snapshot` | 不要一上来 dump 整页 HTML |
| 定位 | `snapshot --interactive-only` 拿 `@eN`；`find` 拿 selector；`measure` 量 rect / style | 不要写 `text=` / `xpath=` / `css=` 前缀 |
| 操作 | `click` / `fill` / `select` / `check` / `send-keys` | 点不动先看 `warnings`，再换 `mouse-click` |
| 验证 | `wait` 后再读一次结果区域 | 不要用固定 `sleep` |
| 收尾 | `cleanup` 只关自己 `open` 出来的 | 不要 `close --tab all`（不支持） |

### 用户脚本调试分支

页面脚本不生效、需要 `GM_*` 环境，或要复现 Tampermonkey 行为时，按这条链路走，不要直接 `exec` 一大段脚本：

```bash
atb open https://example.com/app --background
atb focus --tab @latest
atb userscript ./fix.user.js --parse-only          # 先看 @name / @match / @grant
atb inject --userscript --file ./fix.user.js --gm-namespace fix-demo \
  --cleanup-selector '#legacy-host'
atb exec 'return {host: !!document.querySelector("#fix-host"), flag: window.__FIX__ === true}'
atb screenshot --selector '#fix-host' --out fix-host.png
```

- `inject --userscript` 提供 `GM_getValue` / `GM_setValue` / `GM_deleteValue` / `GM_listValues` / `GM_registerMenuCommand` / `GM_log` / `GM_addStyle` / `GM_info` / `unsafeWindow`；`GM_xmlhttpRequest` 不支持，`@require` 不会自动加载。
- 看 `executed` / `returned` / `threw`：顶层 `ok:false` + `inject_script_threw` 表示脚本已经跑过但自己抛错，副作用可能已发生。
- `--persist` 默认 2 小时过期，永久驻留必须 `--persist-forever --i-know-this-is-long-lived`；`inject --list` 看 `expires_in_sec`，`inject --remove <name>` 清理。
- 页面已有安装版脚本时会重复宿主，先 `--cleanup-selector` 清旧节点，或让用户停用安装版。

## 选目标

`--tab` 是所有页面命令通用的目标选择器：

| 写法 | 含义 |
| --- | --- |
| `--tab 123456` | 单个 tab id |
| `--tab 111,222` | 多个 id，并发执行、一次返回 |
| `--tab all` | 当前浏览器/Profile 全部标签页 |
| `--tab @1,@3` / `@1-5` | 按 `tabs` 顺序（从 1 起） |
| `--tab @latest` | daemon 记录的最近打开 / 最近连接标签页（不是列表第 1 个） |
| `--tab url:example.com` | URL 子串，不区分大小写 |
| `--tab title:订单` | 标题子串 |
| `--tab group:research` | Chrome 标签组 |
| `--tab window:1234567` | Chrome 窗口 id |
| `--tab current` | 当前激活标签页 |

选择顺序：确定 id → `url:` / `title:` → `@N`（先 `tabs --ids` 看清单）→ `all`（破坏性操作不要用）。

- 结果按输入顺序返回；单条失败不影响其它，失败项各自带 `error_code`。
- 多目标写文件时，`--out` 必须含 `{tab}` 或 `{index}`，否则预检拒绝。
- `--async` 与 `all` / `@N` / `url:` / `title:` / `group:` / `window:` 互斥。
- `close --tab group:<标题>` 必须再加 `--yes` 显式确认。
- `atb use <target>` 把默认目标固定到某个标签；之后裸命令不再随 `open` 漂移。`@latest` 是「最近打开 / 连接」，`@1` 是 `tabs` 列表第 1 个，打错时先 `tabs --ids` 或 `tabs --format tsv` 核对。
- `atb focus --tab <id>` 把标签切前台；后台标签会暂停 rAF / IntersectionObserver 并节流定时器，懒加载、动画、截图前先看响应里的 `tab_hidden` / `timers_throttled`。

## 按意图选命令

| 目的 | 命令 | 备注 |
| --- | --- | --- |
| 读正文 | `read --max-chars 4000` | 默认 20000 字符，超长会截断 |
| 确认正在操作谁 | `env` / 看响应 `resolved_target` | `env` 一次给浏览器 / Profile / 扩展 / 标签 / 注入；后台标签带 `tab_hidden` |
| 把标签切前台 | `focus --tab @latest` | `--no-window` 不抢系统窗口焦点；懒加载 / 截图前用 |
| 固定默认目标 | `use <target>` | 之后裸命令都用它；`@latest` 是最近打开 / 连接 |
| 筛选标签页 | `tabs --url-contains <子串> --format tsv` | 还支持 `--title-contains` / `--filter` / `jsonl` |
| 大页面快速扫 | `scan --fast` | 非 fast 更完整，但更慢 |
| 找可点元素 | `snapshot --interactive-only` | 返回 `@eN` 引用 |
| 找元素位置 | `find 'a[href]' --limit 20` | 或 `find --text '登录'` |
| 元素量测 | `measure --selector 'host >>> .card'` | rect / computed style / visible / shadow_path；不支持 `@e` |
| 点击 | `click @e12` | 无效时 `mouse-click` 发真实鼠标事件 |
| 填表单 | `fill` / `select` / `check` / `upload` | 文件框用 `upload '<target>' ./a.pdf` |
| 等条件 | `wait --selector/--text/--js` | 默认 10s，页面内观察，不占进程 |
| 执行 JS | `exec 'return document.title'` | 封装命令覆盖不到时才用；`--file a.js --file b.js` 可拼接 |
| 注入 / 用户脚本 | `inject --userscript --file x.user.js` | GM shim 见「用户脚本调试分支」；`userscript <file> --parse-only` 只解析元数据 |
| iframe | `frames` → `--frame <id>` | `exec` / `inject` 的 `--all-frames` 可一次打全部 |
| shadow DOM | `snapshot` / `find '宿主 >>> 按钮'` | 递归 open shadow root；结果项带 `scope:"shadow"`。closed shadow root 不可达，回包用 `shadow_closed_unreachable` 明示 |
| 看 daemon 日志 | `logs --tail 100` | 输出 JSON：`{ok, lines[], path, truncated}`，按行消费用 `.lines[]` |
| 抓请求 | `network start` 起，`network list` / `detail` 查看 | 或 `net hook` / `net log` |
| console 历史 | `console history --types error,rejection` | 一次取回加载期报错；`console tail` 只轮询，不自动 `start` |
| 改响应 | `net mock <pattern> --body '...'` | `--engine cdp` 可拦静态资源与二进制 |
| 下载 | `download <url> --filename a.pdf` | `download-wait --id <id>` 等完成 |
| 截图 / PDF | `screenshot` / `save-pdf` | 等渲染用 `--wait-selector` / `--network-idle`；要确定性通道加 `--require-cdp` |
| 登录态 | `cookies` | 含 httpOnly，仅本地返回，不落盘 |
| 已装扩展 | `extensions` | 需用户先在扩展设置页「关于 → 高级权限」授权 `management` |
| 批量抓取 | `scrape --urls urls.txt --out out.jsonl` | 自动分批、重试、关页 |
| 多标签 | `--tab 111,222` | 一条命令并发，不要循环 |
| 后台任务 | `--async <cmd>` → `job tail <id>` | `job tail` 看 `elapsed_ms` / `stalled`；`job result <exec_id>` 取回超时结果 |
| 多智能体 | `--agent <name>` / `ATB_AGENT` | 收尾用 `cleanup` |
| 挑战页 | `challenge solve` | `open` 默认已自动尝试；失败用 `widget` + `click --at` 自己点，最后才 `focus` |
| 多浏览器 | `browsers` / `broadcast` | 先看有多少实例；`broadcast` 的 `--args` 里写路径会按工作目录绝对化 |
| 调试横幅 | `launch --dry-run` → `launch --restart` | 只在启动时生效 |
| 版本与能力门禁 | `capabilities` | 一次核对 daemon/扩展版本与 `core` / `open_session` / `group` / `window` / `current` / `challenge` 门禁 |
| 连不上先自修 | `doctor --fix`（加 `--force` 才杀进程） | 修旧 daemon 占端口、token 失配、扩展过旧；只诊断用 `doctor` |
| 查全部参数 | `schema --json` | 顶层命令的机器可读参数与 error_code；子命令用 `--help` |

## 输出契约

默认 compact JSON；`--pretty` 才缩进。成功返回 `ok:true` 与结果字段；失败返回：

```json
{"ok":false,"error":"...","error_code":"target_not_found","hint":"...","example":"...","retryable":true}
```

- `hint` 是下一步动作，`example` 可直接照抄；部分错误带 `retry_after_ms`。
- 退出码：`0` 成功 / `1` 执行失败 / `2` 命令写错 / `3` `job wait`、`job tail`、`job result --wait` 或 `download-wait` 超时 / `4` 结果未知 / `130` 取消。普通 `wait` 超时走 `1`。
- 退出码 `4` 表示请求已发出但响应丢失。动作类命令不要盲目重试：带同一个 `--idempotency-key` 重发，daemon 在 60 秒内回放首次响应（该记录会落盘，daemon 重启后同样有效；同 key 换 body 返回 409 `idempotency_key_reused`）。
- `status` 的 `ok` 跟随 `healthy`；需要旧的「恒 0」行为设 `ATB_EXIT0=1`。

## 失败处理

| error_code | 含义 | 下一步 |
| --- | --- | --- |
| `invalid_selector` / `invalid_js` / `syntax_error` | 括号或引号没配平，脚本本地语法检查失败 | 按 `hint` 的「第 N 行」改；确认误报用 `--no-preflight` 或 `ATB_JS_CHECK=off` |
| `unsupported_selector` | 选择器形态不支持（如 `measure` 传 `@e`） | `find` 拿真实 CSS；shadow DOM 用 `host >>> inner` |
| `focus_failed` | 标签切前台失败 | `tabs --ids` 确认标签仍在；扩展过旧就重载 |
| `permission_required` | `extensions` 未获 `management` | 扩展设置页「关于 → 高级权限」由用户点击授权 |
| `daemon_outdated` | daemon 还是旧二进制，不认识新路由 | `atb restart` 后重试 |
| `inject_script_threw` | 脚本已执行但自身抛错 | 看 `executed` / `threw`；副作用可能已发生，不要当注入通道失败 |
| `exec_result_not_found` | 超时 exec 的迟到结果不存在或已过期 | 确认 `exec_id`；加大 `ATB_EXEC_RESULT_TTL_SEC` 或改用 `--async` + `job wait` |
| `cdp_required` | 指定 `--require-cdp` 但 CDP 不可用 | 去掉 flag，或关闭挡住调试器的扩展 |
| `invalid_encoding` / `script_file_too_large` | 脚本文件非 UTF-8 / 超过上限 | 转 UTF-8 或 `--file -`；拆文件或提高 `ATB_SCRIPT_FILE_MAX_MB` |
| `invalid_target_spec` | `--tab` 语法错 | 对照上面的表；`window:` 后必须是数字 |
| `target_not_found` | tab id 不存在 | 报错里带可用列表；或 `tabs --ids` |
| `no_target_tabs` | 桥正常，但没有（过滤后）可操作标签页 | `open` 一个 `http/https` 页面，或放宽 `--tab` 过滤 |
| `extension_disconnected` | 扩展桥断开（浏览器未开 / SW 断开） | 等 3 秒重试；持续失败看 [troubleshooting.md](./references/troubleshooting.md) |
| `extension_outdated` | 扩展不支持该能力 | 覆盖 `assets/atb_bridge` 后重载扩展 |
| `element_not_found` | 选择器没命中 | `find` 确认元素存在、可见 |
| `csp_blocked` | 页面 CSP 禁止注入 | 改用 `read` / `snapshot` / `click` 等封装命令 |
| `timeout` | 操作超时 | 加 `--timeout`，或先 `wait` 再操作 |
| `wait_js_failed` | `--wait-js` 条件没满足（脚本可能已执行） | 按失败处理：看 `js_return.wait` / `matched`，加时间或改谓词，不要当成功继续 |
| `target_scope_empty` | `--tab all` / 多目标被 agent 归属过滤后为空 | 用 `--tab 123` 显式指定，或设 `ATB_MULTI_SCOPE=all` 跨 agent |
| `agent_no_default`（`target_lost` 的 reason） | `ATB_SESSION_ISOLATION=strict` 下具名 agent 没有自己的默认目标 | 先 `atb open <url>` 建默认；或改回 `legacy` |
| `download_id_required` | `ATB_DOWNLOAD_AUTO_CLAIM=false` 且 `download-wait` 没给 `--id` | 先 `downloads` 取 id，再 `download-wait --id <id>` |
| `outcome_unknown` | 请求已发出但响应丢失（超时/断连），动作可能已执行 | 退出码 4；带同一个 `--idempotency-key` 重发，不要换 key |
| `host_not_allowed` | HTTP `Host` 不是回环名 | 用 `127.0.0.1` / `localhost` 访问 daemon，不要用局域网 IP 或自定义域名 |
| `ws_backpressure` | 下行队列满（v10 只把 pong 控制通道改成有界，命令通道仍无界，因此该码目前是**预留**） | 偶发卡顿等几秒重试；持续出现重载扩展或 `atb restart`，高并发改用 `batch` 控并发 |
| `capture_too_large` | 截图/PDF 超出 `ATB_IO_MAX_OUTPUT_MB` 或 `ATB_CAPTURE_MAX_PIXELS` | 降 `--max-height` / 改 `--format jpeg`，或调高 `ATB_IO_MAX_OUTPUT_MB` |
| `pdf_too_tall` | 页面超出 PDF 单页/分页上限 | 用 `--max-height` 限高，或改 `screenshot --full-page` 留证 |
| `challenge_probe_failed` | 挑战探测拿不到可用载荷（扩展断连/注入失败） | 重试一次；持续失败看 `doctor`，不要当作「没有挑战」 |
| `invalid_port` | URL 的端口不合法（如 `:99999`） | 改用合法端口；`scheme:数字` 不会被猜成 http |
| `unsupported_file_host` | `file://host/...` 的主机不在允许列表 | 用 `file:///C:/...` 本机路径，或把主机加进 `ATB_FILE_HOSTS` |
| `file_not_found` | 本地文件 / `--urls` 列表不存在 | 核对路径；`--urls` 的「像路径但不存在」不再被当成 URL 列表 |
| `output_exists` | `ATB_IO_OVERWRITE=ask` 且目标已存在 | 换输出名，或设 `ATB_IO_OVERWRITE=force` / `backup` |
| `challenge_timeout` | 挑战页没在时限内放行 | 见 [recipes.md](./references/recipes.md) 的挑战页配方 |

完整错误码与排查见 [troubleshooting.md](./references/troubleshooting.md)。

## 多智能体

隔离依据是调用方身份，不是目标标签：

```bash
export ATB_AGENT=reviewer
atb open https://a.com
atb read
atb tabs --mine
atb cleanup
```

优先级：`--agent` → `ATB_AGENT`（旧名 `AGENT_BROWSER_CLI_AGENT`）→ 终端会话标识（`WT_SESSION` / `TMUX_PANE` / `VSCODE_IPC_HOOK_CLI` 等）→ `default`。

- `open` 只改**自己**的默认标签页，别的智能体的裸命令不受影响。
- `cleanup` 只关本智能体 `open` 的页面；`--all-agents` 才清全部。
- 想绝对确定就显式写 `--tab`，不依赖默认标签页。
- 验证隔离：两个身份各自 `open` 后，分别执行不带 `--tab` 的 `exec 'return location.href'`，应各自返回自己的 URL。

## 长任务与后台

超过 30 秒的操作不要阻塞会话：

```bash
atb --async exec 'return await fetch("/api").then(r => r.text())'
atb job list
atb job tail <jobId>                  # JSONL：elapsed_ms / last_heartbeat / stalled
atb job wait <jobId> --timeout 300
atb job get <jobId>
atb job result <exec_id> --wait 60    # 取回超时 exec 的迟到结果
atb job cancel <jobId>
```

- `--async` 返回 `job.id`；`job wait` 默认等 300s，超时返回 `job_wait_timeout`，任务不丢。
- `job tail` 每个状态变化输出一行 JSONL，`stalled:true` 表示心跳超过 `ATB_JOB_STALL_SEC` 没更新；终态退出码 `0` / `1` / `130`，跟踪超时是 `3`。
- exec 超时错误会带 `exec_id`；用 `job result <exec_id> --wait 60` 取回，迟到结果默认保留 3600 秒（`ATB_EXEC_RESULT_TTL_SEC`）。
- 支持异步：页面读取（`read` / `scan` / `find` / `snapshot` / `cookies` / `frames`）、动作（`click` / `mouse-click` / `fill` / `send-keys` / `hover` / `select` / `check` / `scroll` / `upload`）、产物（`screenshot` / `save-pdf`）、脚本（`exec` / `inject`），以及 `wait`。
- 不支持：`open` / `close` / `cleanup` / `tabs` / `focus` / `use` / `tabtree` / `browsers` / `lookup` / `run` / `batch` / `scrape` / `broadcast` / `net` / `network` / `console` / `downloads` / `download` / `download-wait` / `challenge` / `stealth` / `job` / `status` / `env` / `extensions` / `doctor` / `capabilities` / `logs` / `launch` / `restart` / `stop` / `daemon` / `set-extension-port` / `schema` / `profile-label` / `install-skill` / `trust-extension`，会报 `unsupported_flag`（退出码 2）。
- 结果只在 daemon 内存里，重启即失；要留产物就写文件（`--out`）。
- 大量 URL 用 `scrape`，不要用 `--async` 手工编排。

## 版本与扩展

三方版本：CLI/daemon API 协议 `5`、扩展消息协议 `4`、扩展文件版本（当前 **5.5.0**）。

```bash
atb status        # 只在排障时看 compatibility：ok / 期望协议 / 已连接协议 / fixes
atb capabilities  # 一次拿 daemon 版本、扩展版本与能力门禁（core/open_session/group/window/current/challenge）
atb restart       # CLI 与 daemon 不配套
atb restart --force   # daemon 占着 18767 且不认当前 token
```

- 扩展旧了：用仓库 `assets/atb_bridge` 覆盖扩展目录，再到 `chrome://extensions`（Edge 为 `edge://extensions`）点「重新加载」。**覆盖文件不等于重载**。
- `atb capabilities` 返回 `extension_not_connected` / `extension_outdated` / `daemon_not_running` 时按 `hint` 处理；它是判断「能力门禁够不够」的最短路径，比逐条试命令便宜。
- `atb stealth` 返回 `extension_ok:false`：扩展低于 5.0.0，或覆盖后没重载。
- 升级顺序：换扩展文件 → 重载扩展 → 升级 CLI，或先 `restart`。
- 主命令统一 `atb`；环境变量新前缀 `ATB_*`，旧前缀 `AGENT_BROWSER_CLI_*` 仍生效；配置目录仍是 `~/.atb`。

## 边界

- `challenge` 默认会主动求解（定位组件、带轨迹点击、等 token/跳转），不是干等；失败时智能体用 `widget` + `click --at` 兜底，图片/短信验证才 `focus` 转人工。
- 不 attach 的常用路径：`open`（未命中挑战页）、`read` / `scan` / `find` / `exec` / `wait` / `hover` / `select` / `check` / `scroll` / `inject` / `cookies`，以及带 CSS selector 的 `@e` 引用或 `--frame` 触发的 `click` / `fill`。
- 会 attach：`snapshot` 默认（要避开用 `snapshot --js`）、纯 CSS 选择器的 `click` / `fill`、`mouse-click`、`send-keys`、`upload`、`network` / `console`、整页/元素截图、`save-pdf`、`net mock --engine cdp`、挑战求解/点击、显式 CDP。设 `ATB_NO_DEBUGGER=1` 可彻底禁止 attach。
- attach 被拒（页面内嵌别的扩展 iframe，`error_code:"page_protected"`）时不要直接放弃：截图/PDF/鼠标点击会自动回落，结果里看 `via` / `synthetic` 判断走的是哪条通道（`captureVisibleTab` / `captureFullPageStitched` / `stitched-image-pdf` / `synthetic:true`）。只有 `network`、`console`、`upload`、`send-keys`、元素截图仍需调试器。需要确定性 CDP 时加 `--require-cdp`，失败会报 `cdp_required`。
- 整页截图用 CDP `captureBeyondViewport` 时 `position:fixed/sticky` 可能在底部重复，逐屏拼接则每屏重复；这是浏览器渲染限制。后台标签先 `focus`，否则懒加载 / rAF / 定时器会被节流。
- 调试横幅无法运行时去除，唯一开关是启动参数 `--silent-debugger-extension-api`，只能通过 `atb launch --restart` 生效。
- `trust-extension` 的 token 是双向的：daemon 在 WS upgrade 后立刻下发 `ws_hello`（nonce + HMAC proof），扩展配了 `wsToken` 时必须先验 proof 才发 `ext_ready`，不知道 token 的本机进程无法冒充 daemon；没配 token 时（默认 `compat`）任何本机连接都能连，扩展也无法证明对端身份。`ATB_WS_TRUST_MODE` 的 `whitelist` / `token` / `both` 控制「谁能连」，需显式切档；共用机器请同时配白名单与 token。
- daemon 默认空闲 300 秒且无扩展连接时退出，扩展连着时不退；退出后端口空窗期可能被本机其它进程占用，下一条命令会自动拉起。
- `chrome://`、扩展页、`about:blank` 上的可操作能力受限，先导航到普通 `http/https` 页面。

## 参考文件

| 文件 | 内容 |
| --- | --- |
| [commands.md](./references/commands.md) | 按域分组的命令、关键参数、返回要点 |
| [recipes.md](./references/recipes.md) | 15 个可直接照抄的场景配方 |
| [troubleshooting.md](./references/troubleshooting.md) | 症状 → 判断 → 处理 |
