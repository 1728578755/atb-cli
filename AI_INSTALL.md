# AI 安装手册

适用对象：希望由编码 Agent 在本机完成 atb 安装、扩展加载、skill 安装与验证的用户。

当前文档对应 CLI `0.8.0`、扩展文件版本 `5.5.0`、CLI ↔ daemon 协议 `5`。版本号以 `package.json` 与 `assets/atb_bridge/manifest.json` 为准；CLI 与扩展独立发版，不要假设两者同号。用 `@latest` 安装时如果装到更新的版本，以 `atb --version` 与 `atb status` 的输出为准。

## 复制给 AI 的一段话

```text
请在这台机器上安装或升级 atb（仓库 https://github.com/1728578755/atb-cli，npm 包 @g1728578755/atb）。

目标：安装最新版 atb（本文写作时为 CLI 0.8.0、扩展 5.5.0），最低 Chrome/Edge 120，Node >= 18。若实际版本更新，以命令输出为准。

执行要求：
1. 先做前置检查并告诉我结果：操作系统与 CPU 架构、node -v、npm -v、Chrome/Edge 版本、PATH 上是否已有 atb 及其版本、~/.atb 是否已存在。
2. 安装 CLI 优先用 npm install -g @g1728578755/atb@latest；只有预编译包缺失或安装失败时才回退到源码构建 cargo build --release，并用 ATB_BIN 指向产物。
3. 安装扩展前先问我：解压目录放在哪里。从 GitHub 最新 Release 下载 chrome-extensions.zip，解压后扩展文件直接位于该目录（manifest.json、background.js、content.js 在同一层，没有再套一层 atb_bridge）。指导我在 chrome://extensions 开启开发者模式、加载已解压的扩展程序，选择这个直接包含 manifest.json 的目录。如果之前装过，必须在扩展卡片上点“重新加载”。
4. 安装 skill 前先问我是否需要安装、装给哪些 Agent。先运行 atb install-skill --dry-run，把输出原样贴给我，我确认后再执行 atb install-skill；不要用 --yes 跳过确认。
5. 验证依次运行：atb tabs --pretty、atb status --pretty、atb doctor、atb env --pretty、atb open https://example.com --pretty、atb read --tab <tabId> --pretty。先跑 tabs 是为了自动拉起 daemon；直接先跑 status 会得到 daemon_not_running，这不是安装失败。把完整 JSON 贴给我。成功判据：tabs 的 count 大于 0；status 的 healthy 为 true 且 summary 为 ready；env 的 browser_label 非空且 tabs.total 大于 0；open 返回 targets[0].tab_id；read 返回非空 title 或 text。
6. 任何一步失败：先停下，贴出完整命令、退出码、JSON 错误与 daemon 日志末尾，再给出下一步方案，等我确认后再动手。统一使用 atb，不要与旧命令 agent-browser-cli 混用。
```

## AI 执行流程

每一步拿到输出再进入下一步；需要用户操作浏览器时，停下来等用户确认，不要假定“应该已经加载好了”。

### 0. 前置检查

```bash
node -v
npm -v
node -p "process.platform + '-' + process.arch + ' ' + (process.platform !== 'linux' ? 'n/a' : (process.report.getReport().header.glibcVersionRuntime ? 'glibc' : 'musl'))"
atb --version 2>/dev/null || echo "atb 未安装"
```

浏览器版本在 `chrome://version` 或 `edge://version` 第一行读取，需 `>= 120`。`~/.atb` 已存在说明装过旧版，记录 `atb --version`；如果 daemon 正在运行，再记 `atb status` 的 `summary`，然后升级。

WSL 额外确认：WSL `2.0.0+`、Windows 11 22H2+、`.wslconfig` 中 `networkingMode=mirrored`。不满足时，浏览器桥接可能连不上。

### 1. 安装 CLI

```bash
npm install -g @g1728578755/atb@latest
atb --version
```

`atb --version` 应输出 `atb 0.8.0`（或你实际安装的更新版本）。npm 安装会按平台拉取可选依赖子包；平台清单：

| 平台 | 预编译 | 说明 |
| --- | --- | --- |
| macOS arm64 / x64、Linux glibc x64 / arm64、Windows x64 | 有 | 分别拉取对应的 `@g1728578755/atb-<platform>-<arch>` |
| WSL 2 | 有 | 用 Linux 包（linux-x64 / arm64），需要 mirrored 网络连宿主机 Chrome |
| Windows ARM64 | 无 | 需自编译，或改用 x64 Node 走系统仿真 |
| Alpine / musl | 无 | 需自编译，或换 glibc 基础镜像 |

不要用 `--ignore-optional` / `--no-optional`，否则原生二进制不会安装。macOS、Linux 上全局安装报 `EACCES` 时，改用 nvm 或把 npm 前缀设到用户目录，不要直接 `sudo npm install`。

预编译包缺失时改用源码：

```bash
git clone https://github.com/1728578755/atb-cli.git
cd atb
cargo build --release
export ATB_BIN="$PWD/target/release/atb"
```

Windows PowerShell 用 `$env:ATB_BIN = "$PWD\target\release\atb.exe"`。`ATB_BIN` 只对当前 shell 生效，需要长期使用就写进 shell 配置。

### 2. 加载扩展

先问用户解压到哪个目录，例如 `~/atb-extension`。不要替用户决定路径。

macOS / Linux：

```bash
EXT_PARENT="$HOME/atb-extension"
mkdir -p "$EXT_PARENT"
ZIP_URL="$(curl -fsSL https://api.github.com/repos/1728578755/atb-cli/releases/latest | grep -o '"browser_download_url": "[^"]*chrome-extensions.zip"' | head -n 1 | cut -d '"' -f 4)"
curl -fL "$ZIP_URL" -o "$EXT_PARENT/chrome-extensions.zip"
unzip -o "$EXT_PARENT/chrome-extensions.zip" -d "$EXT_PARENT"
```

没有 `unzip` 的 Linux 先装 `unzip`，或用 `python3 -m zipfile -e "$EXT_PARENT/chrome-extensions.zip" "$EXT_PARENT"`。

Windows PowerShell：

```powershell
$ExtParent = "$env:USERPROFILE\atb-extension"; New-Item -ItemType Directory -Force $ExtParent | Out-Null
$release = Invoke-RestMethod https://api.github.com/repos/1728578755/atb-cli/releases/latest
$zip = ($release.assets | Where-Object name -eq 'chrome-extensions.zip').browser_download_url
Invoke-WebRequest $zip -OutFile "$ExtParent\chrome-extensions.zip"
Expand-Archive "$ExtParent\chrome-extensions.zip" -DestinationPath $ExtParent -Force
```

解压后确认目录结构（macOS / Linux）：

```bash
ls "$EXT_PARENT/manifest.json" "$EXT_PARENT/background.js" "$EXT_PARENT/content.js"
node -p "require('$EXT_PARENT/manifest.json').version"
```

第二行应为 `5.5.0`；Windows 用 `Get-Content "$ExtParent\manifest.json"` 查看 `version` 字段。加载时选择 `$EXT_PARENT` 这一层：它必须直接包含 `manifest.json`、`background.js`、`content.js`。解压工具若自动多建了一层子目录，就往下选到直接含 `manifest.json` 的那一层，不要把父目录交给浏览器。

在 Chrome 打开 `chrome://extensions`（Edge 为 `edge://extensions`），开启“开发者模式”，点“加载已解压的扩展程序”，选择该目录。已装过旧版的：覆盖目录后回到扩展页点“重新加载”。仅覆盖文件不重载，浏览器仍跑旧 Service Worker。加载完成后至少打开一个 `http://` 或 `https://` 页面；停在 `about:blank`、`chrome://` 页面时扩展不会上报可用标签页。

如果要用 `atb extensions` 列出已安装扩展，先打开扩展管理页的「关于 → 高级权限」，由用户点击授权 `management`。这是**可选权限**，不授权不影响其他命令；未授权时 `atb extensions` 返回 `permission_required`，而不是安装失败。

### 3. 安装 skill

`atb install-skill` 的来源是 CLI 包内置的 `skills/atb`，不联网、不从 GitHub 拉取。先展示计划，把输出交给用户确认后再执行：

```bash
atb install-skill --dry-run
atb install-skill
```

实体目录是 `~/.agents/skills/atb`，每次安装都会整体更新该目录；如果里面有你改过的内容，先备份。对已存在的 `~/.codex/skills`、`~/.claude/skills`、`~/.config/agents/skills`、`~/.cursor/skills`、`~/.gemini/skills` 建 `atb` 软链接，不复制多份实体文件。这五个位置若已有**实体目录**（不是软链接），会跳过并提示手动处理，`--yes` 也不会覆盖。`--yes` 只用于脚本化安装，不要默认加。

WorkBuddy 的 `~/.workbuddy/skills/atb` 不在写入范围内，需要手动同步。npm 全局安装时：

```bash
SKILL_SRC="$(npm root -g)/@g1728578755/atb/skills/atb"
mkdir -p ~/.workbuddy/skills/atb
cp -r "$SKILL_SRC/." ~/.workbuddy/skills/atb/
```

Windows PowerShell 用 `$SkillSrc = Join-Path (npm root -g) "@g1728578755\atb\skills\atb"`，再用 `Copy-Item "$SkillSrc\*" "$env:USERPROFILE\.workbuddy\skills\atb" -Recurse -Force`。

### 4. 验证

```bash
atb tabs --pretty         # 先跑：自动拉起 daemon，并确认扩展桥
atb status --pretty       # 再做健康检查
atb doctor
atb env --pretty          # 浏览器 / Profile / 扩展 / 标签 / 持久注入一次看
atb open https://example.com --pretty
atb read --tab <tabId> --pretty
```

首次安装后不要先跑 `status`：它只读、不会拉起 daemon，会返回 `daemon_not_running`。先跑 `tabs` 或 `open`，再看 `status`。

| 命令 | 成功判据 |
| --- | --- |
| `status` | `ok:true`、`healthy:true`、`summary:"ready"`、`compatibility.ok:true`；扩展版本用 `atb capabilities` 核对为 `5.5.0` |
| `env` | `ok:true`，`browser` / `browser_label` 非空，`tabs.total` 大于 0；`warnings` 为空或只含可解释项 |
| `doctor` | 各检查项无报错；有可修复项时按输出建议处理 |
| `tabs` | `ok:true`，`count` 大于 0 |
| `open` | `ok:true`，`targets[0].ok:true` 且有 `tab_id` |
| `read` | `ok:true`，`title` 或 `text` 非空 |

`status` 的退出码跟随 `healthy`，不健康时为非 0。脚本里不要用 `atb status && ...` 判断，直接读 JSON 的 `summary`；需要临时恢复恒 0 行为时设 `ATB_EXIT0=1`。`doctor` 只诊断并打印修复建议，加 `--fix` 会尝试修正（例如清理旧 daemon 占用的 `18767`），终止进程需要再加 `--force`。

### 5. 升级

CLI：

```bash
atb stop
npm install -g @g1728578755/atb@latest
atb --version
atb restart
```

CLI ↔ daemon 协议为 `5`。换二进制后必须 `atb restart`，否则新 CLI 会拒绝连接旧 daemon，`status` 的 `compatibility` 会报不配套。`doctor --fix` 能定位占用 `18767` 的进程，`doctor --fix --force` 才会实际结束它。

扩展：重新下载 `chrome-extensions.zip`，解压到同一目录并覆盖旧文件，回 `chrome://extensions` 点“重新加载”，再对比卡片版本与磁盘 `manifest.json` 的 `5.5.0`，不一致就再重载一次。

skill：

```bash
atb install-skill --dry-run   # 先确认计划
atb install-skill
```

`--dry-run` 显示 `update_main` 时，表示 `~/.agents/skills/atb` 会被整体更新；如果该目录有你自己的修改，先备份再执行。显示 `skip_existing_entity` 的是五个 Agent 目录下的同名实体目录，需要你手动移走或改造成软链接。WorkBuddy 用户按第 3 节再同步一次。升级后重跑第 4 节全部验证命令。

## 失败分支

| 症状 | 原因 | 处理 |
| --- | --- | --- |
| 运行 `atb` 报“原生二进制缺失（可选依赖 …）” | 平台子包未装或被 `--ignore-optional` 跳过 | `npm install -g @g1728578755/atb@latest --force`；仍失败则源码构建并设 `ATB_BIN` |
| 报“未提供 linux-x64 / darwin-arm64 的预编译包” | 平台不在预编译矩阵内 | 按提示执行 `cargo build --release`，用 `ATB_BIN` 指向 `target/release/atb` |
| Alpine 上安装后无法启动或提示缺少预编译包 | musl libc 与 glibc 预编译包不兼容 | 源码编译，或改用 `node:20-bookworm-slim` 一类 glibc 镜像 |
| Windows ARM64 安装后无二进制 | 未发布 `win32-arm64` 包 | 改装 x64 版 Node 走系统仿真，或源码编译后设 `ATB_BIN` |
| 扩展页报 `Service worker registration failed. Status code: 2` | 加载了错的目录层、文件名异常、浏览器低于 120，或覆盖文件时读到半截 `background.js`；zip 带 `__MACOSX` / `_metadata` 也会失败 | 确认选的是含 `manifest.json` 的目录；看扩展“错误”详情；升级浏览器；删掉垃圾目录；点“重新加载” |
| 端口被占，`status` 连不上或 daemon 起不来 | `18765` 被其它进程占用，或旧 daemon 仍占 `18767` | `atb doctor --fix` 看 PID；`atb set-extension-port <port>` 换端口并在扩展 popup 填同一端口；旧 daemon 用 `atb restart --force` |
| `status` 的 `compatibility.ok:false`，提示 daemon 协议不配套 | 升级 CLI 后旧 daemon 仍在跑（协议 4 对 5） | `atb stop` 后 `atb restart`；仍失败看 `atb logs --tail 50` |
| `extension_not_connected` / `count` 为 0 | 没有 http/https 页面、扩展未重载、端口不一致 | 打开一个正常网页；重载扩展；核对 `18765`；daemon 刚重启时等 30-60 秒重连 |
| WorkBuddy 里 skill 不生效或仍是旧内容 | `atb install-skill` 不写 `~/.workbuddy/skills` | 按第 3 节手动复制，重启 WorkBuddy 会话，检查 `SKILL.md` 中的扩展版本为 `5.5.0` |

失败时先收集这些信息再改配置：`atb status --pretty`、`atb doctor`、`atb logs --tail 50`、扩展页错误详情全文、`chrome://version` 第一行。

## 人工安装

不经过 AI 的最短路径：

1. 安装 Node 18+ 和 Chrome 120+。
2. 执行 `npm install -g @g1728578755/atb@latest`，确认 `atb --version` 为 `0.8.0` 或更新版本。
3. 到 https://github.com/1728578755/atb-cli/releases/latest 下载 `chrome-extensions.zip`，解压到固定目录。
4. 打开 `chrome://extensions`，开开发者模式，加载解压后的目录（该目录直接含 `manifest.json`），再打开任意一个网页。
5. 执行 `atb install-skill --dry-run` 确认计划后执行 `atb install-skill`，最后用 `atb tabs`、`atb status`、`atb doctor` 验证（先 `tabs` 拉起 daemon）。

首次使用看 [README.md](README.md)，Agent 操作规范看 [skills/atb/SKILL.md](skills/atb/SKILL.md)。

## 安装清单

- [ ] Node `>=18`、Chrome / Edge `>=120`
- [ ] `atb --version` 输出 `0.8.0` 或更新版本
- [ ] `chrome-extensions.zip` 已解压，解压目录下的 `manifest.json` 版本为 `5.5.0`，且与 `background.js` 同层
- [ ] 扩展已加载，卡片版本与磁盘一致
- [ ] 至少一个 http/https 页面处于打开状态
- [ ] `atb tabs` 的 `count` 大于 0（同时拉起 daemon），`atb open` 返回 `tab_id`，`atb read` 返回非空标题或正文
- [ ] `atb env` 的 `browser_label` 非空、`tabs.total` 大于 0
- [ ] `atb status` 的 `healthy` 为 `true`、`summary` 为 `ready`（在 `tabs` 之后跑）
- [ ] `atb doctor` 无未处理失败项
- [ ] `~/.agents/skills/atb/SKILL.md` 存在；WorkBuddy 用户已手动同步

## 卸载与回滚

停止常驻进程并卸载 CLI：

```bash
atb stop
npm uninstall -g @g1728578755/atb
```

`~/.atb` 下的 `config.json`（扩展端口）、`api-token`（本地 token）、`ws-trust.json`（扩展信任）、`net-hooks.json`（网络规则）、`agent-tabs.json`（标签页归属）、`logs/daemon.log`（日志）都可删；整目录删除等于重置配置。

```bash
rm -rf ~/.atb
```

移除扩展：`chrome://extensions` 找到 ATB，点“移除”。同时把解压目录一并删掉。

移除 skill（只删软链接，`~/.codex/skills/atb` 等位置若是实体目录则保留）：

```bash
rm -rf ~/.agents/skills/atb
rm -f ~/.codex/skills/atb ~/.claude/skills/atb ~/.config/agents/skills/atb ~/.cursor/skills/atb ~/.gemini/skills/atb
rm -rf ~/.workbuddy/skills/atb
```

回滚：在 `0.7.x` 内回退只需 `npm install -g @g1728578755/atb@<version>` 后 `atb restart`，同一 `5.5.0` 扩展通常可继续使用；若 `atb status` 报协议或扩展不配套，再换回该版本配套的扩展目录与 skill。跨到 `0.6.0` 之前的大版本回滚，必须同时换回该版本配套的扩展目录与 skill。

相关文档：[README.md](README.md)、[CHANGELOG.md](CHANGELOG.md)、[skills/atb/SKILL.md](skills/atb/SKILL.md)、[SECURITY.md](SECURITY.md)、[docs/EDGE_TROUBLESHOOTING.md](docs/EDGE_TROUBLESHOOTING.md)、[docs/BEST_PRACTICES.md](docs/BEST_PRACTICES.md)。
