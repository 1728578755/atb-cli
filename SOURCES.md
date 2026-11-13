# 来源与依赖

本文件说明 `atb` 的代码来源、第三方依赖与许可边界，供审计和再分发时核对。

## 三类来源

| 类别 | 内容 | 位置 |
| --- | --- | --- |
| 原创 | Rust daemon、CLI 命令面、协议与错误契约、构建发布脚本 | `src/`、`scripts/`、`.github/` |
| 改造自上游 | 浏览器桥接、页面简化、扩展雏形 | `assets/atb_bridge/`、`assets/page_*.js`、`assets/simphtml_*.js` |
| 第三方依赖 | Rust crates | `Cargo.toml` |

## 上游项目

浏览器控制能力提取并改造自 GenericAgent：<https://github.com/lsdefine/GenericAgent>。

沿用的思路与实现：

- `TMWebDriver`：扩展与本地进程之间的桥接、CDP 控制的组织方式。
- `simphtml`：页面简化与正文/列表识别的思路。
- `atb_bridge`：Chrome 扩展的雏形与消息形态。

本仓库做的改造：

- CLI 化：一条命令一个进程，输出 compact JSON，带稳定 `error_code` 与退出码。
- daemon 用 Rust 实现，不依赖 Python 运行时。
- 多标签、多智能体隔离（`--agent`、`agent-tabs.json`）。
- 协议版本化（CLI↔daemon API `5`、扩展消息 `4`）与兼容性检查。
- 端口配置、`api-token`、`trust-extension` 等本地信任机制。
- 平台打包、npm 分发与 CI。

上游许可以其仓库声明为准，本文件不做转述。上游出处标注尚未在代码中落地（关于页、命令输出与派生文件头均未写入出处字符串），已列入未来工作；当前仓库内的派生文件没有内嵌上游版权头，再分发或大幅改造这些文件前，请先核对上游仓库的许可与署名要求。

## Rust 依赖

| crate | 用途 | 许可 |
| --- | --- | --- |
| anyhow | 错误上下文与错误链 | MIT OR Apache-2.0 |
| axum | 本地 HTTP API 与 WebSocket 服务 | MIT |
| clap | 命令行解析（derive） | MIT OR Apache-2.0 |
| dirs | 跨平台配置目录 | MIT OR Apache-2.0 |
| dunce | Windows 路径规范化 | C .0 OR MIT-0 OR Apache-2.0 |
| fs2 | 文件锁，保护配置原子写 | MIT OR Apache-2.0 |
| html5ever | HTML 解析 | MIT OR Apache-2.0 |
| libc | Unix 系统调用与信号 | MIT OR Apache-2.0 |
| markup5ever_rcdom | DOM 树，供页面简化使用 | MIT OR Apache-2.0 |
| path-clean | 路径规范化 | MIT OR Apache-2.0 |
| reqwest | HTTP 客户端（`rustls-tls`，无 OpenSSL） | MIT OR Apache-2.0 |
| serde | 序列化框架 | MIT OR Apache-2.0 |
| serde_json | JSON 读写 | MIT OR Apache-2.0 |
| tokio | 异步运行时 | MIT |
| url | URL 解析 | MIT OR Apache-2.0 |
| uuid | 生成 v4 标识 | Apache-2.0 OR MIT |
| windows-sys | Windows 文件系统与原子替换 | MIT OR Apache-2.0 |

`windows-sys` 仅 Windows 目标启用。许可取自各 crate 的发布元数据，升级版本后可能变化，复核时以 crate 自身声明为准。

`Cargo.lock` 固定了传递依赖的具体版本；完整依赖树以该文件为准。

## 构建与发布

| 脚本 | 作用 |
| --- | --- |
| `scripts/sync-version.mjs` | 以 `package.json` 为准，同步 `Cargo.toml`、README 版本行与徽章 |
| `scripts/sync-version.mjs` | 同步版本锚点、可选依赖与锁文件条目 |
| `scripts/sync-extension-assets.mjs` | 同步扩展与页面脚本副本；`--check` 只校验 |
| `scripts/package-platform.mjs` | 生成单平台 npm 包 |
| `scripts/deploy-extension.mjs` | 把扩展部署到浏览器加载目录，逐文件原子替换并校验 |

GitHub Actions：

- `ci.yml`：Rust 构建与测试、扩展语法检查、Node 测试、端到端测试、仓库卫生检查。
- `publish.yml`：push `v*` 标签或手动触发，矩阵构建 5 个平台包（macOS arm64/x64、Linux glibc x64/arm64、Windows x64），随后发布主包。

npm 发布开启 provenance（`publishConfig.provenance = true`），主包通过 `optionalDependencies` 拉取对应平台包。

## 扩展与页面脚本

- `assets/atb_bridge/`：MV3 扩展，`manifest.json` 是扩展版本的唯一真相源。
- `assets/page_*.js`：页面脚本的唯一真源，直接修改这里。
- `assets/atb_bridge/ops/`：页面脚本的构建副本，由 `sync-extension-assets.mjs` 生成，不要手改。
- `assets/simphtml_*.js`：改造自上游的页面简化脚本。

## 素材

`assets/atb_bridge/icons/atb-*.png`（16/32/48/128，源文件 `atb.svg`）是本项目自绘的几何图标（渐变方底 + 浏览器窗口 + 代理光标），随仓库以 MIT 分发。替换图标时保持这四个尺寸与文件名，`manifest.json` 的 `icons` / `action.default_icon` 要同步。

## 规范参考

- `CHANGELOG.md` 的结构参考 Keep a Changelog。
- 版本号遵循 Semantic Versioning。
- 行为准则参考 Contributor Covenant 2.1。

这些是写作与流程参考，不改变本项目自身的许可。

## 许可证边界

- 本项目以 MIT 许可发布，Copyright (c) 2026 1728578755 / sleepinginsummer / lsdefine，全文见 [LICENSE](./LICENSE)。
- 上游项目与第三方 crate 各自保留其许可，不因本项目采用 MIT 而改变。
- 再分发二进制或源码时，保留 `LICENSE`、上游版权声明与依赖许可文本；如需对外分发，建议同时生成第三方许可清单。
- 修改派生自上游的文件时，不要移除已有的版权或来源注释；如果上游许可要求署名，在该文件顶部补上。

## 维护方式

新增或升级依赖、扩大上游派生范围、替换素材、变更许可证时，同步更新本文件。

CI 不校验本文件内容，由维护者人工确认。

## 相关文档

- 隐私与本地数据：[PRIVACY.md](./PRIVACY.md)
- 安全边界与威胁模型：[SECURITY.md](./SECURITY.md)
- 贡献流程：[CONTRIBUTING.md](./CONTRIBUTING.md)
