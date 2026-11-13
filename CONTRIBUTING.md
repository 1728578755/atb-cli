# 贡献指南

面向要改 atb 代码或文档的人。动手前先读完本页。

atb 由三部分组成：Rust CLI、Rust daemon、Chrome MV3 扩展。一次改动经常横跨两边，先在下面找到对应的层。

## 先判断改哪一层

| 你要改的 | 位置 | 怎么验证 |
| --- | --- | --- |
| CLI 参数、输出、退出码 | `src/cli/` | `cargo test`；手工跑对应子命令 |
| daemon 行为、标签页、网络 | `src/server/` | `cargo test`；`node tests/mock_extension.mjs --self-test` |
| 扩展后台、注入、弹窗 | `assets/atb_bridge/` | `node --check`；浏览器里重载扩展 |
| 页面脚本 | `assets/page_*.js` | `node tests/check_page_script.mjs`；`npm run sync:extension` |
| 文档 | 根目录 `*.md`、`skills/`、`docs/` | 相对链接可达；版本锚点逐字存在 |

只改一层时不要顺手改另一层。跨层改动在 PR 描述里写清联动关系。

## 环境准备

- Rust stable，带 `rustfmt` 与 `clippy`：`rustup component add rustfmt clippy`。
- Node >= 18。CI 用 Node 24，本地结论以 CI 为准。
- Chrome 120 或更高，扩展用到较新的 MV3 API。
- 改扩展时在 `chrome://extensions` 打开开发者模式，用「加载已解压的扩展程序」指向 `assets/atb_bridge`。

首次构建：

```bash
cargo build
```

## 仓库地图

| 路径 | 内容 |
| --- | --- |
| `src/cli/` | 参数解析、输出、退出码、HTTP 客户端、各子命令 |
| `src/server/` | daemon：标签页、读取、动作、网络、挑战、生命周期、目标富化（`target_info.rs`） |
| `src/server/focus.rs` / `env.rs` | `focus` / `use` 与 `extensions` 的 daemon 侧实现 |
| `assets/page_*.js` | 注入页面的脚本，daemon 通过 `include_str!` 打包 |
| `assets/atb_bridge/` | MV3 扩展；`ops/` 是页面脚本的生成副本 |
| `npm/` | npm 包装，`bin/atb.js` 负责定位平台二进制 |
| `scripts/` | 版本同步、扩展资源同步、打包、发布辅助 |
| `skills/` | 随 npm 分发的 Agent 技能 |
| `tests/` | Node 测试、契约测试、假扩展 E2E |
| `docs/` | 深入文档与历史记录 |

`src/protocol.rs` 定义两个协议版本常量（CLI↔daemon 为 5、扩展为 4）与共享 DTO；请求/响应的具体形状在 `src/server/dto.rs` 与各领域模块里。当前顶层命令 61 个；改协议之前先看「不能破坏的契约」。

## 开发循环

CLI 与 daemon：

```bash
cargo build
cargo run -- status
cargo run -- tabs
```

daemon 由 CLI 按需拉起：`tabs`、`open`、`exec`、`scan` 会自动启动。改过 daemon 代码后先 `cargo run -- stop`，再跑下一条命令，确认新进程生效。日志在 `~/.atb/logs/daemon.log`。

扩展：改 `assets/atb_bridge/` 下的文件后，先用 `node scripts/deploy-extension.mjs --to "<你的扩展目录>"` 原子替换（逐字节校验），再到 `chrome://extensions` 点该扩展的「重新加载」。后台脚本不重载不生效；具体目录见 `docs/EDGE_TROUBLESHOOTING.md`。

页面脚本：只改 `assets/page_*.js`，然后执行：

```bash
npm run sync:extension
```

`assets/atb_bridge/ops/` 是生成目录，直接改会在 `--check` 时失败。

文档：改完检查相对链接和版本锚点，规则见「文档规则」。

## 测试矩阵

| 命令 | 覆盖什么 | 什么时候跑 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | Rust 格式 | 提交前、CI |
| `cargo clippy --all-targets --all-features -- -D warnings` | 静态检查，告警即失败 | 提交前、CI |
| `cargo check --all-targets --locked` | 全 target 可编译，锁文件与 `Cargo.toml` 同步 | 提交前、CI |
| `cargo test --all-targets --all-features --locked` | Rust 单元测试 | 提交前、CI |
| `cargo build --release --locked`（`npm run build` 的 cargo 部分） | release profile 可链接（LTO / panic=abort 等差异） | 发布前、CI |
| 对 `assets/atb_bridge` 下全部 `.js` 逐个 `node --check`（含 `options.js` 与 `ui/**`） | 扩展脚本语法 | 改扩展后、CI |
| `node tests/check_page_script.mjs` | 页面脚本可解析、入口齐全 | 改页面脚本后、CI |
| `npm test`（等价 `node --test tests/*.test.mjs`） | 页面脚本与接口契约回归 | 提交前、CI |
| `node tests/mock_extension.mjs --self-test` | 真 daemon + 假扩展端到端 | 改 daemon 或协议后、CI |
| `node scripts/e2e.mjs`（CI 只跑 `ATB_E2E=smoke`） | 真 Chrome + 真扩展场景 | 仅 CI Linux（xvfb）；本地手动全量 |
| `node scripts/sync-version.mjs --check` | 版本号在各处一致 | 改版本后、CI |
| `node scripts/sync-extension-assets.mjs --check` | `ops/` 与主副本逐字节一致 | 改页面脚本后、CI |
| `npm run check:lock`（`node scripts/check-lock.mjs`） | `Cargo.lock` / `package-lock.json` 与清单一致 | 改依赖或版本后、CI |
| `npm ci --dry-run --ignore-scripts` | `package-lock.json` 与 `package.json` 一致 | 改依赖后、CI |

`npm test` 只收 `*.test.mjs`。`live_*` 需要真浏览器，`*_bench` 是压测，都不在 CI 里跑。

E2E 占用 `18766` 与 `18767` 端口，必须串行执行。并行会互相抢端口，产生假红或假绿。

## 不能破坏的契约

| 契约 | 内容 |
| --- | --- |
| 退出码 | `0` 成功、`1` 执行失败、`2` 命令写错、`3` `job wait` / `job result` / `download-wait` 超时、`4` 结果未知、`130` 取消 |
| `error_code` | 已有取值不改名、不改语义；新增时配 `hint` |
| JSON 字段 | 只增不改。删字段、改类型、改含义都算破坏 |
| CLI ↔ daemon 协议 | 当前 `5`。不兼容变更要两端同时改并升版本 |
| 扩展消息协议 | 当前 `4`。同上 |
| 新增命令（0.8.0） | `focus` / `use` / `env` / `extensions` / `measure` / `userscript` 共 6 个顶层命令；`job tail` / `job result` 是子命令。新增命令必须同时进 `args.rs`、`cmd_schema.rs`、`README` 命令表与 `skills/atb/references/commands.md` |
| `resolved_target` | 单目标页面命令顶层字段只增不改（`tab_id` / `session_key` / `url` / `title` / `browser_id` / `profile_id` / `browser` / `profile` / `active` / `visible`）；`/open` `/multi` `/run` `/batch` `/scrape` `/broadcast` 按设计不富化 |
| 本轮新增 / 明确的 `error_code` | `target_ambiguous`（exit 2）、`permission_required` / `extension_outdated` / `focus_failed` / `inject_script_threw` / `exec_result_not_found` / `special_scheme_open_failed` / `daemon_outdated`（exit 1）、`unsupported_selector`（exit 2）；全部带 `hint` |
| optional 权限 | `management` 只能放 `optional_permissions`，由用户在扩展设置页授权；未授权时 `atb extensions` 返回 `permission_required`，不得静默降级 |
| inject TTL | 新 daemon 的 `--persist` 默认 7200s；旧 daemon 不传 TTL 字段时扩展保持旧行为「永久」；永久必须显式 `--persist-forever --i-know-this-is-long-lived` |

`status` 的 `ok` 跟随 `healthy`；`ATB_EXIT0=1` 保留旧的「恒 0」行为。动这块前先确认不影响现有脚本。

## 扩展与页面脚本单一真源

- 页面脚本的真相源是 `assets/page_*.js`。扩展里的 `ops/` 是副本，只由脚本生成。
- 扩展本体只维护 `assets/atb_bridge/` 一份，不要另存副本。
- 扩展版本以 `assets/atb_bridge/manifest.json` 为准，独立于 CLI 版本。

提交前跑 `npm run check:extension`，它等价于 `node scripts/sync-extension-assets.mjs --check`。

## 提交规范

```
type(scope): summary
```

- type：`feat` / `fix` / `docs` / `refactor` / `test` / `chore`。
- scope：`cli` / `server` / `extension` / `skill` / `docs`。
- summary 用祈使句，不超过 72 字符，末尾不加句号。
- 一个提交只做一件事。格式化与逻辑改动分开提交。

示例：

```
fix(server): keep paused fetch request resumable
docs(skill): rewrite failure handling table
```

## PR 清单

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 通过
- [ ] `cargo test --all-targets --all-features --locked` 通过
- [ ] `npm test` 通过
- [ ] `node scripts/sync-version.mjs --check` 通过
- [ ] `npm run check:lock` 通过
- [ ] 改过页面脚本时，`npm run sync:extension` 已执行且 `npm run check:extension` 通过
- [ ] 文档里的版本锚点逐字未变，或已随版本一起更新
- [ ] 中英 README 同步：改一边就改另一边
- [ ] 行为、命令、契约、扩展或构建流程有变化时，相关文档与测试已同步更新

PR 描述写清三件事：改了哪一层、为什么改、怎么验证。输出或行为有变化时贴前后对比。

## 版本与发布

- 版本唯一真相源是 `package.json`。
- 改版本后跑 `npm run sync-version`，它按 [`scripts/version-docs.json`](./scripts/version-docs.json) 的落点清单改写 `Cargo.toml`、`README.md`、`README_EN.md` 等当前真相文档，并 `git add`；新增“当前版本”文档时必须把落点加进清单，否则守卫会空转。
- 平台包清单必须四处一致：`package.json` 的 `optionalDependencies`、`scripts/platforms.json`、`.github/workflows/publish.yml` 的 matrix（由 `scripts/platforms.mjs` 生成）、`npm/bin/atb.js` 的平台解析。
- 扩展版本独立维护：改 `assets/atb_bridge/manifest.json`，同步文档里的版本断言。
- 发布由 push tag `v*` 触发，也可手动 dispatch。workflow 构建 5 个平台包后发布 npm。

`node scripts/sync-version.mjs` 同时校验 `package-lock.json` 与 `Cargo.lock` 的根包版本。锁文件不由它写入：前者用 `node scripts/sync-version.mjs --deps`，后者交给 cargo。

## 文档规则

- 事实以代码为准。命令、默认值、退出码先看 `src/`，不要照抄旧文档。
- 中英 README 逐节对应，信息量一致。
- 版本锚点字符串被测试断言，不能手改。升版本走脚本。
- `CHANGELOG.md` 按 Keep a Changelog 组织；历史版本在 [`docs/CHANGELOG_ARCHIVE.md`](./docs/CHANGELOG_ARCHIVE.md)。
- 文档之间用相对链接，不写指向本仓库的 `https://github.com/.../blob/main/...`。

## 需要先讨论的改动

先在 issue 里说明，拿到结论再写代码：

- 改 CLI ↔ daemon 协议或扩展消息协议。
- 加扩展权限，或改 `manifest.json` 的 `host_permissions`。
- 改命令默认值：超时、上限、并发、挑战策略。
- 删除或重命名 JSON 字段、`error_code`。
- 改退出码语义。
- 改 `~/.atb` 下的文件格式。

参与讨论前请读 [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md)。
