# 能力现状：已实现 / 下一步 / 边界

更新时间：2026-11-13（扩展 5.5.0 ATB，CLI 0.8.0，CLI↔daemon 协议 5，扩展协议 4）

顶层命令 **61 个**（`atb --help`，不含 `help`；另有 `net` / `job` / `challenge` / `stealth` / `network` / `console` / `lookup` / `profile-label` / `trust-extension` 等子命令）。主命令名为 `atb`，npm 只提供 `atb` 一个 bin。

这份文档回答三个问题：现在能做什么、下一步做什么、什么做不到。

---

## 1. 已实现

### 0.8.0：目标身份、环境快照与长任务可观测性（2026-11-13）

| 能力 | 命令 / 字段 | 说明 |
|---|---|---|
| 切前台 | `atb focus [--tab <spec>] [--no-window]` | 调扩展 `focusTab`（`chrome.tabs.update`，可选窗口聚焦）；后台标签页的 rAF / IntersectionObserver / 定时器会被 Chrome 暂停或节流，先 focus 再测懒加载/动画 |
| 固定默认目标 | `atb use <spec>` | 把当前 agent 的默认目标固定到指定标签；命中多个标签直接 `target_ambiguous`（不会静默取第一个） |
| 最近目标 | `--tab @latest` | 最近打开/连接（`latest_session_key` → connected_at 最大 → 列表最后一个）；`@N` 仍是 `tabs` 列表第 N 个 |
| 单目标响应身份 | `resolved_target` / `browser` / `profile` / `warnings` / `hint` | 顶层只增字段；后台标签附 `tab_hidden` / `timers_throttled` 与 focus 提示（`ATB_HIDDEN_TAB_WARNING=off` 可关）；富化只覆盖白名单页面端点，`/open` `/multi` `/run` `/batch` `/scrape` `/broadcast` 不富化 |
| 浏览器横幅 | `ATB_BROWSER_BANNER=off/once/always` | CLI 在 stderr 打一行「→ 正在操作：…」，默认同一 agent+浏览器 10 分钟一次；状态存 `~/.atb/banner.json`（只存 agent/browser/时间） |
| 环境快照 | `atb env` | 纯 CLI 聚合 `/health` `/tabs` `/inject --list` `/capabilities`；输出浏览器/Profile/扩展版本/标签摘要/持久注入/警告，任何一块失败只降级成 `available:false + error` |
| 扩展列表 | `atb extensions [--include-disabled]` | 扩展 `listExtensions` → `chrome.management.getAll`；`management` 是 optional 权限，未授权返回 `permission_required`；只读，不启用/停用/卸载 |
| 元素量测 | `atb measure --selector` | rect / viewport / computed style / 可见性 / shadow 路径；支持 `host >>> inner`，不接受 `@e` / XPath |
| 用户脚本 | `atb userscript <file-or-url>` | 解析 `==UserScript==` 元数据并打开 Tampermonkey/Violentmonkey 安装页；`--parse-only` 只解析 |
| 用户脚本注入 | `inject --userscript --gm-namespace` | 最小 GM shim（GM_getValue/setValue/deleteValue/listValues/registerMenuCommand/log/addStyle/info/unsafeWindow）；`GM_xmlhttpRequest` 明确不支持；支持 `--cleanup-selector` / `--cleanup-file` |
| 持久注入安全阀 | `inject --persist` | 默认 7200s 过期（`--ttl` 可调，上限 30 天）；永久必须 `--persist-forever --i-know-this-is-long-lived`；`--list` 回 `expires_in_sec` / `warning`；旧 daemon 不传 TTL 字段时扩展保持旧行为「永久」 |
| inject 执行结果 | `inject` | `executed` / `returned` / `threw` / `duration_ms` / `cleanup`；脚本已执行后抛错顶层 `ok:false` + `inject_script_threw` |
| 本地脚本复用 | `exec --file a --file b` / `--file -` | 按顺序拼接或读 stdin；`--max-chars` 是 `--max-bytes` 的可见别名；`--no-preflight` / `ATB_JS_CHECK=off/warn/strict` |
| 截图等待与降级 | `screenshot --settle-ms/--wait-selector/--network-idle/--require-cdp` | 回包 `tab_visible` / `captured_tab` / `clip_box` / `truncated` / `degraded` / `warning` / `warnings`；整页 clip 用 `cssContentSize`（CSS px），修复 dpr>1 横向重复 |
| 等待新模式 | `wait --dom-stable[=ms]` / `--network-idle[=ms]` | 优先级 js > selector > text > dom-stable > network-idle，回 `mode_used`；`wait --js` 超时回最近 5 次求值 `last_values`；network-idle 只统计等待开始后的 fetch/XHR |
| tabs 过滤与流式输出 | `tabs --url-contains/--title-contains/--filter/--format json/jsonl/tsv` | 过滤在 daemon 侧执行；`tabs_total` / `filtered` 区分过滤前与过滤后；大列表附 `suggest` |
| console 历史 | `console history` / `console tail` | history 挂 CDP 监听后短采样回放加载期 console/error/unhandledrejection（`--settle-ms` / `--types` / `--clear` / `--limit`）；tail 轮询跟随输出 |
| job 进度 | `job tail <id>` / `job result <exec_id>` | tail 轮询 `/job/get/:id` 输出 `elapsed_ms` / `last_heartbeat` / `heartbeat_age_ms` / `stalled`（`ATB_JOB_STALL_SEC` 默认 60s）；result 取超时 exec 的迟到结果（TTL 3600s，`ATB_EXEC_RESULT_TTL_SEC`） |
| 特殊页面 | `open --wait-for none`；`ATB_OPEN_SPECIAL_WAIT_SEC` | `chrome://` / `chrome-extension://` / `about:` 等自动跳过等待，默认 3s 预算；`chrome-extension://` 可打开本工具自己的页面，其它扩展页面由 Chrome 拒绝 |
| 修复 | `--tab @1,@3` / `tabs --full` / `tabs --mine` / 整页截图 / 嵌套 SyntaxError / measure | `@1,@3` 逐 token 解析；flatten 查询 bool 走 `de_boolish`（旧实现 HTTP 400）；整页截图 CSS px；嵌套脚本错误剥壳映射 `invalid_js`；`measure` 成功不再套 `js_return` |

### 命令面扩展：schema / capabilities / doctor --fix / scrape（本轮新增）

| 能力 | 命令 | 说明 |
|---|---|---|
| 机器可读命令 schema | `atb schema --json` | 全部命令的参数（名/类型/默认值/可选值）与主要 `error_code`，结构 `{"commands":{"open":{"args":{...},"error_codes":[...]}}}`；静态表实现（`src/cli/cmd_schema.rs`），Agent 一次拿到用法 |
| 能力门禁一次探测 | `atb capabilities` | 返回 `{"ok", "extension_version", "daemon_version", "features":{core/open_session/group/window/current/challenge}, "thresholds"}`；阈值 CLI 侧写死：open_session≥2.1、group/window/current≥3.5、challenge≥5.0（与 daemon 各自版本校验口径一致）；daemon 不在时报 `daemon_not_running` |
| 诊断自动修复 | `atb doctor --fix [--force]` | 旧 daemon 占端口（netstat/lsof 找 PID；`--force` 才 taskkill，否则只打印命令）、API token 失配（删 `~/.atb/api-token`）、扩展过旧（打印带实际路径的重载指引）；每条输出 `{fix, executed, result}` |
| 批量抓取 | `atb scrape` | `--urls` 文件/逗号列表（去重）→ 分批后台开页（`--batch`，默认 4）→ 并发执行 JS（`--concurrency`，默认 4）→ 结果写 `--out`（JSONL：`{url, tab_id, ok, data, error, elapsed_ms, attempts, outcome_unknown}`）→ 批完关页 → 失败退避重试（3s/5s/7s，`--retry` 默认 3 轮）→ 摘要；默认每个 URL 只写一条最终记录（诊断「第几次才成功」用 `--record-attempts` 逐次写）；`outcome_unknown` 的 URL 不重试、最终退出码 4；`--batch/--concurrency` 超出 1..=32 退出 2；`--js-file` 缺省取 `{title,url,text(前9000字)}`；全部 CLI 侧编排，复用 open/exec/close，不改 daemon |

### 修复后的能力（CLI 0.8.0 / 扩展 5.5.0）

| 能力 | 命令/字段 | 说明 |
|---|---|---|
| open shadow DOM | `find` / `snapshot` / `read` | 递归收集 open shadow root，命中项带 `scope:"shadow"`、`pierce:true`、`host_selector`、`shadow_path`；选择器写成「宿主 `>>>` 相对路径」，`find` / `read` / `action` 都能再吃回这种写法。回包给出 `scope`（`main-frame-only` / `main-frame+shadow`）、`frame_count`、`shadow_roots`、`shadow_skipped`、`shadow_closed_unreachable`；`snapshot --engine js` 另给 `coverage_complete` / `coverage_hint` |
| 图片型 PDF 分页 + dpr 归一 | `save-pdf` | 整页长图按 14200pt 切片成多页（`pages` / `page_height_pt` / `width_pt` / `height_pt`），页面不再超过 PDF 14400pt 上限；设备像素先按 `dpr` 归一成 CSS px，HiDPI 下不再放大一倍。回包如实标 `paper:"image"`、`paper_applied:false`、`landscape:null`、`scale:null` |
| `--max-height` 全路径生效 | `screenshot --full-page` / `save-pdf` | CDP 主路径同样按 `max_height_px / dpr` 截断并回 `truncated:true`（此前只对拼图回落的显示通道生效） |
| PDF / 截图体积护栏 | `ATB_IO_MAX_OUTPUT_MB` / `ATB_CAPTURE_MAX_PIXELS` | 编码前按 base64 长度比对预算，超限返回结构化 `capture_too_large`；拼接按像素总量判断，默认 8000 万，不再 OOM |
| 下载名净化 | `download --filename` / daemon | 拒绝绝对路径、分隔符、`..`、空名、Windows 保留名、尾部点空格；取 basename，超 180 字符截断保留扩展名。回包 `filename_sanitized{requested,applied,changed,warnings}`；`--dir` 拒绝 `..` 段 |
| 下载取消与归属 | `download-wait` | `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT` / `ATB_DOWNLOAD_CANCEL_ERASE` 控制超时是否取消/擦除；`ATB_DOWNLOAD_AUTO_CLAIM=false` 且无 `--id` 时回 `download_id_required`；每 agent 记住最近 N 条（`ATB_DOWNLOAD_REGISTRY_DEPTH`，默认 8），回包 `claimed_from` |
| WS 四档信任 + 双向鉴权 | `ATB_WS_TRUST_MODE` | `compat`（默认）/ `whitelist` / `token` / `both`；非 compat 档 fail-closed（`ws_whitelist_not_configured` / `ws_untrusted_extension` / `ws_token_not_configured` / `ws_token_invalid`）。配 token 后 daemon 在 upgrade 后下发 `ws_hello`，扩展验证通过才发 `ext_ready`，失败记 `daemon_auth_failed`；`/health.ws_security`、`atb status.security` 与 `doctor.ws_trust` 透出 `mode` / `token_configured` / `mutual_auth` |
| cancel 帧 | `exec` 超时/取消 | daemon 向扩展发 `{cmd:"cancel", execId, tabId}`，扩展按 `execId` 释放这次 attach；被 network/console/Fetch 复用时不打断，回 `cancelled:false, reason:"in_use"` |
| 幂等落盘收敛 | `--idempotency-key` | 默认 `ATB_IDEMPOTENCY_PERSIST=redacted`，只写 `{key,scope,at,status}`；`full` 才写响应体（单条 256KB），`off` 不落盘。错误响应（4xx/`ok:false`）不再进缓存；`Pending` 占位有 Drop 守卫，取消/panic 不会让同 key 永久 504 |
| 网络/console 上限 | `network list` / `console list` | `limit` clamp 到 1–500 并回显 `clamped`；`networkStart` / `consoleStart` 下发 `max_entries`（默认 5000）与 `idempotent:true`，重复 start 不清空缓冲；单条日志 `url`/`body`/`headers` 超 `ATB_NET_LOG_ITEM_CHARS`（默认 8192）截断并标 `truncated` |
| 规则上限 | `net mock --engine cdp` | 单条 body 超 `ATB_NET_RULE_MAX_BYTES`（默认 2MiB）报 `net_rule_too_large`；每 profile 规则条数上限 `ATB_NET_RULE_MAX_RULES`（默认 64） |
| `logs` 输出 JSON | `atb logs` | 返回 `{ok, lines[], path, truncated}`；按行消费改用 `--tail` + `.lines[]`，不再逐行裸文本 |
| `capabilities` / `doctor --fix` | 运维 | `capabilities` 一次核对 daemon/扩展版本与能力门禁；`doctor --fix [--force]` 自动修复端口占用、token 失配、扩展过旧，`--force` 才杀进程 |
| `broadcast` 路径与 dry-run | `broadcast` | 走 `finalize_page_payload`：`out` 等路径绝对化 + 目标语法预检；`--dry-run` 在本地出计划，不拉 daemon；`ATB_ALLOW_RELATIVE_OUT=1` 保留旧的相对路径行为 |
| `tabs --limit 0` | `tabs` | `0` 表示**不截断**（返回全部），不是空列表；`tabs_count` 保持原始总数，另给 `returned_count` 与 `total_before_limit` |
| 组合命令结果语义 | `--wait-js` / `scrape` / `broadcast` | `wait.ok=false` 或 `matched=false` 时写 `wait_js_failed`，顶层 `ok:false` + 退出码 1；`scrape` 的传输层失败标 `outcome_unknown`（退出码 4）且不重试；`ATB_RESULT_STRICT=strict` 可让组合命令任一子结论失败即整体失败 |

### 反检测与挑战自动等待（0.6.0 新增，v10 修订）

| 能力 | 命令 | 说明 |
|---|---|---|
| 命令改名 | `atb` | Cargo `[[bin]]`、clap `name`/`bin_name` 均为 `atb`；npm `bin` 只有 `atb`，指向 `npm/bin/atb.js`；配置目录仍为 `~/.atb`（有意不改，避免新旧 daemon 各用一套目录与端口） |
| 环境变量新前缀 | `ATB_AGENT` / `ATB_EXIT0` / `ATB_IDLE_TTL` / `ATB_NO_DEBUGGER` / `ATB_BIN` / `ATB_HOME` / `ATB_WS_TRUST_MODE` 等 | 新名优先，旧前缀 `AGENT_BROWSER_CLI_*` 继续生效；完整的 65 个 `ATB_*`（56 个用户可见 + 9 个测试/CI）按主题分组列在 README「环境变量」章节 |
| 痕迹审计 | `stealth audit`（默认） | MAIN world 审计站点可读信号，输出 `score`(0-100) / `level`(clean/low/medium/high) / `headline` / `findings[]` / `env` / `self_fixable` / `extension_version` / `extension_ok` / `fixes` |
| 审计项 scope | `findings[].scope` | `self`=本工具自身（own_globals / own_dom_badge / own_net_hook）；`browser`=启动方式与窗口状态（webdriver / Headless UA / plugins 空 / 窗口 0 / SwiftShader / 时区空 / Notification 权限不一致 / **tab_hidden 后台标签**）；`unknown`=框架残留与被他人改写的 fetch/XHR |
| 自查清理 | `stealth scrub` | 只清本工具痕迹：先 `__abcNet({action:'reset'})` 还原 fetch/XHR，再删注入全局与角标节点，返回 `restored` / `removed` / `remaining`；不动 `scope:"browser"` |
| 扩展 5.x 隐蔽化 | 自动生效 | 页面操作脚本默认 ISOLATED world；只有 `challenge`（要读 `window.turnstile` / `_cf_chl_opt` / `__hcaptcha_cfg`）、`net`（要包装页面 fetch/XHR）、`stealth`（要在站点视角审计）走 MAIN world，`challenge` 入口 `__abcChallenge` 不可枚举且可删除。角标默认关闭 + closed Shadow DOM（宿主 id `atb-status-host`）；网络状态槽改为不可枚举 `window['__atb_net_<随机>']`，包装函数做 `toString()` 伪装与 `name`/`length` 对齐 |
| 打开即处理挑战 | `open --challenge auto\|wait\|off` | 默认 auto：标题判别（`Just a moment` / `Attention required` / `Checking your browser` / `请稍候` / `安全验证` 等，标题为空也探一次）→ 正常页面零额外往返；命中后**一律先跑一次元素驱动的主动求解**（不再拿「页面脚本说有没有可点组件」当门，因为组件是异步渲染进 closed shadow root 的），拿不到再退被动等待，默认 `--challenge-timeout 15`；拿到 token / 离开挑战页返回 `cleared:true`；需要人点时返回 `need_human` + `hints` |
| 判定只认可见证据 | `challenge status` | `blocking` 只由**看得见的拦截页**（标题 / `#challenge-stage` / 正文）判定；CF 的 `challenge-platform` 脚本与 `_cf_chl_opt`、各家 WAF 的 SDK 脚本在正常页面上也常驻，只记进 `signals`，不再据此判拦截（否则已放行的站点会被永久当成挑战页，`wait` 白等到超时） |
| 挑战页零全局污染 | 自动 | `challenge` 的页面脚本跑在 **MAIN world**（`PAGE_OP_WORLD.challenge = 'MAIN'`），因为判定必须读站点自己挂在页面 `window` 上的全局：`window.turnstile`、`window._cf_chl_opt`、hCaptcha 的 `__hcaptcha_cfg`。ISOLATED world 有独立 window，读这些全局恒为 `undefined`，会稳定误判「没有挑战」。代价是入口 `window.__abcChallenge` 对页面可见，因此必须**不可枚举 + 可配置**（`Object.defineProperty(..., {enumerable:false, configurable:true})`，）：`Object.keys(window)` 看不到，`delete` 也能成功，`stealth scrub` 不会永远报 `remaining`。旧实现用顶层 `function __abcChallenge(){}`，那是 classic script 的不可删除全局，且可枚举 |
| 组件在 closed shadow root 里 | `challenge status` 的 `turnstile` | CF 把 Turnstile iframe 挂在页面的 **closed shadow root**：页面 JS 的 `querySelectorAll('iframe')` 恒为 0、`#challenge-stage` 也不存在，但组件其实已经渲染。判据只用 DOM：「CF 挑战页 + 隐藏的 `cf-turnstile-response` 字段 + 无可见 iframe/stage」→ `turnstile.shadow_widget:true`、`interactive:true`，不再误报 `stalled: turnstile_not_rendered` |
| 硬拦截快速失败 | `challenge wait/solve/auto` | CF 的 `Sorry, you have been blocked`（error 1020/1015）点也点不了、等也不放行，识别到 `kind:"blocked"` 立刻返回 `error_code:"challenge_blocked"` 与处置建议，不再空耗 `--timeout`；`hard_block` 只在 CF 专属证据成立时为 true，普通 403 是 `blocked:true, hard_block:false`（另带 `plain_blocked` 诊断位），不再被误判成硬拦截 |
| 等待超时按失败 | `challenge wait` | `ok` 跟 `cleared` 走：超时 / `need_human` 回 `ok:false`（`error:"timeout"` / `error_code:"challenge_wait_timeout"`），不再是「`ok:true` + `cleared:false`」；daemon 仍按 `cleared` 判定，并保留 `passed` 语义 |
| 卡住时兜底 reload | `challenge wait --reload-after <秒>` | 默认 0 = 不 reload；挑战 cookie 可能已下发、页面脚本没走完时整页 reload 一次再继续等 |
| 标题误判收敛 | `challenge status` 的 `suspected` | 标题分两档：CF 专属文案（`Just a moment` / `Attention required` / `Verifying you are human` 等）直接判拦截；通用文案（`One more step` / `Access Denied`）只记 `suspected:true` + `signals:title-only-suspect`，必须等到 DOM/脚本实锤（`#challenge-stage` / orchestrate 脚本 / `#sec-if-cpt-container`）才升级，避免把 403 页当验证码页白等一轮 |

### 多浏览器身份、连接稳定与调试横幅（0.7.0 新增）

| 能力 | 命令/字段 | 说明 |
|---|---|---|
| 可读浏览器标签 | `browser_label` | 扩展上报 `品牌 · 系统 · 自定义名 · #短码`（如 `Edge · Windows · #5531`）；`tabs` / `browsers` / `lookup` 都带这个字段，多浏览器并行时不用再记 UUID |
| 按标签选浏览器 | `--browser Edge` | 支持完整 `browser_id`、id 前缀（≥8 字符）、标签子串（大小写不敏感） |
| 连接不抖 | 自动 | daemon 有空闲 TTL，但**只要还有扩展连着就不退出**；扩展侧 `onclose` 后按 0.3→0.6→1.2→2.5→5s 退避重连（alarms 只做长期兜底）。daemon 重启后 2~4s 恢复 |
| 调试横幅 | 自动 | `chrome.debugger` attach 的「已开始调试此浏览器」横幅只停留约 1.2s（`setTimeout` detach），不再挂满 30s |
| 横幅**彻底**不出现 | `atb launch --restart` | 横幅是 Chrome 自己的 UI，运行时无 API 可去；唯一开关是启动参数 `--silent-debugger-extension-api`（Chrome 156 仍支持，实测：带参数 attach 无横幅、不带参数有横幅）。`launch` 用这条参数重启浏览器；默认不动正在用的窗口，只有显式 `--restart` 才温和关闭（`taskkill` 不带 `/F`，等同点关闭按钮）并轮询确认退出；浏览器由 daemon 拉起，命令结束不会被回收 |
| 挑战提速 | `open --challenge auto` | 判定拦截后**无论前后台都切 active**（`allowFocus:false`）；不再自动 reload；求解器每轮先看元素（token / 是否离开挑战页 / 组件在不在）再决定动作，没有组件时很快返回，不白耗预算 |
| 挑战失败可诊断 | `reason` / `auto_solve.reason` / `env` / `turnstile` | 没放行时说明**为什么**：`no-widget-frame`（压根没出现组件）、`click-no-effect`（点了但页面毫无反应）、`verifying_not_completed`（点了、页面已进入「正在验证」但 CF 一直不给 token／不跳转）、`widget-removed-no-nav`（组件被摘掉但没跳转）、`turnstile_not_rendered` / `window_not_visible` / `timeout`。`auto_solve` 里带 `clicks` / `evidence` / `tried` / `diag` |
| 卡死提前收口 | 自动 | 只有「确实没有组件」才报 `turnstile_not_rendered`；组件藏在 closed shadow root 里时不再误判（见上一条）。真的没下发时 10s 收口，不耗满 `--challenge-timeout` |
| 智能体自己点（兜底） | `challenge widget` / `challenge click --at X,Y` | `widget` 返回组件的 `rect`/`url`/`click{x,y}`（含 local 坐标），`click --at` 按坐标发一次可信鼠标事件并立刻回 status。自动求解失效时不必转人工，三步自查自点：`widget` → `click --at` → `status` |
| 点一次就不再碰 | `challenge auto` | 组件出现即点（不预热）；iframe 消失 / src 换新 / 正文变「正在验证」= 点击被接收，立刻停手；3s 内毫无变化才算被吞、允许重点（上限 6 次）。旧实现固定偏移 (30,h/2) + 隔 5s 重点，把组件反复重置 |
| 别的扩展挡住 CDP | 自动 | 页面里有别的扩展注入的 `chrome-extension://` iframe 时，Chrome 拒绝 `chrome.debugger` attach（"Cannot access a chrome-extension:// URL of different extension"），定位／点击／截图全废。求解器识别这个错误后先摘掉这类 iframe 再重试 attach，`diag` 里记 `step:attach / stripped:<n>` |
| 窗口最小化修正 | 自动 | `--background` 或窗口被最小化时 `outerWidth=0` / `visibility=hidden`，CF 的 Turnstile 不会执行 → 等待期间发现环境不可见就先 `tabs switch`（不抢焦点）恢复窗口 |
| Cookie 清理 | `cookies --clear [--url <页面>] [--domain <域>]` | 按 url 清能删到分区 cookie（`cf_clearance`）；单站点 >200 条直接拒绝，防误删 |

### 页面脚本健壮性

| 问题 | 现在的行为 |
|---|---|
| CSS 路径不唯一导致点错元素 | `find` / `snapshot` 生成的选择器**逐层验唯一**（`querySelectorAll(path).length === 1`），不唯一就继续加祖先到 `documentElement`；`snapshot @e37` 不会再落到第 1 张卡片的同位按钮上 |
| 口令进入命令行/上下文 | `read` / `find` / `snapshot` / `scan` 对 `type=password` 与 `autocomplete=current-password/new-password/cc-number/cc-csc/one-time-code` 一律返回 `•••` + `redacted:true`，明文不出页面 |
| 点击「假成功」 | 目标中心点被遮罩盖住时（cookie 弹窗 / 登录浮层 / 全屏 click-catcher），`click` / `mouse-click` 结果带 `blocked:true` + `covered_by`，顶层另有 `warnings:[{code:"action_target_covered"}]`；事件仍派发到目标，但调用方能看见「真实鼠标点不到」 |
| React 受控 checkbox 静默失效 | `check` / `uncheck` 改走**原生 `click()`**（React 的 onChange 对 checkbox 挂的是 click），返回 `via:"click"`；已等于目标状态时 `unchanged:true`，disabled 或页面回滚时报 `ok:false` |
| `--capture-bodies` 挂死页面 | body 读取改成旁路（日志先落 `body_capture_pending`，preview 后补）；`text/event-stream` 与 >4MB 响应直接标 `body_capture_skipped`，页面自己的 fetch 不再被拖住 |
| 响应改写丢字段 / 整段缓冲 | 只改 status/headers 时**透传 body 流**（`body_stream_passthrough`）并补回 `url`/`type`/`redirected`（补不上标 `url_lost`）；目标状态码是 204/304 时显式置空 body（`body_dropped:"null_body_status"`），不会因 `new Response(stream,{status:204})` 抛错 |
| mock 构造异常打断页面 | 非法 status / 204 带 body 不再把 `TypeError` 抛给页面：记 `mock_invalid` 并放行真实请求；XHR 的 abort 现在置 `readyState=4`、`status=0` 并派发 `ProgressEvent`，复用同一个 xhr 时清掉上一次的 `readyState` 补丁 |
| 规则匹配口径不一致 | 相对 URL 先解析成绝对 URL 再匹配（页内 `fetch('/api/x')` 能命中 `match:"host/api/x"`）；默认**子串**匹配，`regex:true` 才按正则（`app.js` 不再命中 `appXjs`）；`net route` 更新同 (match,method) 规则改为**原位替换**，不改变命中优先级；重放超 50 条时返回 `routes_dropped` |
| `net fetch` 自我欺骗 | 诊断请求改用 hook 之前的原始 `fetch`，命中不了自己写的 mock，返回体标 `bypassed_hooks:true` |
| scan 的浮层剔除失效 | `nodeInfo` WeakMap 现在真的写入（此前从未 `set`，整条 `R:covered`/`R:floatingAd` 链路是死代码）；固定定位的图片广告会被剔出 scan 结果 |
| 超深 DOM 让 scan 崩掉 | 页面侧克隆与分析加了 512 层上限（与 `html.rs` 的 `MAX_DOM_DEPTH` 同阈值），超限返回 `…truncated: DOM deeper than 512 levels`；其它异常统一成 `__ABC_SCAN_ERROR__<msg>` 由 daemon 转结构化错误 |
| 页面污染全局函数 | 扩展每次调用都重新注入页面脚本（不再「探测到同名函数就跳过」），并对 MAIN world 的 `__abcNet` / `__abcStealth` / `__abcChallenge` 校验不可枚举标记 `__abcOwn`（三者都是 `requireOwn:true`）；被页面抢占时明确报错，不执行页面提供的实现 |
| 调试日志污染页面 | `simphtml_opt` 的 `console.log`（含遗留的 `area/totalArea` 调试语句）默认全关，需要时传 `{debug:true}`；文件末尾多余的 `optHTML()` 裸调用已删，注入不再白白整页克隆一遍 |
| `--interval` 对 `wait --js` 无效 | daemon 传的键名由 `intervalMs` 改为 `minIntervalMs`，`wait --js --interval 5` 实测 11s 内只巡检 3 次（此前被静默按 50ms 巡检） |
| 列表容器被改 id | `find_list` 不再给页面容器写 `id='_ljq..'`，改用不落地 DOM 的 nth-of-type 路径 |

### 多智能体 / 版本配套 / 多窗口（0.5.0 新增）

| 能力 | 命令 | 说明 |
|---|---|---|
| 身份隔离 | `--agent <名字>` / `ATB_AGENT`（旧名 `AGENT_BROWSER_CLI_AGENT`） | 每个身份独立默认标签页，互不抢目标 |
| 收尾清理 | `cleanup [--dry-run] [--all-agents]` | 只关本工具 `open` 过的标签页，不按 URL 猜 |
| 多窗口 | `--tab window:<id>` | `tabs --ids` 带 `window` 字段；需扩展 4.0+ |
| 版本配套 | `status` 的 `compatibility` | CLI/daemon/扩展三方协议比对 + `fixes` |
| 能力门禁 | `extension_outdated` | window/group/current 在旧扩展上直接报版本错 |
| 非接触截图 | `screenshot`（可见区域） | 走 `captureVisibleTab`，不附加调试器 |
| 禁用调试器 | `ATB_NO_DEBUGGER=1`（旧名 `AGENT_BROWSER_CLI_NO_DEBUGGER`） | CDP 命令返回替代方案 |

### 多目标并发与命令预检（0.4.0 新增）

| 能力 | 命令 | 说明 |
|---|---|---|
| 多标签并发 | `--tab all / a,b / @1-5 / url: / title: / group:` | 一次请求内并发（默认 8 路），单条失败不影响其它；结果逐条带 `error_code` + `hint` |
| 精简目标列表 | `tabs --ids` | 只回 `tab_id/title/url/active/group`，选目标不再翻长 JSON |
| 预检 | 自动执行 | 选择器/JS 括号与引号配平、`--tab` 语法、URL/文件、数值范围、输出路径冲突、`--async` 组合 |
| 只检查不执行 | `<命令> --dry-run` | 输出解析后的目标与计划；不产生页面副作用 |
| 退出码 | — | `0` 成功 / `1` 执行失败 / `2` 命令写错 / `3` 等待超时（`job wait` / `job result` / `download-wait`）/ `4` 结果未知（动作可能已执行，带同一 `--idempotency-key` 重发）/ `130` 取消 |
| 输出占位符 | `screenshot/save-pdf --out 'shots/{index}-{tab}.png'` | 多目标写同一文件会在预检阶段被拒绝 |
| 长图高度上限 | `screenshot --full-page --max-height N` / `save-pdf --max-height N` | CDP 主路径与拼图回落通道都生效：单位是**设备像素**（与 `scrollH × dpr` 比较），默认 20000，超出按上限截断并标 `truncated:true`（截断时附 `requested_height` 与 hint） |

### 会话与标签

| 能力 | 命令 | 说明 |
|---|---|---|
| 列出标签 | `tabs [--full] [--browser] [--profile]` | 默认截断 URL，`--full` 给完整 URL |
| 标签树 | `tabtree` | 按 browser/profile 分组，省略 session_key 省 token |
| 定位单个标签 | `lookup tab/browser/profile` | 精确查询 |
| 打开 | `open <url...> [--background] [--focus] [--group-title]` | **多 URL 由 daemon 内部并发打开（单请求）**，逐条返回 `tab_id/ready/elapsed_ms` |
| 关闭 | `close --tab <id>[,<id>...]` | 支持批量；不接受 `all`，避免误关用户标签页 |
| 多目标 | `--tab all \| a,b \| @1-3 \| url:xx \| title:xx \| group:xx \| current` | 所有页面命令通用；并发执行、按输入顺序汇总 |
| 默认路由 | 任意命令省略 `--tab` | 命中最近一次 `open` 的标签；并发任务用显式 `--tab` |

### 页面读取

| 能力 | 命令 | 返回 |
|---|---|---|
| 结构化读取 | `read [--selector] [--max-chars] [--links] [--no-forms] [--include-hidden] [--budget-ms N] [--max-nodes N] [--frame N]` | 标题/URL/正文/元信息/标题层级/各类计数/表单结构；单遍遍历 + 时间预算，超大页面不卡 |
| 隐藏文本 | `read --include-hidden` | 走 `textContent` 语义，把 `display:none` / `hidden` / `aria-hidden` / CSS 隐藏的内容也返回；默认仍只返回可见文本（`text_mode` 标明 `visible` / `visible-approx` / `all`） |
| 元素查找 | `find <selector>` / `find --text <文本>` / `find --xpath <expr>` | 命中数 + 每个元素的可操作信息（tag/text/value/href/rect/disabled/checked/selector）；`selector` 为命中元素的 CSS 选择器（`find` 命中多个时全部返回，动作类命令取第一个）；口令/支付类字段的 `value` 返回 `•••` 并标 `redacted:true` |
| 页面扫描 | `scan [--tabs-only] [--text-only] [--fast] [--max-nodes N]` | 精简 HTML、纯文本或快速文本；超大页面（>6 万元素）自动改走文本并注明；固定浮层广告/被遮挡层会被剔除，超 512 层嵌套截断并注明 |
| 页面网络 | `net fetch <url> [--method] [-H k:v] [--body] [--response-type]` | 在页面上下文发请求（带 cookie / 同源策略 / 页面 CSP 视角），返回 status/headers/body/json |
| 网络 hook | `net hook [--capture-bodies] [--persist] [--match <pattern>]` / `net log` / `net reset` | 包装 `fetch` 与 `XMLHttpRequest`，记录 URL/方法/状态/耗时；`--persist` 把「库 + 规则」注册成 document_start 注入，**刷新/跳转后自动重装** |
| 网络 mock | `net mock <片段\|正则> [--status N] [--body JSON] [--method GET] [--abort]` / `net routes` / `net unroute` | 命中规则的 `fetch`/XHR 直接返回伪造响应（或网络错误），不真的发出去；规则按 **profile 共享**，同 profile 的新标签自动继承。默认按子串匹配、相对 URL 先解析成绝对 URL（`fetch('/api/x')` 也能命中 `match:"host/api/x"`），`regex:true` 才按正则；同 (match,method) 规则原位替换以保持优先级 |
| 网络层拦截 | `net mock <pattern> --engine cdp [--response-status N] [--response-body B] [-H 'k: v']` | 走扩展 `chrome.debugger` 的 `Fetch` 域，能改 **blob / 二进制体**、也能拦 `<img>` / `<script>` / JSONP；不换 body 时用 `continueResponse` 原地放行；CDP 被占用时明确报错且不落库。干净 profile 真机 15 条断言通过（`tests/live_cdp_net.mjs`） |
| 挑战页/验证码 | `challenge status` / `wait` / `solve` / `auto` / `widget` / `click` / `focus`；`open --challenge` 默认 auto | 识别 CF/Cloudflare Turnstile/reCAPTCHA/hCaptcha/PerimeterX/Akamai/DataDome/AWS WAF 与通用验证码，滑块/点选类（阿里云 noCaptcha、极验、腾讯防水墙、顶象、数美）单独标 `kind:"slider"`；`open --challenge auto` 命中后先主动求解（元素驱动、带轨迹点击）再等 token/跳转；`challenge solve` 手动跑同一路径，`auto` 只求解一次，`wait` 只等待。失败时 `widget` 拿坐标、`click --at` 由智能体自己点，`focus` 才是转人工；`--reload-after <秒>` 卡住时整页 reload 一次再等。`open` 命中挑战页时结果里带 `challenge` 字段 |
| 请求改写 | `net mock <pattern> --request-header 'x-a: 1' --request-body JSON` | 放行真实请求，但按规则改请求头 / 请求体（GET/HEAD 不写 body）；日志标记 `request_rewritten` |
| 响应改写 | `net mock <pattern> --response-status 201 --response-body JSON [-H 'x-a: 1']` | 放行真实请求后替换状态码 / 响应体 / 响应头；**fetch 与 XHR 都可靠**（XHR 走访问器接管，与页面监听器注册顺序无关），支持 `text` / `json` / `arraybuffer` / `document` / `blob`（blob 需规则显式给 body） |
| 结果限流 | `exec --max-bytes N` | 返回值超限时给截断预览（`truncated:true`），避免一条命令塞满上下文 |
| 脚本注入 | `inject <code> [--file] [--world main\|isolated] [--frame N] [--all-frames]` | 一次性注入，语义同 `exec` |
| 持久注入 | `inject --persist <name> [--match <pattern>] <code>` + `--list` / `--remove` | 存代码；注册 `persist_boot.js` 在 **document_start** 生效，注册失败时退回导航后重放 |
| iframe 列表 | `frames [--tab]` | 返回 frame_id / parent_frame_id / url，配合 `--frame` 使用 |
| 快照 | `snapshot [--js] [--interactive-only] [--max-nodes N] [--details]` | 可访问性树 + `@e` 引用；CDP 不可用时**自动回退 DOM 快照**（`mode:"js"`）；`--interactive-only` 只留可操作元素，`--max-nodes` 截断大树 |
| 引用重定位 | 任意 `@e` 操作 | 页面导航 / SPA 换页后，`@e` 会按 role+name 在新页面**自动重定位**并返回 `ref_relocated:true` + `ref_selector`；确实找不到才报 `ref_expired` |
| Cookie | `cookies [--url]` | |
| 网络/控制台 | `network *` / `console *` | 需要 CDP |

### 页面操作

| 能力 | 命令 | 实现路径 |
|---|---|---|
| 点击 | `click <target> [--frame N]` | CDP `DOM.resolveNode` + 真实事件；`@e` 来自 JS 快照或指定 `--frame` 时走页面内点击；命中点被遮罩盖住时顶层返回 `warnings:[{code:"action_target_covered", covered_by}]` |
| 鼠标点击 | `mouse-click <target>` | CDP `Input.dispatchMouseEvent`；同样带遮挡告警（此时点击真的落在遮罩上，页面可能收不到） |
| 填写 | `fill <target> <value> [--clear] [--append] [--frame N]` | CDP；`@e` 来自 JS 快照或指定 `--frame` 时走页面内 setValue |
| 键盘 | `send-keys <keys> [--target] [--frame N]` | CDP（frame 只影响 focus 目标） |
| 悬停 | `hover <target>` | 页面内单往返（pointer/mouse 六事件） |
| 下拉选择 | `select <target> --value V` / `--label L` | 页面内单往返，触发 `input`+`change` |
| 勾选 | `check <target>` / `uncheck` | 页面内单往返，走**原生 click**（React 受控组件能收到 onChange），返回 `via:"click"` / `unchanged:true` / 失败原因 |
| 滚动 | `scroll [<target>] [--dy N] [--to top\|bottom]` | 页面内单往返 |
| 等待 | `wait [--selector S] [--text T] [--js C] [--visible] [--frame N]` | 全部**一次注入**完成：`--selector`/`--text` 走页面内 MutationObserver（`mode:"observer"`）；`--js` 谓词在页面内编译一次、DOM 变化由观察器触发（`mode:"js-observer"`，`injections` 通常为 1） |
| 前进/刷新 | `back` / `reload` | 先返回再触发导航，避免白等超时 |
| 截图 / PDF | `screenshot` / `save-pdf` | 优先 CDP；被拒时自动回落（可见区域 `captureVisibleTab`、整页拼图、图片型 PDF），返回 `via` 标出通道 |
| 上传 | `upload <target> <files...>` | 需要 CDP |

所有 `target` 支持 CSS 选择器，`hover/select/check/scroll` 还支持 `text=可见文本` 与 `--index`。

### 批量、异步与并发

| 能力 | 命令 | 语义 |
|---|---|---|
| 串行管道 | `run <steps.json>` | 一次进程多步，默认遇错即停（适合有依赖的流程） |
| 并发批量 | `batch <steps.json> --concurrency N` | 一次请求内最多 N 个并发（适合跨标签批量读取/操作） |
| 异步任务 | `--async <cmd>` + `job list/get/wait/cancel` | 立即返回 job_id，daemon 内继续执行 |
| 单次分发 | `open a b c` | 客户端并发打开多个标签 |
| 浏览器清单 | `browsers` | 列出所有 (browser_id, profile) 与代表标签，多浏览器编排前先看这里；`count` 是**浏览器数**，`profile_count` 是 (browser, profile) 对数；扩展刚连上、快照未到时返回 `warning_code:"snapshot_pending"` + `retryable`，不是「没有浏览器」 |
| 跨浏览器广播 | `broadcast <op> [--script S\|--args JSON] [--browser B] [--profile P] [--concurrency N]` | 一次在**所有**匹配的 browser/profile 上并发执行同一操作，逐目标返回结果与耗时 |
| 下载监听 | `downloads [--limit] [--state] [--id]` / `download <url> [--filename] [--dir]` / `download-wait [--id] [--timeout]` | 扩展 `chrome.downloads`，list/start/wait；wait 在 daemon 侧轮询 |

同一标签的命令在 daemon 内**串行排队**（每会话一把异步锁，仅「注入」这一步持锁；等回包 / `wait --js` 轮询期间主动放锁，不空占锁），不同标签各自并发；这样同一标签 50+ 并发不会互相踩踏，也不会拖慢其它标签。

`batch` 的可用 op：`exec / read / find / action / wait / scan / snapshot / click / fill / mouse-click / send-keys / upload / screenshot / save-pdf / cookies`。

### 无 eval 通道（严格 CSP 页面）

`chrome.scripting.executeScript` 注入函数本身不受页面 CSP 限制，但函数内部 `eval` 会。于是给固定操作准备了一条不 eval 的路径：扩展注入 `ops/page_*.js`（与 daemon 侧同源），再调用其全局函数。

> **5.0.0 起**：这条通道默认注入 **ISOLATED world**，`window.__abcRead` 之类的全局只存在于扩展的隔离世界，站点读不到。只有 `challenge`（判定要读 `window.turnstile` / `_cf_chl_opt` / `__hcaptcha_cfg`）、`net`（要包装页面 `fetch`）和 `stealth`（要在页面视角审计）走 MAIN world。

> **2026-11-01 更新**：`ops/` 与 `assets/` 两处副本不再靠手工同步——`npm run sync:extension` 生成/更新副本，`npm run check:extension` 在 CI 里守卫一致性。改过 `assets/page_*.js` 后必须跑一次 sync，否则扩展实际注入的还是旧副本。

| 能力 | 走法 | 说明 |
|---|---|---|
| `read` / `find` / `snapshot --js` / `action`（hover/select/check/scroll/click/fill） / `wait` | 先 eval 快路径；命中 CSP 错误时自动切 `pageOp` | 返回里带 `via:"no-eval"`（snapshot 仍是 `mode:"js"`） |
| 任意 `exec` / 页面变量读取 | 不可用 | 需要 eval，CSP 页面无解；错误会明确说原因 |

实测 GitHub 首页（CSP 严格 + debugger 被其他扩展 iframe 挡住）：`read` 返回 269 字正文、`find` 命中 19 个链接、`snapshot --js` 给出 5 个 `@e`、`hover`/`scroll`/`wait` 全部可用。

### 扩展自更新与诊断

- `status`：`extension.stale` / `warning` 直说浏览器里跑的是不是最新扩展。
- `stealth`：`extension_ok` 判定扩展是否 ≥ 5.0.0，false 时附 `fixes`（覆盖扩展目录 + 重新加载）。
- `doctor`：`extension_fresh` 等检查项。
- daemon 发现扩展版本落后会主动发 `management.reload`（同版本只发一次）。
- 扩展每 60 秒比对 `background.js` 哈希，磁盘有改动且空闲时自我重载。
- `status` / `tabs` / `browsers` 的 `browser_label` 带品牌与完整版本（`Chrome 156.0.8078.79 · Windows · #cac3`），多浏览器并行时可直接区分。
- popup 有「连接状态 / 数据 / 设置」三段，右上角进入管理页面（`options.html`）：端口、token、信任档位、主题、弹窗宽度、间距、角标开关、Cookie 与诊断；「运行版本」与「重新加载扩展」在设置段里。
- 诊断命令：`injectProbe`（三种注入路径）、`selfCheck`（哈希基线/定时器状态）。

---

## 2. 性能数据

环境：Windows + Chrome 156，本地 daemon，真实浏览器，页面为 `example.com`。
测量工具：`node tests/concurrency_bench.mjs --tabs N --rounds M`。

| 场景 | 实测（多次运行区间） |
|---|---|
| `open` 12 个标签（并发） | 0.37 ~ 1.35 s（首次加载 vs 命中缓存） |
| 单标签一次 `exec`（含 CLI 冷启动） | 89 ~ 254 ms（冷启动 daemon 占大头） |
| 60 步（12 标签 × 5 轮，read 4KB + exec 15ms 忙等）并行 `batch(concurrency=12)` | **82 ms**，失败 0 |
| 同样 60 步串行 `run` | 542 ms（各步合计） |
| 加速比 | **6.6x** |
| 60 步纯 `exec 'return 1'`（轻负载）并行 / 串行 | 6 ms / 8 ms —— 轻负载已到噪声底，加速比无意义 |
| 每步平均（并行，warm） | 2 ~ 4 ms |
| 同一标签 12 条并发命令 | 38 ~ 48 ms，失败 0 |
| `read` 单次（warm daemon） | 7 ~ 115 ms，取决于正文大小 |
| `exec --monitor` 额外开销（改观察器后） | 约 1 ms（旧实现两次整页序列化，抓取成本随页面大小线性增长） |
| `wait --selector`（800ms 后出现） | **856 ms 命中，2 次检查**（旧实现约 9 次注入） |
| `wait --selector`（已存在） | 20 ms 命中，1 次检查 |
| `wait --selector` 超时（2s） | 2585 ms 返回 `matched:false`，1 次注入（不再每 100ms 注入） |
| `wait --js`（条件已成立） | **5 ms 命中，1 次注入，1 次求值** |
| `wait --js`（DOM 800ms 后出现） | **673 ms 命中，1 次注入**（旧实现约 8~9 次注入） |
| `wait --js`（定时器条件 800ms） | 722 ms 命中，1 次注入，15 次求值 |
| `wait --js` 超时（0.8s） | 868 ms 返回 `matched:false`，1 次注入 |
| `wait --js` 跨导航 | 2 次注入后命中（旧页面销毁后按剩余时间自动重试） |
| `exec --wait-js`（条件已成立） | 0.4 ms 页面内命中，1 次注入 |

超大页面（`tests/gen_huge.mjs`，26 万元素 / 6 MB HTML，同一标签连续测量）：

| 场景 | 改动前 | 现在 |
|---|---|---|
| `read`（默认，可见文本） | `innerText` 单次约 200 ms，再叠加 5~6 次全量 `querySelectorAll` | **270 ~ 300 ms**（单遍遍历，页面内 65 ~ 140 ms） |
| `scan --text-only` | 4.6 ~ 4.9 s（`optHTML` 逐节点 `getComputedStyle`） | **270 ~ 310 ms** |
| `scan`（HTML） | 首次超过 230 s 仍未返回（整页布局 + 序列化） | **265 ~ 345 ms**（自动改走文本，内容头部注明原因） |
| `scan --fast` | — | **285 ~ 350 ms** |
| `exec 'return 1'` | — | 190 ~ 220 ms（warm） |

机制：`read`/`scan` 不再做多次全量查询；`optHTML` 有节点预算与布局跳过；页面元素数 > 6 万时 `scan` 直接返回文本而不是序列化整棵 DOM。元素数按会话缓存 2 分钟，避免重复探测。

几点说明：

- 串口瓶颈不在页面执行，而在「每条命令一次 CLI 冷启动 + 一次 HTTP」。所以**批量场景必须用 `batch`/`run`**，不要用 shell 循环调 CLI。
- 并发收益取决于单步是否有实质工作：`read`/`find`/页面脚本这类每步几毫秒以上的任务能到 6x；`exec 'return 1'` 这种 1~2 ms 的步骤会被固定开销淹没，此时并行与串行差别在噪声范围内。基准脚本会先预热每个标签，并对并行取两轮较小值。
- 扩展侧对同一标签的命令是并发的（`onmessage` 不串行 await），12 条并发仍 47 ms。
- 隐藏标签页的 `setTimeout` 会被 Chrome 节流，所以等待都放进页面里由 MutationObserver 驱动；`wait --js` 的非 DOM 条件再由 `setInterval` 兜底（后台会被节流到 1s，DOM 类条件不受影响）。

---

## 3. 下一步（按投入产出排序）

| 优先级 | 事项 | 价值 |
|---|---|---|
| P2 | `sessions` map 从 `DriverState` 搬出去 | 已把 `driver` 换成 RwLock（只读路径并发）；`sessions` 仍和默认会话选择共用一把写锁，继续拆要重排 26 处锁顺序 |
| P2 | `net` 规则的跨会话共享 | 现在持久化配置按标签（session）记录，换标签要重新 `net hook --persist` |
| P2 | `snapshot` 增量游标 | 折叠已支持（`--interactive-only` / `--max-nodes`）；跨次调用的增量尚未做 |
| P2 | 下载进度事件 | 现在是轮询 `chrome.downloads.search`；高频进度可用 `onChanged` 推送 |
| P2 | 统一响应信封 + `timing` 字段 | 现在 `/tabs`、`/scan` 仍是 `{ok,result:{...}}` 双层 |
| P2 | `snapshot` 增量与分页游标 | 大页面一次返回全部引用仍然偏大 |
| P2 | 单 tab 并发背压 | 排队已实现（每会话一把串行锁，同标签命令排队执行）；背压 / 优先级策略待定 |
| P3 | `click`/`fill` 全面改页面内动作 | 现在只有 `@e`（JS 快照）与 `--frame` 走页面内；CSS 选择器仍默认 CDP |

> **2026-11-01 更新（已变更）**：上表 `net` 规则那一行已不成立——`net` 规则现在按 `browser:profile` 共享（见 §1「网络 mock」），同 profile 下任意标签的 `net routes` / `net mock` / `net reset` 作用于同一套规则，`--persist` 注册的规则在新标签加载时自动生效，换标签不需要重新 hook。另：`net unroute` 现支持 `--engine page|cdp`，CDP 规则可单独删除，不必 `net reset` 全清；CDP 引擎的「放行」默认不再写日志，`net log` 里留下的是真正被拦的请求。

---

## 4. 边界与限制

**环境限制（不是代码问题）**

1. Chrome 156 在页面内嵌**其他扩展** iframe 时拒绝 `chrome.debugger.attach`（crbug 40753571）。
   - 已全程有回落：`screenshot` → `captureVisibleTab`（`via:"captureVisibleTab"`）；`screenshot --full-page` → 逐屏滚动 + OffscreenCanvas 拼图（`via:"captureFullPageStitched"`、`stitched:true`、`slices`、`truncated`）；`save-pdf` → 整页长图包成单页图片型 PDF（`via:"stitched-image-pdf"`、`text_selectable:false`）；`mouse-click` → 页面内合成事件（`synthetic:true`、`trusted:false`）；`snapshot` → DOM 版（`mode:"js"`，`@e` 仍可用于 `click/fill`）；`network` / `console` / `upload` / `send-keys` 仍需要调试器，会带 `page_protected` 与替代方案。
   - 错误文案会点名「页面内嵌了扩展 XX 的 iframe」（扩展命令 `blockingExtensions`）；需要真实输入事件或可检索 PDF 时，让用户临时禁用该扩展。
2. 页面 CSP 禁止 `eval` 时（GitHub、Google 等），任意 `exec` 不可用；但 `read`/`find`/`snapshot --js`/`action`/`wait` 会**自动切到无 eval 通道**（扩展注入 `ops/page_*.js`），返回里带 `via:"no-eval"`。
3. 依赖顺序：
   - 允许 eval → 全部能力可用；
   - 禁止 eval 但允许注入（大多数 CSP 站）→ `exec` 不可用，DOM 读/查/快照/动作/等待可用；
   - 禁止 eval 且禁止注入（极少）→ 只有 `tabs`/`cookies`/`open`/`close`。
4. 持久注入优先由 `persist_boot.js` 在 document_start 执行（实测页面内 173ms 处已生效）；若浏览器不支持动态注册，会退回「导航提交后重放」，此时比 document_start 晚几毫秒。
5. 指定 `--frame` / `--all-frames` 时不再回退 CDP（CDP 只跑主框架），失败会直接报 `No frame with id N`，不会悄悄在主框架执行。

**设计边界**

- 一个浏览器 profile 一条 WebSocket：命令在扩展内并发处理，但共用一条连接，极端并发下会排队。
- `@e` 引用会跨导航自动重定位（role+name 匹配，最多扫 600 个可操作元素）；如果新页面上同名元素有多个，可能命中第一个而非原来那个；带 `--tab` 的引用不能跨标签复用。
- 无 eval 通道只覆盖固定操作；`exec` 任意 JS、读取页面变量在 CSP 页面仍然不可用。
- 超大页面（>6 万元素）的 `scan` 返回的是文本而不是 HTML（内容头部有 `<!-- scan: ... -->` 说明）；需要 HTML 时请显式 `--max-nodes` 或分块读取。
- `read` 默认只返回可见文本：元素数 ≤ 3 万时用 `innerText`（精确排除 CSS 隐藏）；更大的页面用 TreeWalker 近似（`text_mode:"visible-approx"`，能看到 `[hidden]`/`aria-hidden`/内联样式隐藏，但**看不到 class 级 CSS 隐藏**）。要完整文本用 `--include-hidden`。
- `find --text` 只返回「最内层」匹配元素（避免整段容器命中），因此不会返回包裹层。
- 批量操作失败不重试；`batch` 会跑完所有步骤并逐条报告。
- `wait --js` 的谓词在页面内反复求值：DOM 频繁变更的页面上约 60~80 次/秒（受事件循环批次限制），谓词要保持轻量（`querySelector` / 读全局变量），不要在里面做全量遍历或重计算；精确控制频率可以用 `--interval`。

**0.8.0 新增边界**

- **`wait --network-idle` 只统计等待开始后的 fetch/XHR**：不 hook WebSocket / EventSource（没有天然完成语义），也不统计图片 / 脚本 / 样式等资源；等待开始前已在途的请求不可见。`screenshot --network-idle` 同源。
- **整页截图的 fixed/sticky 仍可能在底部重复**：CDP `captureBeyondViewport` 是浏览器侧合成，固定/吸底元素可能多出现一次；扩展无法消除（拼图通道则会在每屏重复）。回包用 `sticky_artifacts:"possible"` / `hint` 明示。
- **`chrome://` / `edge://` / `about:` 不能由扩展创建**：`open` 返回 `special_scheme_open_failed`；`chrome-extension://` 只对本工具自己的扩展页面可靠，其它扩展页面由 Chrome 拒绝。保留页需用户手动打开后再用 `tabs` 选目标。
- **`atb extensions` 依赖用户授权 optional `management`**：未授权返回 `permission_required`（exit 1），授权入口在扩展设置页「关于 → 高级权限」；授权成功后扩展自动重载一次；命令只读，不提供启用/停用/卸载。
- **job 没有服务端进度回调**：`job tail` 是 CLI 轮询 `/job/get/:id`；`progress` 字段只有 daemon/任务提供时才出现，当前没有通用回调。
- **`measure` 不接受 `@e` / XPath**：只支持 CSS 与 `host >>> inner`；closed shadow root 仍返回 `shadow_closed_unreachable`。
- **`atb env` 是聚合诊断，不是强一致快照**：任一子请求失败只降级成 `available:false + error`；多浏览器且无默认时可能不猜 identity，需看 `warnings` / `hints`。
- **`resolved_target` 不覆盖多目标命令**：`/multi` `/run` `/batch` `/scrape` `/broadcast` 与 `/open` 按设计不富化（单值没有稳定语义）。

**多浏览器能力边界**

- `browsers` / `broadcast` 按扩展上报的 `browser_id + profile_id` 分组，Chrome 与 Edge 各装一份扩展即可同时出现在列表里；每个 profile 取「默认标签」作为该目标。
- `broadcast` 支持 `exec / read / find / scan / net / downloads / click / fill / ...`（与 `batch` 的 op 集合一致）；每个目标上仍是串行执行。
- 扩展未安装/未连接的浏览器不会出现在 `browsers` 里；`broadcast --browser edge` 在没装 Edge 扩展时会返回 `no_target`。
- **隐式默认目标有粘性**：默认标签所在连接断开时，daemon 会把这条标签记进 `preferred_default_session_key`（6 小时 TTL），同一标签重连后自动恢复为默认；只有标签真的关了才会永久切走。`read` 在「≥2 个浏览器 + 未显式指定目标」时会附带 `multi_browser_hint`；`/health.connection` 的 `default_session_key` 与 `profiles[].is_default` 可查当前默认落在哪个浏览器。
- 具名 agent（`--agent`）的默认标签失效时，**动作类命令**直接报 `target_lost` 而不是回退到别人的标签（多智能体不串线）；`open` 例外：它会先清掉失效默认再开新页，所以 `cleanup` 之后仍能继续 `open`。

**已知保留的限制**

- **closed shadow root 不可达**：`find` / `snapshot` / `read` 只能递归 open shadow root；closed shadow root 内的节点页面 JS 与扩展注入都取不到，回包用 `shadow_closed_unreachable:true` 明示。CF 的 Turnstile 就常在 closed shadow root 里，所以 `challenge` 改用 CDP `DOM.getDocument{pierce:true}` 而不是页面查询。
- **Chrome match pattern 不支持端口**：`net hook --persist` 的 `*://host/*` 无法表达 `host:port`，同一 host 的不同端口会互相命中；回包会带 `warning`，要按端口隔离只能用 `--match` 显式指定或改 `net route --engine cdp`。
- **WS 无 TLS**：daemon 与扩展之间的 WebSocket 是明文回环连接。配了 `ws-trust.json` token 后已有双向鉴权；没有 token 时仍是 `compat` 兼容档，扩展无法证明「对端是真 daemon」，共用机器必须显式配 token。
- **`e2e-live` 已在 2026-11-06 本机实跑 6/6 通过**（daemon + 真实扩展 + 受控页面；同一进程内起 daemon，命令结束即回收）；CI 仍跑 `ATB_E2E=smoke`，Chrome 137+ 对 `--load-extension` 的支持以 CI 为准。
- **`snapshot --js` 的 shadow 覆盖依赖脚本版本**：旧扩展不回 `scope` / `shadow_*` 字段，此时 `coverage_complete` 不出现（形状保持兼容），不能据此判断「已覆盖 shadow」。
