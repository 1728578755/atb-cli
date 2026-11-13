# 后台运行与异步任务

> 目标：让智能体在**不打断用户当前浏览**的前提下操作页面，并且不必阻塞等待长任务。
> 本文描述本次改造后的行为、用法与边界。实现集中在 `src/server/run.rs` / `src/server/state.rs` / `src/server/boot.rs`（任务队列、daemon 侧等待）、`src/cli/mod.rs` / `src/cli/cmd_pipeline.rs`（`--async` 分发、`job`）、`assets/atb_bridge/*`（后台保活、降低页面开销）。

---

## 1. 三项能力

| 能力 | 用户感受 | 实现方式 |
|---|---|---|
| 后台打开与操作 | 标签页在后台创建，不切换、不抢焦点，用户继续做自己的事 | `open --background` 设 `active:false`；默认是前台打开；受控标签页设置 `autoDiscardable:false`，避免被"内存节省"丢弃 |
| 异步任务 | 提交后立刻拿到 `job_id`，智能体可以先去干别的，或直接把控制权交还用户 | daemon 内 `JobStore` + `tokio::spawn`；`--async` 只负责提交，`job wait` 负责取结果 |
| 后台等待 | 隐藏标签页里的 `--wait-js` 不再必然超时 | 等待从"页面内 `setTimeout` 循环"改成"daemon 侧轮询"（Chrome 会把隐藏标签页定时器节流到秒级甚至分钟级） |

---

## 2. 行为变更（重要）

### 2.1 `open` 默认前台，`--background` 才后台

```bash
# 默认：前台打开，切到新标签页（不聚焦系统窗口）
atb open https://example.com

# 后台打开：不切标签页、不抢焦点，用户继续做自己的事
atb open https://example.com --background

# 需要连窗口一起聚焦时
atb open https://example.com --focus
```

- 无论前台还是后台，**新标签页都会成为后续命令的默认目标**，所以 `open` 之后可以直接 `exec` / `click`，不需要每次 `--tab`。
- `open` 会最多等 3 秒，直到新标签页成为可操作会话；返回里 `result.ready` 表示是否可以立即操作。
- `--background` 与 `--focus` 互斥（clap 校验）。

### 2.2 受控标签页禁止自动丢弃

扩展在创建标签页后调用 `chrome.tabs.update(tabId, { autoDiscardable: false })`。
这样即使用户打开了很多标签页，Chrome 的"内存节省"也不会把我们正在操作的页面回收掉，避免"命令执行到一半标签页没了"。

### 2.3 长任务的上限与失败语义

- `exec --timeout` 现在真正生效：默认 60 秒，可放大到 600 秒；超时返回 `ok:false` + `exec_timeout`（以前是 15 秒硬上限，且超时返回 `ok:true` + 一段提示字符串，容易被误判为成功）。
- `scan --timeout` 同样生效，页面内容抓取超时返回 `scan_timeout`。
- 异步任务没有额外总时限：各操作按自己的 `timeout` 约束，`job wait` 只影响"等多久取结果"，不影响任务本身。

### 2.4 页面侧开销降低（不打扰用户）

- 右下角状态角标：只在页面**可见**时轮询（原先是所有标签页每 3 秒一次）。
- 内容脚本补注入：按扩展版本只做一次（原先每次 Service Worker 唤醒都会对所有标签页 × 所有 iframe 重注入）。
- 标签页列表同步：250ms 去抖（一次导航会触发多条 `onUpdated`，原先会连续全量同步）。

---

## 3. 异步任务用法

### 3.1 支持范围

支持异步（提交后立刻返回 `job_id`）：

`exec`、`read`、`find`、`scan`、`snapshot`、`cookies`、`frames`、`click`、`mouse-click`、`fill`、`send-keys`、`hover`、`select`、`check`、`scroll`、`upload`、`inject`、`wait`、`screenshot`、`save-pdf`。

不支持异步，带 `--async` 一律 `unsupported_flag` + 退出码 2（清单与 `src/cli/mod.rs::reject_async_if_unsupported` 及各命令入口的 `reject_async_if_set` 一致）：

| 类别 | 命令 |
| --- | --- |
| 路由 / 会话 | `open`、`close`、`cleanup`、`tabs`、`tabtree`、`browsers`、`lookup` |
| 管道 / 批处理 | `run`、`batch`、`scrape`、`broadcast` |
| 网络 / 下载 | `net`、`network`、`console`、`downloads`、`download`、`download-wait` |
| 挑战 / 痕迹 | `challenge`、`stealth` |
| 任务 / 运维 | `job`、`status`、`doctor`、`capabilities`、`logs`、`launch`、`restart`、`stop`、`daemon`、`set-extension-port`、`schema` |
| 配置 | `profile-label`、`install-skill`、`trust-extension` |

新增命令时要同步这两处：分发层的 `reject_async_if_unsupported`（跨命令族拒绝）与命令入口的 `reject_async_if_set`（单命令拒绝）；漏一处就是「flag 被静默吞掉」。

```bash
atb --async exec 'return document.title'
atb --async scan --text-only --tab 123456
atb --async snapshot --details --limit 200
atb --async screenshot --full-page --out /tmp/full.png
```

返回：

```json
{
  "ok": true,
  "job": { "id": "0f3c…", "op": "exec", "status": "running", "created_at": 1793512483.7 },
  "hint": "atb job wait 0f3c…"
}
```


### 3.2 取结果

```bash
atb job list                     # 最近任务（含 elapsed_ms / last_heartbeat / stalled）
atb job get <job_id>             # 查一次
atb job wait <job_id> --timeout 300
atb job cancel <job_id>
atb job tail <job_id>            # 轮询跟随，状态/进度变化时输出 JSONL 快照
atb job result <exec_id> --wait 60   # 取回超时 exec 的迟到结果
```

`job wait` 会阻塞到任务结束；超时不会丢弃任务，返回 `error_code: "job_wait_timeout"` 并附带当前状态，可以稍后再查。

`job tail` 不是服务端推送：CLI 每 `--interval`（默认 0.5s）轮询 `/job/get/:id`，只在状态、进度或 `stalled` 变化时输出一行；任务结束后输出最终结果并按 succeeded/failed/cancelled 映射 0/1/130。`--timeout 0` 表示跟到结束（上限 24h）。

`job result` 轮询 `GET /exec-result/:id`。这个结果来自「扩展在 CLI 超时后才回包」的 exec，daemon 暂存 **60 分钟**（`ATB_EXEC_RESULT_TTL_SEC`，60..86400，默认 3600）、最多 512 条；`exec_timeout` 的 hint 会直接给出 `atb job result <exec_id> --wait 60`。等待超时 exit 3，`--wait 0` 的立即未命中 exit 1（`exec_result_not_found`）。

任务完成后的结构：

```json
{
  "ok": true,
  "job": {
    "id": "0f3c…",
    "op": "exec",
    "status": "succeeded",          // running | succeeded | failed | cancelled
    "elapsed_ms": 208,
    "last_heartbeat": 1793512483800, // epoch ms
    "heartbeat_age_ms": 1200,
    "stalled": false,
    "result": { "status": "success", "js_return": "Example Domain", "tab_id": "…" },
    "error": null,
    "session_key": "browser-…:profile-…:123456"
  }
}
```

说明：

- 任务结果与同步接口完全一致，直接复用同一套执行函数，不存在"两套逻辑"。
- `stalled` 只表示「running 且距上次心跳超过 `ATB_JOB_STALL_SEC`（默认 60s）」，不是「页面无响应」的判定；终态永不 stalled。`last_heartbeat` 与 `heartbeat_age_ms` 都是 epoch 毫秒口径（时钟回拨时年龄夹到 0）。
- 取消是**协作式**的：等待类任务立刻停止；已经发出的页面脚本会执行完当前步骤。
- daemon 在**有任务运行时不会因空闲退出**（空闲计时会让路），任务结束才恢复计时；即使计时到点，只要还有扩展连着也不会退。
- 任务元数据软上限 100 条（超出按「最旧已结束」批量淘汰 20 条）、硬上限 500 条；硬淘汰 Running 任务前先 `abort()`，淘汰存根只保留最近 32 条。
- 结果只在 daemon 内存里；daemon 重启后 `job get` 会返回 `job_not_found`。需要长期留存的产物请落到文件（`screenshot --out` / `save-pdf --out`）。超时 exec 的迟到结果另有 60 分钟 TTL，见 `job result`。

### 3.3 `health` 新增字段

```json
{ "jobs": { "running": 1, "total": 7 } }
```

`status` 输出中也会带上该字段，方便判断"daemon 是不是正忙"。

### 3.4 多步管道：`run`

一次进程执行多步，省掉 N−1 次进程启动与 HTTP 往返：

```bash
atb run steps.json
```

```json
{
  "steps": [
    { "op": "exec",   "args": { "script": "return document.title" } },
    { "op": "fill",   "args": { "target": "#q", "value": "hello", "has_value": true } },
    { "op": "click",  "args": { "target": "#submit", "wait_js": "return !!document.querySelector('.done')" } },
    { "op": "screenshot", "args": { "out": "shot.png" } }
  ]
}
```

- 支持 `exec / scan / snapshot / click / mouse-click / fill / send-keys / upload / screenshot / save-pdf`。
- 默认遇错即停（`stopped_at` 标记位置），`--continue-on-error` 可继续。
- 返回逐步结果：`{ok, count, total, stopped_at, steps:[{index, op, ok, elapsed_ms, result|error}]}`。

### 3.5 失败可编程与输出体积

- 退出码：`0` 成功 / `1` 执行失败（含普通 `wait` 与 `challenge` 超时）/ `2` 命令写错（含 `--async` 不支持）/ `3` 仅 `job wait`、`job result` 与 `download-wait` 超时 / `4` 结果未知（动作可能已执行，带同一 `--idempotency-key` 重发）/ `130` 任务被取消。
- `ATB_EXIT0=1` 可恢复旧的「失败也返回 0」行为，仅在有脚本依赖旧行为时使用；`0` / `false` / 空串不再开启（旧实现只要设置就生效）。
- 默认输出 **compact JSON**（单行）；需要人读时加 `--pretty`。
- `tabs` 默认把 URL 截断到 80 字符，`tabs --full` 输出完整 URL；内部数据模型始终保留完整 URL。

### 3.6 相关环境变量

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `ATB_AGENT` | 终端会话标识 → `default` | 调用方身份，异步任务在提交时捕获，不跟随 daemon 的当前默认 |
| `ATB_HOME` | `~/.atb` | 配置目录；只影响配置与 token，不影响 `~` 展开 |
| `ATB_IDLE_TTL` | 300 | 无扩展连接时的空闲退出秒数（有任务运行或扩展连着时不退） |
| `ATB_MAX_RUNNING_JOBS` | 32 | 同时运行的 job 上限，硬淘汰前先 abort |
| `ATB_JOB_STALL_SEC` | 60 | `job list/tail` 判定 running 任务「卡住」的心跳超时（5..3600 秒） |
| `ATB_EXEC_RESULT_TTL_SEC` | 3600 | 超时 exec 迟到结果的保留时长（60..86400 秒） |
| `ATB_EXIT0` | 关 | 恢复旧的恒 0 退出码，值须为 `1` / `true` |
| `ATB_HTTP_SLACK_SEC` | 10 | CLI 计算 HTTP 超时时在业务超时上追加的余量 |
| `ATB_WAIT_MAX_TOTAL_SEC` | 600 | `wait` / `--wait-js` 单条命令的总预算上限 |
| `ATB_WAIT_MIN_INTERVAL_MS` | 50 | `wait --interval` 的最小值，防止 `interval=0` 忙轮询 |
| `ATB_IDEMPOTENCY_PERSIST` | `redacted` | 幂等记录落盘档位：`redacted` / `full` / `off` |
| `ATB_IDEMPOTENCY_FILE_MAX_MB` | 32 | `idempotency.jsonl` 运行期上限，超限后台压缩 |

`ATB_AGENT` / `ATB_HOME` / `ATB_EXIT0` 的旧名（`AGENT_BROWSER_CLI_AGENT` 等）仍兼容，但新脚本请用 `ATB_*`。


---

## 4. 后台等待为什么必须改

Chrome 对**隐藏标签页**的定时器有两级节流：

1. 普通后台标签页：`setTimeout`/`setInterval` 最快约 1 秒一次；
2. 长时间后台（"intensive throttling"）：可降到 1 分钟一次。

旧实现把等待循环写在页面里：

```js
while (Date.now() < deadline) {
  if (await run(waitJs)) break;
  await new Promise(r => setTimeout(r, 100));   // ← 后台标签页里变成 1s / 60s
}
```

结果：后台标签页里 `--wait-js --wait-timeout 10` 往往只能轮询几次就超时，表现为"页面明明已经好了，但命令报 wait 超时"。

改造后：页面只负责**回答一次判断**，循环交给 daemon：

```text
daemon: 默认每 50ms 注入一次 wait_js（`ATB_WAIT_MIN_INTERVAL_MS`，值域 1..=60000）→ 读返回值 → 真值则返回
页面:   每次注入都是同步执行，不受标签页可见性影响
```

输出形状保持兼容：

```json
{ "ok": true, "result": { "js_return": { "result": "…", "wait": { "ok": true, "polls": 5 } } } }
```

`wait.polls` 是本次轮询次数，可用于判断"页面反应有多慢"。

---

## 5. 能力边界（需要知道的 Chrome 限制）

| 限制 | 影响 | 缓解 |
|---|---|---|
| `chrome.debugger` attach 会显示"正在调试此浏览器"提示条 | 只要用到 CDP（snapshot / 截图 / 网络 / 控制台 / mouse-click）就会出现 | 普通 DOM 操作（exec / click / fill / send-keys）走 `scripting`，不 attach；整页/元素截图与 `save-pdf` 在被拒时回落到不 attach 的通道（`captureVisibleTab` / 拼图 / 图片型 PDF），结果里用 `via` 区分 |
| 后台标签页不渲染 | `IntersectionObserver` 懒加载、图片延迟加载、`requestAnimationFrame` 可能不触发 | 需要完整内容时先 `atb focus --tab <id>`（或 `open --focus`），或显式滚动（`scrollIntoView`）；单目标命令回包会带 `tab_hidden` / `timers_throttled` 警告与 focus 提示 |
| 后台标签页可能被冻结 | 长任务中途脚本执行被暂停 | 已用 `autoDiscardable:false` 防止丢弃；CDP 路径额外调用 `Page.setWebLifecycleState('active')` |
| 同一标签页只能被一个 debugger 占用 | 用户打开 DevTools 时 `snapshot` 走 JS 版；**可见区域**截图仍可（`captureVisibleTab`）；整页/元素截图、`network`、`console` 会失败 | 错误返回 `Another debugger is already attached`；此时改用 `exec` / `read` / `find`，或让用户关掉 DevTools |
| 页面原生弹窗 | `alert/confirm` 会阻塞页面脚本 | 扩展在 CLI 执行期间临时抑制并 toast 提示，执行完恢复 |
| daemon / 插件重启 | 执行中的命令会失去结果 | 扩展侧每条连接持自己的 socket，发送失败不再抛空指针；daemon 断开时立即让挂起命令以 `extension_disconnected` 失败（实测 66ms），不再等 15~60s 超时 |

---

## 6. 实测基线（Windows + 真实 Chrome，仅本机参考）

| 场景 | 实测 |
|---|---|
| CLI 进程启动 + 提交异步任务（`--async exec`，不含页面执行） | 260 ~ 285 ms |
| 隐藏标签页执行 `return 1`（daemon 侧 `elapsed_ms`） | 205 ~ 215 ms |
| 隐藏标签页 `--wait-js`（2 秒条件，100ms 间隔） | 约 7 次轮询、2.5 s 完成 |
| `scan --text-only`（example.com，含 CLI 启动） | ≈ 480 ms |
| 可见/隐藏标签页 `exec` 端到端 | 380 ~ 420 ms |

解读：进程启动占 60% 左右，页面执行约 200 ms 主要来自 MV3 Service Worker 唤醒 + `chrome.scripting` 调度；隐藏标签页会被 Chrome 降优先级，但**不再因为定时器节流而超时**。

---

## 7. 协议（daemon HTTP）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ping` | **无需 token**，返回 `{ok,running,pid,protocol}`；CLI 用它判断 daemon 存活 |
| POST | `/job/submit` | `{op, args}`；`args` 与对应同步接口的请求体一致 |
| GET | `/job/get/:id` | 单任务状态与结果 |
| GET | `/job/list?limit=20` | 最近任务（running 计数在顶层） |
| POST | `/job/cancel/:id` | 协作式取消 |

`/ping` 独立于鉴权是刻意设计：token 文件缺失或残留旧 daemon 时，不会再被误判为"未启动"从而重复拉起 daemon、白等 15 秒。

---

## 8. 自测

不依赖真实 Chrome 的端到端测试（假扩展）：

```bash
cargo build
node tests/mock_extension.mjs --self-test
```

覆盖：daemon 冷启动、后台打开、默认会话路由、`--async` + `job wait`、daemon 侧 `--wait-js` 轮询、`job list`、`health.jobs`、关闭标签页。

真实浏览器手测建议：

```bash
atb open https://example.com          # 观察是否切走当前标签页
atb exec 'return location.href'        # 应命中新标签页
atb --async exec 'return document.title'
atb job wait <job_id>
atb exec 'return 1' --wait-js 'return document.readyState === "complete"'
atb close --tab <opened_tab_id>
```

---

## 9. 仍未做

`open --wait-for`、按会话加锁（同一标签页串行、不同标签页并行）、`snapshot --js` 已在当前版本落地；本节只列仍未做的：

1. `job` 结果落盘（`--out`），避免 daemon 重启丢结果。
2. 后台模式的焦点模拟（`Emulation.setFocusEmulationEnabled`）与懒加载触发策略。
3. 服务端进度回调：`job tail` 当前是 CLI 轮询；`progress` 字段只有任务自己提供时才出现，没有通用进度通道。
