# Edge / Chrome 扩展加载失败排查

目标现象：

```text
Agent Browser CLI Bridge
问题 已找到 1 个
Service worker registration failed. Status code: 2
```

`Status code: 2` 是 Chromium 的「Service Worker 注册失败」，它**不告诉你原因**。
按下面的顺序逐项排除，绝大多数是前三条。

---

## 0. 先对比版本号（30 秒定位「改完没生效」）

`edge://extensions` 里本扩展卡片上显示的版本号，必须等于磁盘上 `manifest.json` 的版本。

```bash
node -e "console.log(require('<你的扩展目录>/manifest.json').version)"   # <你的扩展目录> 换成浏览器实际加载的目录
```

- **两边一致** → 浏览器跑的是当前代码，继续往下排查；
- **卡片上更旧**（例如磁盘 5.4.2、卡片 5.0.1）→ 浏览器从没重载过，`Service worker registration failed`
  多半是上一次替换文件时读到了半截 `background.js`。这种情况**改代码没用**，点一次「重新加载」即可。

### 为什么会出现「半截文件」

Chrome / Edge 重新注册 Service Worker 时会重新读 `background.js`。如果用 `cp` / 解压覆盖的方式更新目录，
而浏览器恰好在这一刻重载，就会读到写了一半的文件，注册直接失败；失败状态会一直挂在扩展页上，
后续不会自动重试，看起来就像「代码坏了」。

统一用原子部署脚本，不再有这个问题：

```bash
node scripts/deploy-extension.mjs --to "<你的扩展目录>" --dry-run   # 看计划
node scripts/deploy-extension.mjs --to "<你的扩展目录>"             # 原子替换 + 逐字节校验
```

### 已安装位置（路径按你的机器替换）

| 角色 | 目录 | 说明 |
| --- | --- | --- |
| Chrome / Edge | `<你的扩展目录>` | 两个浏览器可以共用同一份目录（实测 `Secure Preferences` 都指向它） |
| 镜像 | `<镜像目录>` | 只作备份 / 打包用，浏览器**不加载**它；不要以为改了这里就会生效 |

两个目录都保持同步（镜像用于打包），但判定「是否生效」只看浏览器实际加载的那个。想确认浏览器到底加载了哪一份：

```bash
node -e "const fs=require('fs');const base=process.env.LOCALAPPDATA;for(const [n,p] of [['Chrome',base+'/Google/Chrome/User Data/Default/Secure Preferences'],['Edge',base+'/Microsoft/Edge/User Data/Default/Secure Preferences']]){const j=JSON.parse(fs.readFileSync(p,'utf8'));for(const [id,v] of Object.entries(j.extensions.settings)){if(v&&v.path&&/chrome-extensions|atb/i.test(v.path))console.log(n,id,v.path)}}"
```

两个目录都要部署（镜像用于打包），否则会出现版本漂移：

```bash
node scripts/deploy-extension.mjs --to "<你的扩展目录>"
node scripts/deploy-extension.mjs --to "<镜像目录>"
```

脚本**没有内置默认目录**（换台机器不会在错误位置静默建一套扩展），必须用 `--to` 或 `ATB_EXT_DIR` 指定。

### 部署后什么时候生效（自动重载）

`deploy-extension.mjs` 最后会写一个 `.atb-build.json` 指纹（整包 digest：manifest + 全部 js/html/css
的 `rel:sha256` 排序哈希）。扩展每分钟自检一次这个文件，digest 变了就在空闲时
`chrome.runtime.reload()`，**改 popup / options / css / manifest 也会生效**——不再需要手动动
`background.js` 才能触发。

- 想立刻生效：`chrome://extensions` → 本扩展 →「重新加载」（最快，也不依赖 daemon）；
- 想确认到底跑的是哪版：`atb status` 看 `extension.bundled_version` 与
  `extension.connected_versions`，两边一致才是真的换了；`stale:true` 时会附一行提示。

判断是不是这一类问题的旁证：扩展页显示旧版本、`atb status` 里看不到这个浏览器（SW 没起来就不会连 daemon）、
但用干净 profile 启动同一个目录能正常加载（`CDP Bridge installed`）。

---

## 1. 确认加载的是哪个目录（最常见）

必须选择**包含 `manifest.json` 的那一层**。

```text
正确： .../assets/atb_bridge/          ← 这里有 manifest.json
错误： .../assets/                          ← 没有 manifest.json
错误： .../atb-extension/atb_bridge/    ← 解压工具多建了一层子目录
```

检查方法：把要加载的目录打开，第一眼应该能看到 `manifest.json`、`background.js`、`content.js`。

命令行确认：

```bash
ls assets/atb_bridge/manifest.json assets/atb_bridge/background.js
```

如果是从 `chrome-extensions.zip` 解压的：zip 根目录直接就是扩展文件，`manifest.json` 就在解压目录里，没有再套一层 `atb_bridge`。若解压工具自动多建了子目录，往下选到直接含 `manifest.json` 的那一层。

---

## 2. 确认 `background.js` 真的存在且没被改名

manifest 里写的是：

```json
"background": { "service_worker": "background.js" }
```

如果目录里叫 `background.js.txt`（Windows 隐藏扩展名导致）、`Background.js`（大小写敏感环境），
注册就会失败。用 `dir` / `ls -l` 看真实文件名。

---

## 3. 确认浏览器版本

`manifest.json` 里声明了 `"minimum_chrome_version": "120"`。Edge 是基于 Chromium 的，
但**版本号不一定对得上**：

- Edge 120+ → 等价 Chromium 120，可用；
- 更老的 Edge（例如 117/118）→ 缺少 `chrome.tabs.group`、`captureVisibleTab` 的某些行为，
  启动期调用可能直接抛错，表现就是 SW 注册失败。

查看：`edge://version`，第一行 `Microsoft Edge 1xx.x.x.x`。
低于 120 请升级浏览器，或改用 Chrome。

---

## 4. 看扩展页的「错误」详情

`edge://extensions` → 找到本扩展 → 点「错误」/「Errors」按钮：

| 详情 | 含义 | 处理 |
| --- | --- | --- |
| `Manifest file is missing or unreadable` | 目录选错 / JSON 损坏 | 回到第 1 步 |
| `Could not load background script 'background.js'` | 文件缺失 | 回到第 2 步 |
| `Service worker registration failed` + 一段 JS 堆栈 | 启动期脚本抛错 | 升级扩展（4.0+ 已把启动期调用包了防御），或把堆栈发出来 |
| `Cannot load extension with file or directory name _metadata` | 解压出 Mac 垃圾文件 | 删掉 `__MACOSX` / `_metadata` 后重载 |

扩展 4.0+ 会把最近一次启动错误写进 `chrome.storage.local.lastServiceWorkerError`，
在扩展的 popup 里能看到一行「最近启动错误」。

---

## 5. 企业策略 / 未打包扩展被禁

学校或公司管理的 Edge 常禁用开发者模式：

- `edge://extensions` 里「开发人员模式」开关是灰的；
- 或提示「此扩展程序已被管理员阻止」。

查看策略：`edge://policy`，搜索 `ExtensionInstallBlocklist` / `ExtensionDevelopmentMode`。
这种情况需要管理员放开，或用个人设备测试。

---

## 6. 重复加载 / ID 冲突

同一个扩展目录被加载了两次（不同路径各一份），会互相抢 WebSocket 连接，表现为
daemon 里出现两个 `browser_id`、命令随机打到没有响应的那个。

处理：`edge://extensions` 里只保留一份，删掉多余的。

---

## 7. 加载成功后仍然连不上 daemon

按顺序看：

```bash
atb status        # summary / compatibility / next_steps
atb doctor        # 逐项自检
atb logs --tail 100
```

### 扩展 5.4.2 起的行为变化

- **open shadow DOM**：`find` / `snapshot` / `read` 现在会递归 open shadow root，返回项多出 `scope` / `host_selector` / `shadow_path` 字段（这些字段 5.4.0 之前的版本就已提供）。旧版扩展不返回这些字段，daemon 仍按旧形状解析，不会报错，但拿不到 shadow 内容——这才是「明明页面上有按钮却 `find` 不到」的常见原因，先确认扩展卡片是 5.4.2。
- **exec 取消帧**：daemon 在命令超时/取消时会向扩展发 `{cmd:"cancel", execId}`。**旧 daemon 不认识这个帧也无所谓**：扩展只是不提前释放那次 CDP attach，等 idle 定时器回收即可，命令结果与错误语义都不受影响。反过来，5.4.2 扩展配旧 daemon 时也不会因为缺少 cancel 帧而失败。
- `extension_not_connected`：端口不一致（默认 18765）→ `set-extension-port` 或改扩展 popup 里的端口；
- `compatibility.ok:false` 且 `issues[].kind=extension_outdated`：浏览器里跑的是旧版扩展文件，
  覆盖目录后重新加载；
- 刚重启 daemon：扩展重连最长约 30-60 秒（MV3 alarm 下限），稍等再试。

---

## 8. Edge 下的 `chrome-extension://` 页面与扩展列表

### 8.1 打开扩展页面

`atb open` 支持 `chrome-extension://<扩展ID>/<路径>`，但这是**浏览器级限制**：

- 只有本工具自己的扩展页面（例如 `chrome-extension://<ATB ID>/options.html`）可以可靠打开；
- 打开别的扩展页面会被 Chrome / Edge 拒绝，回 `special_scheme_open_failed`（错误里会说明是浏览器限制）；
- 页面里内嵌了**其它扩展**的 iframe 时，`exec` / `screenshot` / `network` 等 CDP 路径仍可能 `page_protected`（crbug 40753571），换 `read` / `scan` / `mouse-click` 或临时禁用挡路扩展。

扩展 ID 在 `edge://extensions`（Chrome 为 `chrome://extensions`）的本扩展卡片上；`atb status` / `atb tabs` 的 `browser_id` 不是扩展 ID，不要混用。

### 8.2 `atb extensions` 报 `permission_required`

`management` 是 **optional 权限**，manifest 不会在安装时自动授予，CLI 也不会替用户点授权：

1. 打开扩展管理页 →「关于」→「高级权限」；
2. 点「授权读取扩展列表」，在浏览器弹窗里确认；
3. 重试 `atb extensions`；成功回包 `permission_granted:true`。

排障：

| 现象 | 原因 / 处理 |
| --- | --- |
| 授权按钮点了没反应 / 提示不支持 | Edge 企业策略禁用了 `chrome.permissions` 或 optional 权限申请；请管理员在 `edge://policy` 放开，或换个人设备。扩展详情页不能代替 optional 权限授权 |
| 仍报 `permission_required` | 授权被拒绝或策略覆盖；`atb extensions` 不会自动降级成「只列本工具」，这是刻意行为 |
| 报 `extension_outdated` | 浏览器里跑的扩展还没有 `listExtensions`：用 `assets/atb_bridge` 重新部署并在 `edge://extensions` 重新加载 |
| 列表为空但确实装了扩展 | 默认只列**启用**的扩展；加 `--include-disabled` 再试 |
| 列表里有名字/版本为空的项 | 扩展自身没有提供元数据；不是 atb 读取失败，按 `id` 在 `edge://extensions` 定位 |

`atb extensions` 只读（`chrome.management.getAll`），没有启用 / 停用 / 卸载能力。不想保留授权时，在同一「高级权限」区撤销即可；撤销后只有 `atb extensions` 失败，其它命令不受影响。

---

## 9. 仍然失败时，请提供这些信息

1. `edge://version` 第一行；
2. `edge://extensions` 里该扩展的「错误」详情全文；
3. 加载的目录路径（`dir` 输出前 10 行）；
4. `atb status --pretty` 输出；
5. daemon 日志最后 50 行（`atb logs --tail 50`）。
