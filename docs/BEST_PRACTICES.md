# 最佳实践

面向两类读者：把 atb 接进自己智能体的工程师，以及直接用命令行的人。

---

## 1. 给每个智能体一个稳定身份

默认身份是 `default`。多个智能体同时跑时，虽然 `--tab` 能保证目标正确，但"裸命令"会共用同一个默认标签页。

```bash
# 方式一：写进智能体的环境（推荐）
export ATB_AGENT=reviewer

# 方式二：每条命令显式指定
atb --agent reviewer open https://example.com
```

优先级：`--agent` → `ATB_AGENT`（旧名 `AGENT_BROWSER_CLI_AGENT`）→ 终端会话标识 → `default`。
同一台机器开两个 Windows Terminal 窗口时，`WT_SESSION` 不同，会自动隔离。

**判断是否隔离成功**：两个身份各自 `open` 不同页面后，各自执行不带 `--tab` 的
`exec 'return location.href'`，应当分别返回自己打开的那个 URL。

---

## 2. 批量操作：一条命令，而不是一个循环

```bash
# 好：1 个进程、1 个 HTTP 请求、daemon 内并发
atb read --tab url:docs --max-chars 2000

# 差：N 个进程串行，每个都要建连、解析参数
for id in 111 222 333; do atb read --tab $id; done
```

实测（6 个标签页、同一台机器）：批量 138ms，串行 648ms。

选择器的选取顺序建议：

1. 能确定 id → `--tab 111,222`（最快、最明确）
2. 按业务分组 → `--tab url:order` / `--tab title:订单`
3. 调试/探索 → `tabs --ids` 看清单，再用 `--tab @1-3`
4. 清理 → `close --tab ...` 或 `cleanup`

---

## 3. 先 dry-run，再执行

会影响多个标签页的命令，先看一眼解析结果：

```bash
atb click --tab all '#load-more' --dry-run
```

输出里的 `targets` 是 daemon 实际会打的标签页。如果多了一个不该出现的（比如用户自己开的同域页面），
换成更精确的 `--tab` 再执行。

同样的思路适用于 `open`：`open url1 url2 --dry-run` 只做语法与本地文件校验，不会开标签页。

---

## 4. 收尾一定要清理

任务结束调用：

```bash
atb cleanup --dry-run   # 确认清单
atb cleanup             # 关闭本智能体 open 的标签页
```

`cleanup` 的依据是"本工具 `open` 过的会话"，不按 URL 匹配，所以不会关掉用户自己的页面。
多个智能体共用一台机器时，各自清理各自的；需要一次性收干净用 `--all-agents`。

**不要**用 `close --tab all`（不支持），也不要用 `pkill chrome` 这类粗暴手段。

---

## 5. 版本配套与升级

三个版本：CLI/daemon 的 API 协议（当前 5）、扩展的消息协议（当前 4）、扩展的文件版本（当前 5.5.0）。

```bash
atb status | jq '.compatibility'
```

- `ok:true`：三方配套；
- `ok:false` + `issues[].kind:"extension_outdated"`：扩展旧了。用仓库里的
  `assets/atb_bridge` 覆盖扩展目录，然后在 `chrome://extensions` 重新加载；
- CLI 报 `CLI 与 daemon 版本不配套`：旧 daemon 还占着 18767。先 `restart`，
  不行就按提示结束占用端口的进程。

升级顺序建议：**先换扩展文件 → 重载扩展 → 再升级 CLI（或先 `atb restart`）**。反过来也能用，
只是中间会有一段时间 `compatibility.ok:false`。协议 5 的 CLI 会直接拒绝连旧 daemon，
不会"尽力而为"地解析旧响应。

**覆盖扩展文件不等于重载扩展**：5.0.0 的改动（页面操作走 ISOLATED world、角标默认关闭）
都在扩展现行代码里，不点「重新加载」就完全没生效——`atb stealth` 的 `extension_ok` 会是 false。

---

## 6. 少用 `exec`，多用封装命令

| 场景 | 推荐 | 原因 |
| --- | --- | --- |
| 读正文 | `read` | 单次注入、按预算截断，比 `document.body.innerText` 省 token |
| 找元素 | `find` / `snapshot` | 返回可操作的 selector / `@eN` 引用 |
| 点击 | `click` / `mouse-click` | 处理滚动、遮挡、真实事件序列 |
| 等待 | `wait --selector/--text/--js` | 页面内 MutationObserver，不占进程 |
| 网络 | `net hook` / `net mock` | 页面侧包装，抗 CSP 更好 |

`exec` 适合：封装命令覆盖不到的一次性读取、调试、以及在隔离世界跑脚本。

---

## 7. 稳定性与性能

- **别用固定 `sleep`**：用 `wait`，或 `exec --wait-js`。
- **大页面**：`read --max-chars` / `scan --fast` / `snapshot --max-nodes` 限流。
- **并发上限**：`/multi` 默认 8 路。页面很重时降到 4 路更稳（目前通过 `--tab` 分组间接控制）。
- **长任务**：超过 30 秒的操作用 `--async` + `job wait`，不要阻塞调用方。
- **daemon 空闲退出**是正常行为（默认 300 秒且无扩展连接时），不需要常驻；扩展连着时它不会退出，下一条命令也会自动拉起。



## 8. 结果语义：`ok:true` 不等于「动作成了」

组合命令（`--wait-js`、`broadcast`、`scrape`、`run`、`batch`）会同时带回结果与子结论。判断成败以 **顶层 `ok`** 为准，不要只看退出码为 0：

- `exec --wait-js`：脚本本身跑成功但等待失败时，`js_return` 里是 `{result, wait}` 或带 `combined_wait:true`，daemon 会写 `error_code:"wait_js_failed"` 并把顶层置为 `ok:false`、退出码 1。**看到 `wait_js_failed` 一律按失败处理**，不要当成「脚本跑完就继续」。
- 页面上 `wait.ok=false` / `matched=false` 是结论，不是噪声；重试要带新的 deadline，而不是原样重发。
- 严格度可调：`ATB_RESULT_STRICT=off|warn|strict`（默认 `warn`）。CI 与批量脚本建议 `strict`——任一子结论失败即整体 `ok:false`。
- `outcome_unknown`（退出码 4）表示「请求已发出，响应没回来」。只有带同一个 `--idempotency-key` 才可安全重发；不要换 key 重试副作用命令。

---

## 9. 超时预算：一条命令一个 deadline

`--timeout` 只管单次等待，**不管整条命令**；多 iframe、多次重试、分片轮询会把它成倍放大。默认由 daemon 维护总预算：

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_WAIT_MAX_TOTAL_SEC` | 600 | `wait` / `--wait-js` 整条命令的总上限（不是每片） |
| `ATB_WAIT_MIN_INTERVAL_MS` | 50 | 轮询下限，防 `--interval 0` 忙轮询 |
| `ATB_HTTP_SLACK_SEC` | 10 | CLI 发 HTTP 时在业务超时上追加的余量 |
| `ATB_CHALLENGE_CLICK_TIMEOUT_SEC` | 8 | 挑战坐标点击整条链的硬上限 |
| `ATB_CHALLENGE_MAX_IFRAMES` | 8 | 单次挑战探测处理的 iframe 上限（按面积取前 K） |

实践：

- 外层给总预算，内层用「剩余时间」：不要把同一个 `Duration` 复用到循环每一轮。
- 预计超过 30 秒的操作改用 `--async` + `job wait`，把等待从命令行移到 daemon。
- `job wait` / `job result` 超时与 `download-wait` 超时的退出码是 `3`，普通 `wait` 超时是 `1`；脚本按码分支，不要解析文本。

---

## 10. 被站点判定为自动化时怎么排查

先别改浏览器参数，先看**站点到底读到了什么**。这一步只有 `atb stealth audit` 能回答：

```bash
atb stealth audit
atb stealth audit --tab 111 --timeout 20
```

输出里的 `score` / `level` 只是概览，真正要读的是 `findings[].scope`——它决定了这锅该谁背、该按什么顺序处置。

### 处置顺序

**第 0 步：确认扩展是新版**

`extension_ok:false` 说明浏览器里跑的还是 5.0.0 之前的扩展。此时审计可能显示"干净"，但页面操作仍在往 MAIN world 写全局——**先按返回的 `fixes` 重载扩展，再重新审计**，否则后面全是错的。

**第 1 步：`scope:"self"` —— 本工具自己的，先清掉**

| finding id | 含义 | 处置 |
| --- | --- | --- |
| `own_globals` | 页面 window 上有本工具注入的全局函数 | `atb stealth scrub` |
| `own_dom_badge` | 页面 DOM 里有扩展角标节点 | popup 里关掉「显示状态角标」，或 `scrub` 临时移除 |
| `own_net_hook` | `fetch` / XHR 被网络钩子包装过 | `atb network stop` 或 `scrub`；只抓包用 `net mock --engine cdp` |

```bash
atb stealth scrub
# → restored / removed / remaining
```

注意 `scrub` 会连带停掉 `net hook`。正在抓包的页面别顺手 scrub，会断流。

**第 2 步：`scope:"browser"` —— 启动方式的问题，页面内修不了**

| finding id | 典型原因 | 处置 |
| --- | --- | --- |
| `navigator_webdriver` | 用 `--enable-automation` 或调试端口拉起 Chrome | 用日常的 Chrome 手动启动，装扩展连 daemon；不要用自动化框架的启动器 |
| `ua_headless` / `no_plugins` / `zero_outer_size` | headless 模式 | 换有头模式重开浏览器 |
| `software_webgl` | 传了 `--disable-gpu` / `--use-gl=swiftshader` 或虚拟机无 GPU | 去掉这些参数，用真实 GPU |
| `no_timezone` | headless 环境未设时区 | 设置系统时区 |
| `notification_permission_mismatch` | headless 下常见 | 有头模式重开 |

这一类**不要**试图用页面 JS 去"补"——改 `navigator.webdriver` 之类只是给这一个页面贴创可贴，换页面就露，而且等于伪造。

**第 3 步：`scope:"unknown"` —— 别人的痕迹**

- `automation_framework`（`__playwright` / `__puppeteer` / `__selenium` / `cdc_` 等）：说明这个浏览器 profile 被别的自动化框架接管过，去掉对应框架的注入 / 启动参数，或用干净的 profile。
- `patched_network`（`fetch`/XHR 非原生，但本工具没开 hook）：多数是油猴脚本或别的扩展改的，逐个停用定位。
- `chrome_runtime_exposed`：别的扩展声明了 `externally_connectable` 或注入了 web_accessible resource。


---

## 11. 排障顺序

```text
命令报错
  ├─ 看 error_code
  │   ├─ invalid_* / unsupported_*  → 命令写错，按 hint 改
  │   ├─ target_not_found / no_target_tabs → tabs --ids 复核目标
  │   ├─ extension_outdated         → 重载扩展（见第 5 节）
  │   └─ timeout / csp_blocked      → 换命令或加超时
  ├─ 站点判定为自动化
  │   ├─ stealth audit → 看 findings[].scope
  │   ├─ self         → stealth scrub / network stop
  │   └─ browser      → 换有头模式、真实 GPU（见第 10 节）
  └─ 连接类问题
      ├─ status       看 summary / compatibility / next_steps
      ├─ doctor       逐项自检（不启动 daemon）
      └─ logs --tail  看 daemon 日志
```

## 12. 安全档位选择

默认安装是「单机单人」档：`ATB_WS_TRUST_MODE=compat`，谁装了扩展谁就能连 18765。需要更强隔离时按场景选：

| 场景 | 建议 | 命令 |
| --- | --- | --- |
| 单机、单人、只用一份扩展 | 保持默认 `compat` | — |
| 同机装了多份扩展 / 只想允许一份 | `ATB_WS_TRUST_MODE=whitelist` + `trust-extension <id>` | `atb trust-extension <extensionId>` 后 `ATB_WS_TRUST_MODE=whitelist atb restart` |
| 共用机器（有别的系统用户或不可信进程） | `both`（白名单 + token） | `atb trust-extension <id> --token <value>`，再 `ATB_WS_TRUST_MODE=both atb restart` |
| 只想要握手 token、不维护白名单 | `token` | `atb trust-extension --token <value>` 后切档 |

- 非 `compat` 档是 fail-closed：配置缺失或校验失败会直接拒绝握手，不会退回放行。
- 切档后跑一次 `atb status`：`security.mode` 与 `security.warning` 会告诉你当前档位；`compat` 一定带告警。
- 关掉 `ATB_NO_DEBUGGER` 之外的加固：`ATB_NO_DEBUGGER=1` 禁止 attach；`ATB_IDEMPOTENCY_PERSIST=redacted`（默认）确保幂等记录不写响应体。
- 白名单/token 约束「谁能连 daemon」；配上 token 后握手还有双向鉴权（`ws_hello` + HMAC proof），扩展能验证对端是不是真 daemon；没配 token 时扩展无法验证对端身份，这是默认档的已知边界。
- 从 Release 手工下载二进制时先核 `SHA256SUMS`，
  从 npm 安装时用 `npm audit signatures` 校验 provenance；平台包哈希在 `package-lock.json` 里锁死，
  发布门禁要求与 registry 一致（详见 `SECURITY.md` 的「发布产物校验」）。

---

## 13. 前端测试与用户脚本的五条习惯

### 13.1 先 `focus`，再测懒加载 / 动画

后台标签页会被 Chrome 暂停 `requestAnimationFrame`、降低 `IntersectionObserver` 优先级并节流定时器；`exec` / `screenshot` / `wait` 回包里的 `tab_hidden` / `timers_throttled` 就是在说这件事。

```bash
atb focus --tab @latest          # 切前台（默认同时聚焦系统窗口）
atb focus --tab 123 --no-window  # 只激活标签页，不抢窗口
```

需要「真实可见」才出现的图片 / 列表 / 动画，先 focus 再断言；批量脚本不关心可见性时可用 `ATB_HIDDEN_TAB_WARNING=off` 静音提示，但不要据此认为后台行为与前台一致。

### 13.2 用户脚本注入前先 cleanup

用户脚本经常改同一块 DOM。重复注入会在页面上叠出多份宿主节点，后续断言就会数错。

```bash
atb inject --userscript --cleanup-selector '#my-panel' --cleanup-selector '.old-widget' \
  --file script.user.js --tab @latest
# 或者把清理代码放进文件：
atb inject --userscript --cleanup-file cleanup.js --file script.user.js
```

`--cleanup-selector` 可重复；清理失败只记 `cleanup` 结果，不会中断主脚本。持久注入还要注意 TTL：默认 2 小时（`--ttl` 可调），永久驻留必须显式 `--persist-forever --i-know-this-is-long-lived`。

### 13.3 长脚本用 `--file`，不要塞命令行

Windows 命令行有约 32K 字符上限，长脚本还会让 shell 转义变成主要故障源。

```bash
atb exec --file setup.js --file assertions.js --tab @latest
cat script.js | atb exec --file - --tab @latest   # stdin
```

多个 `--file` 按出现顺序拼接；单个文件上限 `ATB_SCRIPT_FILE_MAX_MB`（默认 32 MiB）。本地配平检查误报时用 `--no-preflight` 或 `ATB_JS_CHECK=warn`，但不要把语法错误留给浏览器。

### 13.4 大浏览器先过滤 `tabs`

100+ 标签时 `tabs --ids` 本身就会吃掉大量 token，而且列表顺序不代表「最近使用」。

```bash
atb tabs --format jsonl --url-contains order --limit 20
atb tabs --format tsv --title-contains 订单
atb tabs --fields tab_id,title,url --mine
```

过滤在 daemon 侧执行；`tabs_total` 是过滤前总数，`filtered` 是过滤后数量。选中目标后优先 `atb use <tab_id>` 或 `--tab @latest`，不要靠 `@1` 猜「最近那个」。

### 13.5 截图先等页面稳定

`captureVisibleTab` / CDP 都不会替你等异步渲染。固定 `sleep` 要么不够、要么浪费；用截图自带的等待参数：

```bash
# 等选择器出现，再等 300ms 稳定
atb screenshot --wait-selector '#chart canvas' --settle-ms 300 --out chart.png

# 等网络空闲（只统计等待开始后的 fetch/XHR，不含 WebSocket/EventSource/静态资源）
atb screenshot --network-idle --out page.png

# 要求必须走 CDP；被拒时直接失败，不静默降级
atb screenshot --full-page --require-cdp --out full.png
```

回包里看 `via` / `tab_visible` / `captured_tab` / `clip_box` / `degraded`：`via:"captureVisibleTab"` 表示走了降级通道，`captured_tab` 告诉你实际截到谁。整页截图的 fixed/sticky 元素仍可能在底部重复一次，这是 `captureBeyondViewport` 的浏览器限制；需要严格无重复时先隐藏固定元素或改用逐屏拼图并接受每屏重复。
