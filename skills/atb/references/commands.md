# 命令参考

按域列出高频命令。完整参数与默认值优先看 `atb <command> --help`；`atb schema --json` 给的是顶层命令的参数表（子命令细节仍需 `--help`）。

通用参数对多数页面命令有效：`--tab`、`--browser`、`--profile`、`--timeout`、`--async`、`--pretty`、`--dry-run`、`--agent`、`--idempotency-key`。不支持某个全局参数的命令会报 `unsupported_flag`（退出码 2），不会静默忽略。下表只列该命令特有或最常改的参数。

参数约定：

- 数值型 `--timeout` 单位是秒，可带小数（如 `0.5`）。
- 布尔开关只写 flag 本身，不需要 `=true`。
- 可重复参数（`--header`、`--match`、`--browser`）写多次，不要用逗号拼。
- 占位符用尖括号，如 `<tabId>`、`<port>`、`<path>`；照抄时替换成实际值。

## 标签页与目标

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `tabs` | `--ids` / `--fields tab_id,title,url` / `--limit N` / `--mine` / `--full` / `--url-contains` / `--title-contains` / `--filter` / `--format json\|jsonl\|tsv` | 标签清单，含 browser_id / profile_id / tab_id；`--limit 0` = 不截断，`tabs_count` 始终是原始总数，另给 `returned_count`；大列表附 `suggest` |
| `focus` | `--tab` / `--browser` / `--profile` / `--no-window` / `--timeout` | 把目标标签切前台；`focused` / `tab_id` / `url` / `title` / `window_id` / `window_focused`；只取第一个目标 |
| `use <target>` | `--browser` / `--profile` | 把当前 agent 的默认目标固定到该标签；命中多个返回 `target_ambiguous`，不会静默取第一个 |
| `tabtree` | `--full` | 按 browser → profile → 标签页树形展示归属 |
| `lookup tab` / `lookup browser` / `lookup profile` | 位置参数为 id 或名称 | 反查归属，确认打的是哪个浏览器、哪个 Profile |
| `browsers` | 无 | 已连接的浏览器实例列表 |
| `open <url...>` | `--background` / `--window` / `--focus` / `--group-title` / `--wait-for` / `--wait-for-timeout` / `--challenge` / `--challenge-timeout` / `--warm` | 新标签 id 与 `ready`；多个 URL 并发打开 |
| `close --tab <spec>` | `--yes`（`group:` 必填） | 关闭结果；`all` / `url:` / `title:` 被拒绝 |
| `cleanup` | `--dry-run` / `--all-agents` | 只关本智能体 `open` 出来且未关闭的标签页 |
| `back` / `reload` | `--tab` | 导航结果 |

`--tab @latest` 表示 daemon 记录的「最近打开 / 最近连接」标签页；`@1` 仍是 `tabs` 输出顺序的第一个。目标里写 `@1,@3` / `@1-@3` 也支持逐 token 解析。

`open --wait-for` 的形式：`dom-quiet`、`selector:<CSS>`、`text:<文本>`、`none`（跳过条件等待）。超时返回 `wait_for_timeout`，不影响其它目标。`chrome://` / `chrome-extension://` / `about:` 等特殊页默认用 3 秒预算（`ATB_OPEN_SPECIAL_WAIT_SEC`）；`chrome-extension://` 可以打开本工具自己的页面。

`dom-quiet` 是**近似**的「网络空闲」：资源计数稳定 + DOM 静默 ≥1s + 连续 3 次 200ms 采样无变化，不是真正的 network idle。旧名 `networkidle` 作为兼容别名保留，语义相同。

`open` 默认前台打开并切到新标签；`--background` 不切换标签页，`--focus` 连窗口一起聚焦，二者互斥。

改动多个标签页前先看解析结果：

```bash
atb click --tab all '#load-more' --dry-run
```

输出里的 `targets` 就是 daemon 实际会打的标签页。多出不该出现的页面，就换更精确的 `--tab`。

## 页面读取

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `read` | `--selector` / `--max-chars`（默认 20000） / `--links` / `--no-forms` / `--include-hidden` / `--frame` | 标题、正文、表单结构、计数；open shadow DOM 结果带 `scope:"shadow"` |
| `scan` | `--text-only` / `--tabs-only` / `--fast` / `--max-nodes` | 内容感知结果；`--fast` 走紧凑抽取 |
| `snapshot` | `--interactive-only` / `--limit`（默认 200） / `--offset` / `--details` / `--js` / `--max-nodes` | 带 `@eN` 引用的可操作树；JS 快照透传 `scope` / `frame_count` / `shadow_*` / `coverage_complete` / `coverage_hint` |
| `find` | `<selector>` / `--text` / `--xpath` / `--limit`（默认 50） / `--exact` / `--include-hidden` / `--frame` | 命中的元素、位置与可用于后续动作的 selector；shadow 内路径写作 `宿主 >>> 相对路径` |
| `measure` | `--selector`（必填，CSS 或 `宿主 >>> 内层`） / `--styles`（默认 `display,position,width,height,zIndex`） / `--with-text` / `--frame` | `tag` / `id` / `class` / `rect` / `viewport` / `visible` / `display` / `visibility` / `styles` / `shadow_path`；不接受 `@e`（`unsupported_selector`），成功结果不再套 `js_return` |
| `frames` | 无 | 页面内 iframe 列表与 frame_id |
| `cookies` | `--url` / `--clear` / `--domain` | Cookie 列表（含 httpOnly），仅本地返回 |

单目标页面命令的响应顶层还会带 `resolved_target`（tab_id / session_key / url / title / browser_id / profile_id / active）、`browser`、`profile`、`warnings`、`hint`。目标在后台时 `warnings` 含 `tab_hidden` / `timers_throttled`，先 `focus` 再等懒加载或动画。

`snapshot --js` 强制走 DOM 快照，不依赖 CDP；CDP 被其它扩展 iframe 挡住时用。默认折叠无操作价值的 generic 容器；要保留无障碍树里的 generic 节点用 `--no-skip-generic`。

三种读法的取舍：

- 只要正文或表单字段：`read`，默认 20000 字符，超长截断。
- 页面结构复杂、想先看概览：`scan`，结果偏语义，不含精确 selector。
- 要能点的元素引用：`snapshot --interactive-only`，树较大，配 `--limit` 或 `--max-nodes`。
- 已知 CSS / 文本：`find`，只返回命中项，最省 token。

大页面可用 `read --budget-ms` / `--max-nodes` 限流；`--inner-text-limit` 是**节点数阈值**（不是字符数）：节点数不超过该值时用 `innerText` 精确抽取，超过则退化为近似可见文本。默认 30000，传 0 等同未指定，正数按 1000~500000 clamp。

## 页面操作

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `click <target>` | `--frame` / `--wait-js` / `--timeout` | 点击结果与 `warnings`（如 `action_target_covered`） |
| `mouse-click <target>` | `--frame` | 按坐标或元素发真实鼠标事件；普通 click 无效时用 |
| `fill <target> [value]` | `--append` / `--clear` / `--frame` | 默认覆盖，返回写入后的值 |
| `send-keys <keys>` | `--target` / `--frame` | 向焦点元素或指定元素发按键（`Enter`、组合键） |
| `hover <target>` | `--index` / `--frame` | 触发 hover 菜单 |
| `select <target>` | `--value` / `--label` / `--index` | 选中项与 `el.value`；未命中时返回可选列表 |
| `check <target>` | `--index` | 勾选 checkbox / radio；已勾选返回 `unchanged:true` |
| `scroll` | `--to top` / `--to bottom` / `--dy <像素>` | 滚动后的位置 |
| `upload <target> <file...>` | `--frame` | 给文件输入框设置本地文件，可传多个 |
| `wait` | `--selector` / `--text` / `--js` / `--visible` / `--dom-stable[=ms]` / `--network-idle[=ms]` / `--timeout`（默认 10） / `--interval` | 条件成立即返回；超时返回 `ok:false` 与 `error:"timeout"`（退出码 1）；`--js` 超时带最近 5 次 `last_values` |
| `inject <script>` | `--file` / `--userscript` / `--gm-namespace` / `--cleanup-selector`（可多次） / `--cleanup-file` / `--persist <name>` / `--ttl <sec>` / `--persist-forever --i-know-this-is-long-lived` / `--remove <name>` / `--list` / `--match` / `--world` / `--all-frames` / `--no-preflight` | 注入结果 `executed` / `returned` / `threw` / `duration_ms` / `cleanup`；`--persist` 命名后每次导航自动重放，默认 2 小时过期；`--list` 给 `expires_in_sec` / `warning` |

所有操作类命令都支持 `--wait-js`：动作完成后在页面内等条件成立，比固定 `sleep` 稳。`--index` 用于命中多个元素时按索引取（从 0 开始）。顶层只有 `check`（勾选）；需要取消勾选时，在 `run` 步骤里用 `{"op":"uncheck",...}`，当前没有 `atb uncheck` 顶层命令。

`wait --dom-stable` 等 DOM 连续静默 500ms（`ATB_WAIT_DOM_STABLE_MS`），`wait --network-idle` 等在途 fetch / XHR 归零并静默；两者都接受显式毫秒值，如 `--dom-stable 800`。

`inject --userscript` 注入最小 GM shim：`GM_getValue` / `GM_setValue` / `GM_deleteValue` / `GM_listValues` / `GM_registerMenuCommand` / `GM_log` / `GM_addStyle` / `GM_info` / `unsafeWindow`。`GM_xmlhttpRequest` 不支持，`@require` 不会自动加载。脚本抛错时顶层 `ok:false` + `inject_script_threw`，但 `executed:true` 表示副作用可能已经发生。`--persist` 默认 7200 秒过期（扩展设置 `persistTtlSec` 参与默认值），永久驻留必须同时给 `--persist-forever --i-know-this-is-long-lived`。

## 截图与 PDF

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `screenshot` | `--selector` / `--full-page` / `--format png\|jpeg\|webp` / `--quality` / `--max-height` / `--settle-ms` / `--wait-selector` / `--network-idle[=ms]` / `--require-cdp` / `--out` | `path` / `via` / `tab_visible` / `captured_tab` / `clip_box` / `degraded` / `warning` / `warnings`；整页 `clip_box` 用 CSS px |
| `save-pdf` | `--paper` / `--landscape` / `--scale` / `--no-print-background` / `--max-height` / `--out` | PDF 路径、页数与尺寸；CDP 被拒时回落为图片型 PDF（`via:"stitched-image-pdf"`、`text_selectable:false`） |

- `--settle-ms` 默认 `ATB_SCREENSHOT_SETTLE_MS`（0）；`--network-idle` 不带值表示静默窗口 500ms（`ATB_SCREENSHOT_NETWORK_IDLE_MS`），可写 `--network-idle 800`。
- `--wait-selector` 支持 `host >>> inner`，截图前先等该元素出现。
- `--require-cdp` 在 CDP 不可用时直接返回 `cdp_required`，不静默回落 `captureVisibleTab` 或整页拼接。
- `--max-height` 按设备像素；CLI 不传即不截断，扩展整页拼图通道默认上限 20000。整页 CDP `captureBeyondViewport` 下 `position:fixed/sticky` 仍可能在底部重复，拼接通道则每屏重复，这是浏览器渲染限制。
- 后台标签截图前先 `focus`；结果里的 `tab_visible` / `captured_tab.tab_id` 用于确认实际截到谁，`degraded:true` 表示走了回落通道。

## 脚本与批处理

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `exec <script>` | `--file`（可多次，按顺序拼接；`-` 读 stdin） / `--world main\|isolated` / `--frame` / `--all-frames` / `--wait-js` / `--max-chars`（`--max-bytes` 别名） / `--no-preflight` | `js_return`；超长会截断预览；本地配平检查默认 `strict`（`ATB_JS_CHECK`） |
| `userscript <file-or-url>` | `--parse-only` / `--tab` | 解析 `==UserScript==` 并打开安装页；`--parse-only` 只返回 `metadata` / `valid`（仅本地文件） |
| `run <steps.json>` | `--continue-on-error` | 串行执行多步，遇错即停 |
| `batch <steps.json>` | `--concurrency`（默认 4，1-32） | 并发执行多步，步骤需显式给 `switch_tab_id` |
| `scrape` | `--urls` / `--out` / `--js-file` / `--concurrency`（4，1..=32） / `--batch`（4，1..=32） / `--retry`（3） / `--timeout`（30） / `--group-title` / `--record-attempts` | JSONL：每行 `{url, tab_id, ok, data, error, elapsed_ms, attempts, outcome_unknown}`；越界退出 2，`outcome_unknown` 不重试且最终退出 4 |
| `job list` | `--limit`（默认 20） | 最近的后台任务 |
| `job get <jobId>` | 无 | 任务状态与结果 |
| `job tail <jobId>` | `--interval`（0.5） / `--timeout`（600，0=跟到结束） / `--human` | JSONL 进度快照：`elapsed_ms` / `last_heartbeat` / `heartbeat_age_ms` / `stalled`；终态退出 0/1/130，跟踪超时 3 |
| `job result <exec_id>` | `--wait`（默认 5） / `--interval`（0.5） | 取回超时 exec 的迟到结果；命中 `found:true` 退出 0，等待超时 `exec_result_not_found` 退出 3 |
| `job wait <jobId>` | `--timeout`（默认 300） / `--interval` | 阻塞到结束；超时不丢任务 |
| `job cancel <jobId>` | 无 | 协作式取消，等待类任务立即停止 |

`scrape --urls` 接受文件路径（每行一个，行内可逗号分隔）或逗号分隔的 URL 列表，自动去重。`--js-file` 的脚本需 `return` 一个可 JSON 序列化的值；缺省返回 `{title, url, text}`。

`exec --file` 可重复，按出现顺序拼接；`--file -` 从 stdin 读。文件读不到、不是 UTF-8 或超过 `ATB_SCRIPT_FILE_MAX_MB`（默认 32）时，错误会带绝对路径与大小。`--no-preflight` 只跳过当前命令的本地配平检查。exec 超时错误会给出 `exec_id`，用 `job result <exec_id> --wait 60` 取回；迟到结果默认保留 3600 秒（`ATB_EXEC_RESULT_TTL_SEC`）。`job tail` 的 `stalled:true` 由 `ATB_JOB_STALL_SEC`（默认 60 秒）判定。

`run` / `batch` 的步骤文件结构：

```json
{
  "steps": [
    { "op": "fill", "args": { "target": "#q", "value": "hello" } },
    { "op": "click", "args": { "target": "#submit", "wait_js": "return !!document.querySelector('.done')" } },
    { "op": "exec", "args": { "script": "return document.title" } }
  ]
}
```

`run` 在同一个标签页串行执行，遇错即停；`batch` 并发执行，每步必须显式给 `switch_tab_id`。两者都省掉 N−1 次进程启动与 HTTP 往返。

`exec` / `click` / `fill` / `mouse-click` / `send-keys` 还支持 `--monitor`（等待页面变化）以及 `--wait-js` / `--wait-timeout` / `--wait-interval`。`monitor` 返回 `change = {changed, added, removed, attributes, text, samples}`；旧的 `change.top_change` 已移除。

## 网络与调试

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `net fetch <url>` | `--method` / `-H 'Name: value'` / `--body` / `--credentials include\|omit\|same-origin` / `--response-type text\|base64` / `--no-parse-json` / `--max-chars` | 带页面 Cookie 与同源策略的请求结果 |
| `net hook` | `--capture-bodies` / `--persist` / `--match` / `--max-log` | 开始记录页面 fetch/XHR；`--persist` 导航后自动重装 |
| `net log` | `--limit`（默认 50） / `--match` / `--clear` / `--engine page\|cdp\|both` / `--debug-pass` | 拦截记录；`--debug-pass` 也记放行的请求 |
| `net mock <pattern>` | `--status` / `--body` / `--method` / `-H` / `--abort` / `--request-header` / `--request-body` / `--response-status` / `--response-body` / `--engine page\|cdp` | 命中规则与伪造响应；规则可用 `routes` 查看 |
| `net routes` | `--frame` | 当前 mock 规则 |
| `net unroute <pattern>` | `--engine page\|cdp` | 删除规则 |
| `net reset` | `--frame` | 还原 fetch/XHR 并清空状态 |
| `network start` | `--tab` | 开始抓取页面网络请求 |
| `network list` | `--filter` / `--limit`（默认 100） | 请求列表 |
| `network detail <requestId>` | 无 | 单条请求的详情 |
| `network clear` / `network stop` | 无 | 清空记录 / 结束抓取 |
| `console start` | `--tab` | 开始抓取 console |
| `console list` | `--level` / `--limit`（默认 100） | console 条目 |
| `console history` | `--settle-ms`（默认 `ATB_CONSOLE_HISTORY_SETTLE_MS` 300） / `--types error,warn,log,info,debug,rejection` / `--clear` / `--limit`（200） | 一次取回加载期 console / error / unhandledrejection；`started_by_history` 表示本次是否临时启动采集 |
| `console tail` | `--duration`（10） / `--interval`（0.5） / `--types` / `--limit` | 轮询输出新条目，最后一行 `tail_finished`；**不会**自动启动采集，需先 `console start` 或 `console history` |
| `console clear` / `console stop` | 无 | 清空 / 结束 |
| `downloads` | `--state in_progress\|complete\|interrupted` / `--id` / `--limit` | 下载列表 |
| `download <url>` | `--filename` / `--dir` | 发起下载；`--filename` 会被净化（basename、拒绝 `..`/保留名/分隔符），回包 `filename_sanitized`；`--dir` 相对路径按当前工作目录解析，含 `..` 会被拒 |
| `download-wait` | `--id` / `--timeout`（默认 120） / `--interval` | 省略 `--id` 时自动认领本 agent 最近一条；`ATB_DOWNLOAD_AUTO_CLAIM=false` 时必须显式给 `--id`（否则 `download_id_required`） |

`net mock --engine cdp` 走扩展侧 Fetch 域，能改写 `responseType=blob` 的二进制响应，也能拦 `img` / `script` 这类静态请求；代价是会 attach 调试器。

mock 一条接口并验证：

```bash
atb net mock '/api/user' --status 200 --body '{"name":"test"}' --header 'Content-Type: application/json'
atb reload
atb net routes
atb net log --limit 20
atb net reset
```

只改请求不改响应时用 `--request-header` / `--request-body`；只改真实响应用 `--response-status` / `--response-body`。

## 挑战与痕迹

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `challenge status` | `--tab` | 是否在挑战页、组件位置、`auto_solve.reason` |
| `challenge wait` | `--timeout`（默认 30） / `--focus-after` / `--no-focus` / `--reload-after` | 等站点自行放行；超时返回 `challenge_timeout` |
| `challenge solve` | `--timeout`（默认 120） / `--confirm-ms` | 主动求解并等结果；`--confirm-ms` 是点击后等页面确认的毫秒数（默认 250） |
| `challenge auto` | `--timeout` | 只求解一次并立即返回，不持续等待 |
| `challenge widget` | `--tab` | 组件位置与可点坐标 |
| `challenge click` | `--at x,y` | 智能体按坐标点一次（自动求解失败后的兜底） |
| `challenge focus` | `--tab` | 把标签页带到前台，交给人工（最后通道） |
| `stealth audit` | `--timeout`（默认 15） | `score` / `level` / `findings[].scope` |
| `stealth scrub` | `--tab` | 清除本工具自身痕迹；会连带停掉 `net hook` |

`open --challenge auto`（默认）在命中挑战页时已经包含一次主动求解 + 等待；`challenge solve` 手动触发同一路径，`challenge auto` 只求解一次，`challenge wait` 是纯等待。自动求解失败时用 `challenge widget` 取坐标、`challenge click --at` 由智能体自己点，`challenge focus` 才是转人工。

## 多浏览器与广播

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `browsers` | 无 | 浏览器实例、Profile、连接状态 |
| `broadcast <op>` | `--script` / `--file` / `--args '{"max_chars":2000}'` / `--browser` / `--profile` / `--concurrency` / `--timeout` | 对所有或指定 browser/profile 并发执行同一操作；`--args` 里的输出路径按当前工作目录绝对化，`--dry-run` 在本地出计划（不拉 daemon） |
| `lookup tab` | `<tabId>` | 该标签属于哪个浏览器 / Profile |
| `profile-label set <label>` | `--profile` / `--browser` / `--tab` | 给 Profile 起可读别名，之后可用 `--profile <label>` |
| `profile-label clear` | `--profile` / `--browser` / `--tab` | 清除别名 |

`broadcast` 的 `op` 是命令名（`exec` / `read` / `find` / `scan` / `click` 等），其余参数用 `--args` 传 JSON，键名与 schema 一致（下划线风格，如 `max_chars`）。

## 运维与诊断

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `status` | 无 | daemon、扩展连接、可用标签页、`compatibility`；新增 `browser` / `browser_label` / `profile` / `browser_summary`；`ok` 跟随 `healthy` |
| `env` | `--browser` / `--profile` / `--tab` / `--tab-limit`（10） | 一次聚合 browser / profile / extension / tabs / injections / warnings；`tabs.shown` 前部是默认目标与激活标签，带 `is_default` |
| `extensions` | `--browser` / `--profile` / `--include-disabled` / `--timeout` | 已安装扩展 `items` / `count`；未授权 `management` 返回 `permission_required`（设置页「关于 → 高级权限」） |
| `doctor` | `--fix` / `--force` | 安装位置、端口、扩展逐项诊断；`--fix` 尝试自动修复（旧 daemon 占端口、token 失配、扩展过旧），`--force` 才结束进程 |
| `capabilities` | 无 | daemon/扩展版本与能力门禁（`core` / `open_session` / `group` / `window` / `current` / `challenge`）及阈值 |
| `schema --json` | 无（缺省就是 JSON） | 顶层命令的参数、类型、默认值、error_code；子命令用 `--help` |
| `logs` | `--tail N`（默认 100） | JSON：`{ok, lines[], path, truncated}`；按行消费读 `.lines[]`，不支持 `--follow` |
| `stop` | 无 | 停止 daemon |
| `restart` | `--force` | 重启 daemon；`--force` 强杀占用 18767 的进程 |
| `daemon` | 无 | 前台运行 daemon，日志打到 stdout（调试用，平时自动拉起） |
| `set-extension-port <port>` | 无 | 写 `~/.atb/config.json` 并重启 daemon |
| `trust-extension <id>` | `--list` / `--remove` / `--clear` / `--token` / `--clear-token` | WebSocket 信任配置 |
| `install-skill` | `--dry-run` / `--yes` | 安装到 `~/.agents/skills/atb`，并向已存在的 skill 目录建软链接 |

## 浏览器启动

| 命令 | 关键参数 | 返回要点 |
| --- | --- | --- |
| `launch` | `--restart` / `--force` / `--browser chrome\|edge\|chromium` / `--user-data-dir` / `--arg` / `--no-restore` / `--timeout`（默认 20） | 用 `--silent-debugger-extension-api` 启动，去掉调试横幅 |

不加 `--restart` 时，浏览器已在运行只返回 `browser_already_running`，不会关窗口。`--restart` 会先温和关闭现有窗口并恢复标签页；`--force` 在温和关闭无效时强杀，未保存的表单会丢。

## 调试器 attach 矩阵

不 attach（页面脚本 / 扩展 API）：

`open`（未命中挑战页）、`read`、`scan`、`find`、`measure`、`exec`、`wait`、`hover`、`select`、`check`、`scroll`、`inject`、`cookies`、`focus`、`use`、`env`、`extensions`、`userscript`、`tabs` / `tabtree` / `close` / `cleanup`、`downloads` / `download` / `download-wait`、`net fetch` / `net hook` / `net log` / `net routes` / `net unroute` / `net reset`，以及带 CSS selector 的 `@e` 引用或 `--frame` 触发的 `click` / `fill`。

attach（CDP / `chrome.debugger`）：

`snapshot` 默认（要避开用 `--js`）、纯 CSS 选择器的 `click` / `fill`、`mouse-click`、`send-keys`、`upload`、`network` / `console`、整页/元素截图、`save-pdf`、`net mock --engine cdp`、挑战求解/点击、显式 CDP。

**attach 被拒时（页面内嵌了别的扩展 iframe，Chrome 报 `Cannot access a chrome-extension:// URL of different extension`）** 不是死路，会自动换通道并在结果里标注：

| 命令 | 回落通道 | 结果标记 |
| --- | --- | --- |
| `screenshot` | `captureVisibleTab` | `via:"captureVisibleTab"` |
| `screenshot --full-page` | 逐屏滚动 + 拼接 | `via:"captureFullPageStitched"`、`stitched:true`、`slices`、`truncated` |
| `save-pdf` | 整页长图 → 单页图片型 PDF | `via:"stitched-image-pdf"`、`text_selectable:false` |
| `mouse-click` | 页面内合成 pointer/mouse 事件 | `synthetic:true`、`trusted:false` |

判断依据看 `via` / `synthetic` 字段，不要只看 `ok:true`：合成事件不被部分站点信任，图片型 PDF 的文字不可检索。需要真实输入或可检索 PDF 时，让用户临时禁用注入 iframe 的扩展（错误文案会给出扩展名）。

`broadcast`、`run`、`batch`、`scrape` 的 attach 取决于它们执行的具体操作。`ATB_NO_DEBUGGER=1` 会禁止 attach；依赖 CDP 的命令返回替代方案或失败。

## 环境变量逃生口

部分新增行为以 `ATB_*` 环境变量暴露，命令行没有同名 flag（`atb schema` 的 description 里会注明）：

| 变量 | 默认 | 影响 |
| --- | --- | --- |
| `ATB_RESULT_STRICT` | `warn` | 组合命令严格度：`off` / `warn` / `strict` |
| `ATB_REQUIRE_TAB` | 关 | 写类命令缺 `--tab` 时直接退出 2，不落到默认标签 |
| `ATB_IO_OVERWRITE` | `backup` | `--out` 已存在时：`backup`（留 `.bak`）/ `force` / `ask`（退出 2）/ `append` |
| `ATB_IO_SANITIZE_FILENAME` | `strict` | 下载名净化；`compat` 只告警不改名 |
| `ATB_IO_MAX_OUTPUT_MB` | 50 | 单文件/缓冲字节上限，超限 `capture_too_large` |
| `ATB_CAPTURE_MAX_PIXELS` | 8000 万 | 拼接截图/图片 PDF 的像素总量上限 |
| `ATB_ALLOW_RELATIVE_OUT` | 关 | `broadcast` 的 `out` 保留旧的相对路径行为 |
| `ATB_FORCE_ASSUME_ATB` | `strict` | `restart --force` / `doctor --fix` 的身份判据；`loose` 放宽 |
| `ATB_MULTI_SCOPE` | `agent` | `--tab all` 的默认归属范围；`all` 才跨 agent |
| `ATB_SESSION_ISOLATION` | `legacy` | `strict` 时具名 agent 无默认目标直接 `target_lost` |
| `ATB_DOWNLOAD_AUTO_CLAIM` | `true` | `download-wait` 无 `--id` 时是否自动认领 |
| `ATB_DOWNLOAD_CLAIM_TTL_SEC` | 600 | 自动认领窗口 |
| `ATB_DOWNLOAD_CANCEL_ON_TIMEOUT` | 关 | 超时是否取消下载 |
| `ATB_DOWNLOAD_CANCEL_ERASE` | 关 | 取消后是否擦除下载记录 |
| `ATB_WS_TRUST_MODE` | `compat` | WS 信任档位：`compat` / `whitelist` / `token` / `both` |
| `ATB_HTML_KEEP_SCRIPTS` | 关 | `scan`/`read` 的 HTML 优化保留 `<script>`（调试用） |
| `ATB_CHALLENGE_FOCUS_ON_NEED_HUMAN` | `true` | `challenge` 需要人工时是否自动切前台 |
| `ATB_EXTENSION_ID` | 自动探测 | 覆盖扩展 ID（多份扩展共存时用于识别自身 iframe） |
| `ATB_BROWSER_BANNER` | `once` | CLI stderr「→ 正在操作：…」提示：`off` / `once` / `always` |
| `ATB_HIDDEN_TAB_WARNING` | `true` | 后台标签是否附 `tab_hidden` / `timers_throttled` 警告 |
| `ATB_OPEN_SPECIAL_WAIT_SEC` | 3 | `chrome://` / `chrome-extension://` / `about:` 的 `open --wait-for` 预算 |
| `ATB_WAIT_DOM_STABLE_MS` | 500 | `wait --dom-stable` 的静默窗口（毫秒） |
| `ATB_SCREENSHOT_SETTLE_MS` | 0 | `screenshot --settle-ms` 的默认值 |
| `ATB_SCREENSHOT_NETWORK_IDLE_MS` | 500 | `screenshot --network-idle` 静默窗口 |
| `ATB_JS_CHECK` | `strict` | `exec` / `inject` 本地配平检查：`off` / `warn` / `strict` |
| `ATB_SCRIPT_FILE_MAX_MB` | 32 | 单个 `--file` 脚本上限（MiB） |
| `ATB_EXEC_RESULT_TTL_SEC` | 3600 | 超时 exec 迟到结果保留时长 |
| `ATB_INJECT_PERSIST_DEFAULT_TTL_SEC` | 7200 | `inject --persist` 默认寿命 |
| `ATB_INJECT_PERSIST_MAX_TTL_SEC` | 2592000 | `inject --ttl` 最大值 |
| `ATB_CONSOLE_HISTORY_SETTLE_MS` | 300 | `console history` 采样窗口 |
| `ATB_JOB_STALL_SEC` | 60 | `job list` / `job tail` 判定 `stalled` 的心跳超时 |
| `ATB_TABS_LARGE_WARNING` | 50 | `tabs` 超过该条数时附 `suggest` |

## 完整清单

```bash
atb schema --json                      # 顶层命令的参数表
atb schema --json | jq '.commands.read'  # 单条命令
```

`schema` 输出的参数键名带 `--` 前缀；位置参数不带前缀。error_code 列表是「该命令可能返回」的集合，不是穷举，实际以 `hint` 与 `example` 为准。
