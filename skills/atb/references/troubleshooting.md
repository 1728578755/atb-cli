# 排障

先执行目标命令，不要用本文做开机自检。daemon 由命令自动拉起；只有命令失败，且输出指向连接、端口、扩展或标签页时，才按下表定位。

| 现象 | 先看 | 节 |
| --- | --- | --- |
| 连接被拒绝、`daemon_not_running` | `atb tabs` 是否成功 | 1 |
| `extension_not_connected`（status 摘要）/ `extension_disconnected`（命令错误码） | Chrome 是否打开、扩展是否启用 | 2 |
| `port_mismatch` | 扩展 popup 端口与配置端口 | 3 |
| `CLI 与 daemon 版本不配套` / `extension_outdated` | `status` 的 `compatibility` | 4 |
| `no_target_tabs` / `no_active_tabs`（status 摘要） | 是否有普通网页 | 5 |
| `csp_blocked` / `page_protected` | 页面 CSP 与来源 | 6 |
| `element_not_found` / `invalid_selector` | 选择器与 iframe | 7 |
| `challenge_timeout` | `challenge status` 的 reason | 8 |
| 下载停在 `interrupted` | 磁盘与浏览器下载设置 | 9 |
| 两个智能体互相看到对方页面 | 身份与 `--tab` | 10 |
| 其它 | 日志 | 11 |
| `wait_js_failed` / `outcome_unknown` / `ws_backpressure` 等新码 | 错误码含义与处置 | 12 |
| 后台标签懒加载 / 动画不触发 | `tab_hidden` / `timers_throttled` 警告 | 13 |
| 注入后 `GM_getValue is not defined` | 是否用了 `--userscript` | 14 |
| 页面出现两份宿主 / 重复面板 | 安装版脚本与 atb 注入共存 | 15 |
| `inject --list` 有残留 / 持久注入过期 | TTL 与 `--remove` | 16 |
| 截图截到别的标签 / 整页固定元素重复 | `captured_tab` 与 fixed/sticky | 17 |
| `@1` 打错标签 | `@N` 与 `@latest` 语义 | 18 |

## 1. daemon 没起来

**症状**：`status` 的 `summary:"daemon_not_running"`（`capabilities` 等命令会作为 `error_code` 返回），或连接 `127.0.0.1:18767` 被拒绝。**判断**：daemon 默认空闲 300 秒且无扩展连接时退出；扩展连着时不退，断开后有 60 秒重连宽限。`running:false` 不代表安装损坏，下一条目标命令会自动拉起；`doctor` 不会启动 daemon。

**处理**：先 `atb tabs` 触发拉起；仍失败用 `atb restart`；daemon 占着 18767 且 token 不匹配时用 `atb restart --force`。

## 2. 扩展未连接

**症状**：`summary:"extension_not_connected"`，或 `connection.extension_connected:false`。**判断**：按顺序排除浏览器没开、扩展没加载/没启用、popup 端口与 `~/.atb/config.json` 不一致、覆盖过文件但没点「重新加载」。

**处理**：`atb status` 看 `connection`；冷启动慢时用 `atb open <url> --warm` 先唤醒 service worker。重载入口在 `chrome://extensions` 的扩展卡片上。

## 3. 端口不一致

**症状**：`summary:"port_mismatch"`，扩展连不上但 daemon 正常。**判断**：扩展 WebSocket 端口默认 `18765`，可用 `set-extension-port` 改；`18767` 是 CLI HTTP API，固定且不能作扩展端口。

**处理**：先 `atb doctor` 与 `atb restart`；确认要改端口再 `atb set-extension-port <port>`（写 `~/.atb/config.json` 并重启 daemon），然后在扩展 popup 填同一端口并重载。

## 4. 协议或版本不配套

**症状**：`CLI 与 daemon 版本不配套`、`extension_outdated`、`upgrade_recommended:true`、`stealth` 返回 `extension_ok:false`。**判断**：三方须配套：API 协议 `5`、扩展消息协议 `4`、扩展文件版本 `5.5.0`。不配套时命令明确报错，不会静默跑错。

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `CLI 与 daemon 版本不配套` | 旧 daemon 仍占 18767 | `atb restart`；不行用 `--force` |
| `extension_outdated` | 扩展文件是旧的 | 覆盖 `assets/atb_bridge` 后重载 |
| `upgrade_recommended:true` | 扩展低于期望，core 命令可用 | 重载扩展直到 `compatibility.ok` |
| `extension_ok:false` | 扩展低于 5.0.0 或没重载 | 按返回的 `fixes` 重载后再跑 |

覆盖扩展文件不等于生效；不点「重新加载」，页面脚本仍是旧版。

## 5. 没有可用标签页

**症状**：`summary:"no_active_tabs"`、`active_tabs:0`，或命令返回 `no_target_tabs`（旧版本/兼容路径可能仍是 `no_tabs`）。**判断**：当前只有 `about:blank`、`chrome://` 页面或扩展页，这些不能作为操作目标。

**处理**：`atb open https://example.com --background`，再用 `atb tabs --ids` 确认。`open` 也失败时回到第 2 节查扩展，不要反复重开浏览器。

## 6. CSP 或受限页面

**症状**：`error_code:"csp_blocked"`（hint 提到 CSP / inline script），或 `error_code:"page_protected"`。

**判断**：前者是页面 CSP 禁止注入内联脚本；后者是页面内嵌了其它扩展的 iframe，Chrome 拒绝 attach。
两者都看 `error_code` 与返回里的 `via` / `synthetic` 区分。

- `csp_blocked`：页面禁止内联脚本。改用 `read`、`snapshot --js`、`find`、`click`、`fill`、`wait`（都走扩展注入的 `ops/page_*.js`），返回带 `via:"no-eval"`；要在隔离环境跑脚本用 `exec --world isolated`。
- `page_protected`（`chrome.debugger.attach` 被拒，页面内嵌了别的扩展的 iframe）：**不是死路，不要急着重试**
  | 命令 | 回落通道 | 结果标记 |
  | --- | --- | --- |
  | `screenshot` | `captureVisibleTab` | `via:"captureVisibleTab"` |
  | `screenshot --full-page` | 逐屏滚动 + 拼图 | `via:"captureFullPageStitched"`、`stitched`、`slices`、`truncated` |
  | `save-pdf` | 整页长图 → 单页图片型 PDF | `via:"stitched-image-pdf"`、`text_selectable:false` |
  | `mouse-click` | 页面内合成 pointer/mouse 事件 | `synthetic:true`、`trusted:false` |
  | `snapshot` | DOM 版 | `mode:"js"` |

  仍需要调试器的只剩 `network`、`console`、`upload`、`send-keys`、`net mock --engine cdp`、元素/后台标签页截图。
- 错误里已经点名挡路的扩展（例如「检测到页面内嵌了扩展「XX助手」的 iframe」）：需要真实输入事件或可检索 PDF 时，让用户临时禁用该扩展后重试。
- 不要把「合成事件成功」当成真实点击：部分站点只认 `isTrusted` 事件，看到 `synthetic:true` 要自行确认业务是否真的响应。

## 7. 选择器打不到

**症状**：`invalid_selector`、`unsupported_selector_syntax`、`element_not_found`，或点击无报错但没效果。

| 错误码 | 含义 | 处理 |
| --- | --- | --- |
| `invalid_selector` | 括号、引号没配平 | 按 `hint` 的「第 N 行」定位，`example` 可照抄 |
| `unsupported_selector_syntax` | 写了 `text=` / `css=` / `xpath=` | 文本用 `find --text`，XPath 用 `--xpath`，CSS 直接写 |
| `element_not_found` | 没命中或不可见 | `find` 确认；在 iframe 里就加 `--frame` |
| 点击无效 | 目标被浮层盖住 | 看 `warnings`，`action_target_covered` 时用 `mouse-click` |

```bash
atb find 'button.submit' --limit 20
atb frames
atb wait --selector '.result' --visible --timeout 10
```

## 8. 挑战页超时

**症状**：`challenge_timeout`、`challenge_blocked`，或 `challenge.auto_solve.reason` 给出结论。

**判断**：`open --challenge auto`、`challenge solve`、`challenge auto` 会主动点击组件并等待放行；`challenge wait` 才是纯等待。失败按 reason 处置，兜底由智能体自己点，最后才 `focus` 转人工。

| reason | 含义 | 处理 |
| --- | --- | --- |
| `no-widget-frame` | 组件没渲染 | 等更久，或 `--reload-after 10` 重载一次 |
| `click-no-effect` | 点了没反应 | `widget` 重新取坐标后再点；仍无效再 `focus` |
| `verifying_not_completed` | 停在「正在验证」 | 等待或 reload 后重试；卡住再 `focus` |
| `widget-removed-no-nav` | 组件消失但没跳转 | `challenge status` 复查，必要时 `reload` |

```bash
atb challenge status
atb challenge wait --timeout 30 --focus-after 8
atb challenge widget
atb challenge click --at 320,410
atb challenge focus
```

仍不通过就按「`widget` 取坐标 → `click --at` 自己点 → `status` 复查」兜底；只有图片点选、短信/邮箱验证这类需要人判读时再 `focus` 交给人，并明确说明需要人工完成验证；不要循环狂点。

## 9. 下载不完成

**症状**：`downloads` 显示 `interrupted`，或 `download-wait` 超时（退出码 `3`，error 文本以 `download_wait_timeout:` 开头）。**判断**：下载由浏览器执行，atb 只发起和查询。常见原因是磁盘满、默认下载目录无写权限、站点触发「保留/放弃」确认、直链需要登录 Cookie。

**处理**：

```bash
atb downloads --state interrupted --limit 10
atb download <url> --filename report.pdf --dir atb-downloads
atb download-wait --id <downloadId> --timeout 300
```

`--dir` 相对路径按当前工作目录解析，也可传绝对路径；包含 `..` 段会被拒绝。

## 10. 多智能体串扰

**症状**：裸命令打到别人的页面；`cleanup` 关掉了不属于自己的标签；两个智能体读到同一 URL。**判断**：两个调用方都落到 `default` 身份，或都显式指向同一个 `--tab`。

**处理**：

```bash
ATB_AGENT=reviewer atb open https://a.example.com --background
ATB_AGENT=reviewer atb exec 'return location.href'
ATB_AGENT=reviewer atb tabs --mine
ATB_AGENT=reviewer atb cleanup --dry-run
```

- 身份优先级：`--agent` → `ATB_AGENT` → 终端会话标识 → `default`。
- 给每个智能体固定 `ATB_AGENT`；关键操作显式写 `--tab`。
- `cleanup --all-agents` 会清掉所有人的页面，只在确认后使用。

## 11. 日志、重置与重装

日志固定在 `~/.atb/logs/daemon.log`：

```bash
atb logs              # JSON：{ok, lines[], path, truncated}
atb logs --tail 300
atb logs --tail 300 | jq -r '.lines[]'        # 纯行文本
atb logs --tail 300 | jq '.lines[] | select(test("attach|handshake"))'
```

`logs` 现在返回结构化 JSON，不再逐行裸写 stdout；`--follow` 仍不支持。拿行用 `.lines[]`，`truncated:true` 表示 tail 到上限被截断。重点找 attach/detach 失败、端口监听、扩展握手与版本拒绝。

Chrome 顶部出现「正在控制你的浏览器」通常正常：执行 CDP 命令时 attach，命令结束或 daemon 退出时 detach。只有命令同时失败或日志报错时才继续排查。

重置按影响从小到大：

```bash
atb restart                 # 重新读配置并拉起 daemon
atb restart --force         # 强杀占用 18767 的进程
atb trust-extension --list
atb trust-extension --clear # 清空白名单与 token，回到兼容状态
```

`trust-extension` 是单向的：只约束「谁能连 daemon」，不校验「扩展连的是不是真 daemon」。清空后任何本地扩展都能连，确认环境可信再执行。

重装 skill 先看计划，不要直接覆盖：

```bash
atb install-skill --dry-run
atb install-skill
```

实体目录是 `~/.agents/skills/atb`：**每次安装都会整体更新**（plan 里的 `update_main`），有自定义修改先备份。

五个 Agent 目录（`~/.codex/skills`、`~/.claude/skills`、`~/.config/agents/skills`、`~/.cursor/skills`、`~/.gemini/skills`）只建软链接；同名位置若已是**实体目录**，会跳过且 `--yes` 也不覆盖。这条「不覆盖」只针对这五个目录，不包括主目录。

`~/.workbuddy/skills/atb` 不在写入范围内，升级后手动同步（先备份），装完用 `atb schema --json` 验证：

```bash
cp -r skills/atb/. ~/.workbuddy/skills/atb/
atb schema --json
```

## 12. 新增错误码（0.8.0）

| 错误码 | 症状 | 判断 | 处理 |
| --- | --- | --- | --- |
| `wait_js_failed` | `exec --wait-js` 返回 `ok:false`，但脚本已经执行 | 等待条件没满足；`js_return` 里可能已有结果，`wait.ok=false` / `matched=false` | 按失败处理，不要当成功继续；加 `--wait-timeout`、改谓词（保持轻量），或先 `wait` 再取结果 |
| `challenge_probe_failed` | `challenge status` 返回 `probed:false`、`status_usable:false` | 挑战探测拿不到可用载荷：扩展断连、注入被拒、或页面已导航 | 重试一次；持续失败看 `atb doctor`，**不要**当成「没有挑战」继续操作 |
| `host_not_allowed` | CLI 连 daemon 返回 403 | 请求的 `Host` 不是 `127.0.0.1` / `localhost` / `[::1]` | 用回环地址访问；不要用局域网 IP、自定义域名或反代主机名 |
| `ws_backpressure` | 命令偶发失败，扩展侧卡顿 | 目前只把 pong 控制通道改成有界（`ATB_WS_OUTBOUND_MAX`），命令通道仍是 `unbounded`，所以该错误码目前是**预留**，正常不会出现 | 偶发卡顿等几秒重试；持续出现重载扩展或 `atb restart`；大量并发改用 `batch` 控制并发 |
| `outcome_unknown` | 退出码 4，`outcome_unknown:true` | 请求已发出但响应丢失（超时/断连），动作**可能已执行** | 带同一个 `--idempotency-key` 重发；不要换 key 重试副作用命令；先 `status` 看会话是否还活着 |
| `agent_no_default` | `target_lost`，`reason:"agent_no_default"` | `ATB_SESSION_ISOLATION=strict`，具名 agent 没有自己的默认目标 | 先 `atb open <url>` 建默认；或临时改回 `legacy`（`ATB_SESSION_ISOLATION=legacy`） |
| `focus_failed` | `atb focus` 返回失败 | 标签可能已关闭、扩展过旧，或窗口/标签切换被浏览器拒绝 | `tabs --ids` 确认标签仍在；重载扩展；只想切标签不抢系统焦点时加 `--no-window` |
| `permission_required` | `atb extensions` 返回失败 | 扩展没有授予可选权限 `management` | 打开扩展设置页「关于 → 高级权限」，由用户点击授权；授权成功后扩展会自动重载一次（MV3 SW 需要重载才能拿到 management API），刷新后重试；这是可选权限，不影响其它命令 |
| `daemon_outdated` | 新命令报 HTTP 404 / `invalid_response` | CLI 已升级，但常驻 daemon 还是旧二进制，未知路由 | `atb restart` 后重试；必要时 `atb restart --force` |
| `syntax_error` | `exec` / `inject` 在发送前失败 | 本地 JS 配平/语法检查未通过，错误带行列与片段 | 按提示修正；确认是误报时用 `--no-preflight` 或 `ATB_JS_CHECK=off` |
| `inject_script_threw` | `inject` 顶层 `ok:false`，但 `executed:true` | 脚本已经执行并自己抛错；副作用可能已发生 | 看 `threw` 与脚本逻辑；不要当注入通道失败盲目重试 |
| `unsupported_selector` | `measure` 等命令拒绝选择器 | 传了 `@e`、伪类前缀或不受支持的形态 | 用 `find` 拿真实 CSS 选择器；shadow DOM 写 `host >>> inner` |
| `exec_result_not_found` | `job result` 找不到结果 | `exec_id` 错误、结果已被取走，或超过 TTL | 确认 `exec_id` 来自超时错误；长任务调大 `ATB_EXEC_RESULT_TTL_SEC` 或改用 `--async` + `job wait` |
| `cdp_required` | `screenshot --require-cdp` 失败 | CDP 不可用（页面嵌了别的扩展 iframe，或 `ATB_NO_DEBUGGER=1`） | 去掉 `--require-cdp` 接受回落，或让用户临时关闭挡路扩展 |
| `invalid_encoding` | `exec --file` / `inject --file` 失败 | 脚本文件不是 UTF-8 | 转成 UTF-8；或用 `--file -` 从 stdin 传入 |
| `script_file_too_large` | 读取脚本文件失败 | 文件超过 `ATB_SCRIPT_FILE_MAX_MB`（默认 32MiB） | 拆成多个 `--file`，或提高 `ATB_SCRIPT_FILE_MAX_MB` |

## 13. 后台标签页懒加载 / 动画不触发

**症状**：命令 `ok:true`，但懒加载列表没加载、图表没渲染、`requestAnimationFrame` / `IntersectionObserver` 回调不执行；响应 `warnings` 里有 `tab_hidden` / `timers_throttled`；`env` 的 `warnings` 提示默认目标在后台。

**判断**：Chrome 会暂停后台标签的 rAF 与 IntersectionObserver 回调，并节流定时器。`ATB_HIDDEN_TAB_WARNING=off` 只是关掉提示，不会改变浏览器行为。`resolved_target.active` / `visible` 为 `false` 就是后台。

**处理**：

```bash
atb focus --tab @latest
atb focus --tab <tabId> --no-window     # 只激活标签，不抢系统窗口焦点
atb env                                  # 看 default_target.visible 与 warnings
```

- 需要可见行为（截图、动画、懒加载、可见性判断）时先 `focus`，再执行读取 / 截图。
- 不要用固定 `sleep` 等懒加载；先 `focus`，再 `wait --selector` 或 `wait --dom-stable`。
- `atb screenshot` 的 `tab_visible` / `captured_tab` 可以确认截图时目标是否真的可见。

## 14. 注入环境没有 GM API

**症状**：脚本报 `GM_getValue is not defined`、`unsafeWindow is not defined`，或 `GM_xmlhttpRequest is not supported by atb userscript shim`。

**判断**：只有 `inject --userscript` 才注入 GM shim；普通 `inject` / `exec` 是裸脚本环境。GM shim 是最小实现，不是完整 Tampermonkey：不支持 `GM_xmlhttpRequest`，不会处理 `@require` / `@resource`，`@grant` 只用于元数据展示。

**处理**：

```bash
atb inject --userscript --file ./fix.user.js --gm-namespace fix-demo
atb exec 'return {gm: typeof GM_getValue, u: typeof unsafeWindow, info: typeof GM_info}'
```

- 确认命令里带了 `--userscript`，并且 `--file` 指向真实脚本。
- 需要页面全局变量（`unsafeWindow`）时用默认 `--world main`；`--world isolated` 看不到页面变量。
- 依赖 `GM_xmlhttpRequest` 的脚本要在页面内改用 `fetch`，或由宿主代发请求；`@require` 的库需要手动拼进 `--file`。
- 脚本抛错会返回 `inject_script_threw`；先看 `threw`，不要把它当成 GM 环境缺失。

## 15. 页面已有安装版脚本导致重复宿主

**症状**：页面出现两个面板 / 两份角标，事件触发两次，网络请求翻倍；`document.querySelectorAll('#fix-host').length` 大于 1。

**判断**：用户已在 Tampermonkey / Violentmonkey 里装了同一脚本，atb 又注入了一份；或之前多次 `inject` 没清理旧节点。

**处理**：

```bash
atb exec 'return document.querySelectorAll("#fix-host").length'
atb inject --userscript --file ./fix.user.js --cleanup-selector '#fix-host'
atb inject --list                       # 看是否有旧的持久注入
atb inject --remove <旧 name>
```

- 最稳的是让用户在该站点临时停用安装版，再测试 atb 注入版。
- 脚本自身应做幂等：注入前判断宿主是否存在，存在就复用或先移除。
- `--cleanup-selector` 可写多次；`--cleanup-file` 适合删除旧全局变量或复杂旧节点。
- 不要靠反复 `inject` 覆盖：旧节点和旧事件监听不会自动消失。

## 16. persist 残留 / 过期

**症状**：`inject --list` 里还有早已不用的名字；或之前能跨导航生效的脚本突然不再注入；`expires_in_sec` 很小或为 `null`。

**判断**：`--persist` 默认 7200 秒（2 小时）过期，扩展设置页的 `persistTtlSec` 参与默认值；`expires_in_sec:null` 表示永久，数字是剩余秒数。读取列表时会清掉已过期条目，所以「名字消失」通常就是过期；`--match` 写得太窄也会让新页面不命中。

**处理**：

```bash
atb inject --list
atb inject --remove <name>
atb inject --userscript --file ./fix.user.js --persist <name> --ttl 600   # 重新注入给更短寿命
atb inject --userscript --file ./fix.user.js --persist <name> --persist-forever --i-know-this-is-long-lived
```

- 清理不再需要的持久注入，避免它们在新页面里继续跑。
- 永久驻留必须同时给 `--persist-forever --i-know-this-is-long-lived`；只给其中一个会被预检拒绝。
- `--match` 默认 `<all_urls>`，测试时尽量收窄到目标站点，减少残留影响。
- 改了脚本内容要重新 `inject --persist`，旧条目不会自动更新。

## 17. 截图截到前台窗口 / 整页重复

**症状**：截图内容和目标标签不一致；`captured_tab.tab_id` 不是请求的 tab；整页截图底部出现重复的导航栏 / 悬浮按钮。

**判断**：

- `captureVisibleTab` 只能截「浏览器前台窗口的当前激活标签」。目标在后台、用户切了窗口，或 `focus` 与截图之间又被切走，就可能截到别人。
- 整页 CDP `captureBeyondViewport` 下 `position:fixed/sticky` 元素可能在底部重复；逐屏拼接通道则是在每一屏重复。这是浏览器渲染限制。
- `tab_visible` / `captured_tab` / `clip_box` / `degraded` 是判断依据，不要只看 `ok:true`。

**处理**：

```bash
atb focus --tab <tabId>
atb screenshot --wait-selector '#main' --network-idle --out shot.png
atb screenshot --full-page --require-cdp --out full.png
atb screenshot --selector '#main' --out main.png      # 只要内容区时优先元素截图
```

- 先 `focus` 再截图，中间不要并行执行会切标签的命令。
- 需要确定性 CDP 时加 `--require-cdp`，失败会直接报 `cdp_required`。
- 固定元素重复：临时用 `inject` 改成 `position:absolute`，或改截元素 / 视口，不要期望 `--max-height` 能去掉重复。
- `clip_box` 是 CSS px；`--max-height` 是设备像素，HiDPI 下不要混用。

## 18. `@1` 打错标签

**症状**：`--tab @1` 打到了用户刚开的页面，或和上一次不是同一个；`--tab @latest` 与 `@1` 结果不同。

**判断**：`@N` 是 `tabs` 当前输出顺序的第 N 个，浏览器新开标签、切换顺序或过滤条件变化都会让它指向别的页面；`@latest` 才是 daemon 记录的最近打开 / 连接标签。

**处理**：

```bash
atb tabs --ids
atb tabs --url-contains example --format tsv
atb use <tabId>                 # 固定默认目标，后续裸命令不再漂移
atb focus --tab @latest
atb click --tab @1 --dry-run    # 先看 targets 再执行
```

- 关键操作一律显式写 `tab_id`，不要依赖 `@N`。
- 需要「刚才打开的那个」用 `@latest`；需要「列表第 1 个」才用 `@1`。
- `use <tabId>` 之后，裸命令固定打到该标签；换目标时重新 `use`。
- 多目标 `@1,@3` / `@1-@3` 已支持逐 token 解析，但仍建议先 `--dry-run` 核对 `targets`。
