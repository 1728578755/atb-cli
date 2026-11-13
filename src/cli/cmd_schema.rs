use super::*;

// `atb schema`：把全部子命令的参数（名/类型/默认值/可选值）与主要 error_code
// 输出成机器可读 JSON，让 Agent 一次拿到用法，不必逐条 `--help` / 试错。
//
// 实现用静态表而不是给 `CommandKind` 加方法：表就放在这一个文件里，
// 新增/改参数时对照 `args.rs` / `args_more.rs` / `args_net.rs` 逐个抄即可。
// 结构约定（稳定契约，不要随意改键名）：
// `{"ok":true,"command_count":N,"commands":{"open":{"args":{...},"error_codes":[...]}}}`
// 参数项形状：`{"type":"string|number|bool|path|int","default":...,"optional":true,"variadic":true,"enum":[...]}`
// flag 的键名带 `--` 前缀；位置参数不带。

/// 必填位置参数。
fn required(ty: &str) -> Value {
    json!({ "type": ty, "required": true })
}

/// 可选位置参数。
fn optional(ty: &str) -> Value {
    json!({ "type": ty, "optional": true })
}

/// 可变长位置参数（可给多个）。
fn variadic(ty: &str, required: bool) -> Value {
    json!({ "type": format!("{ty}+"), "variadic": true, "required": required })
}

/// 布尔 flag（开关，缺省 false）。
fn flag() -> Value {
    json!({ "type": "bool", "default": false })
}

/// 有默认值的标量参数。
fn with_default(ty: &str, default: Value) -> Value {
    json!({ "type": ty, "default": default })
}

/// 可选参数（无默认值，不给就用 daemon 侧行为）。
fn opt(ty: &str) -> Value {
    json!({ "type": ty, "optional": true })
}

/// 枚举参数。
fn with_enum(ty: &str, default: &str, values: &[&str]) -> Value {
    json!({ "type": ty, "default": default, "enum": values })
}

/// 把 b 合并进 a（浅合并，键冲突以 b 为准），返回合并结果。
/// 旧签名是 `(&mut Value, Value)` 且无返回值——调用方全都把它当表达式用，
/// 拿到的是 `()`，编译期就是 26 个 E0308；改成拿所有权、返回 Value。
fn merge_args(base: Value, extra: Value) -> Value {
    let mut base = base;
    if let (Some(a), Some(b)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in b {
            a.insert(key.clone(), value.clone());
        }
    }
    base
}

/// 页面命令通用的三件套 + 超时（--tab/--browser/--profile/--timeout）。
fn page_args(timeout: f64) -> Value {
    json!({
        "--tab": opt("string"),
        "--browser": opt("string"),
        "--profile": opt("string"),
        "--timeout": with_default("number", json!(timeout)),
    })
}

/// 单条命令的 schema 条目：名称、说明、参数、主要 error_code。
struct CmdSchema {
    name: &'static str,
    description: &'static str,
    args: Value,
    /// `Vec` 而不是静态切片：落扩展的页面命令要在 push 时自动补上 WS 通道类的码。
    error_codes: Vec<&'static str>,
}

/// 错误码 → 「下一步怎么办」（`atb schema` 的顶层 `error_code_hints`，纯新增字段）。
///
/// 逐命令的 `error_codes` 只回答「这条命令可能发哪些码」；Agent 真正难的是
/// 「这个码要不要重试 / 改哪里」。`retryable=false` 表示原样重跑没有意义
/// （要么改参数，要么先清环境）。
const ERROR_CODE_HINTS: &[(&str, &str, bool)] = &[
    (
        "ws_backpressure",
        "daemon 到扩展的下行队列已满（扩展没跟上），动作没有下发：等 1~2 秒后重试同一条命令",
        true,
    ),
    (
        "ws_disconnected",
        "扩展连接已断开（浏览器关闭 / 扩展重载 / 插件端口变了）：先 atb status 确认连接，再重试",
        true,
    ),
    (
        "task_panicked",
        // 与 server 侧 `multi.rs::hint_for_error` 同一句：同一个码在两处给不同口径会让人怀疑是不是两回事。
        "这是 atb 的 bug（任务 panic）：附上 daemon 日志反馈；先用 tabs 复核目标状态，不要立刻整批重试",
        true,
    ),
    (
        "wait_js_failed",
        "wait --js 的条件在超时前没有成立（页面通道超时或 CDP 回退也超时）：先确认表达式本身，或加 --timeout / 改用 wait --selector",
        true,
    ),
    (
        "wait_js_eval_blocked",
        // 常规路径 daemon 会自动改走 CDP；能看到这个码说明连 CDP 都定位不到会话。
        "页面 CSP 禁止 eval，且当前没有可定位的会话可改走 CDP：先 atb tabs 确认连接，或改用 wait --selector / snapshot 判断",
        false,
    ),
    (
        "net_rule_too_large",
        // 单条 body 超字节上限：本次未下发、规则未写入；原样重试永远不会成功。
        "单条 mock/route 的 body 超过上限（ATB_NET_RULE_MAX_BYTES，默认 2MiB，1KiB..16MiB）：裁剪 body 或调高该变量；本次规则未写入",
        false,
    ),
    (
        "too_many_rules",
        "该 profile 的 net 规则数超过上限：先 atb net routes 看清清单，再用 net unroute / net reset 清理",
        false,
    ),
    (
        "no_tabs",
        "浏览器里没有可用网页标签页：先 open 一个页面，或用 tabs 确认扩展已连接",
        true,
    ),
    (
        "target_not_found",
        "目标标签页/会话不存在或已关闭：用 tabs --ids 重新选，再传 --tab <tab_id>",
        false,
    ),
    (
        "outcome_unknown",
        "POST 已发出但响应丢失，动作可能已执行：不要盲目重试，先复查页面状态或带同一个 --idempotency-key 重放",
        false,
    ),
    (
        "job_wait_timeout",
        "job 在 --timeout 内没结束（不代表失败）：用 atb job get <id> 继续查，或加大 --timeout",
        true,
    ),
    (
        "invalid_api_token",
        "18767 上有一个不认当前 token 的 daemon：结束占用该端口的进程后重启（atb restart --force）",
        false,
    ),
    (
        "daemon_not_running",
        "daemon 没在跑：执行任意页面命令会自动拉起，或 atb restart",
        true,
    ),
    (
        "extension_not_connected",
        "扩展没连上：确认浏览器已开、扩展已启用，且插件端口与 CLI 配置一致",
        true,
    ),
    (
        "challenge_timeout",
        "挑战页在 --timeout 内没有放行：加 --timeout，或人工处理后用 challenge status 复查",
        true,
    ),
    (
        "file_not_found",
        "本地文件不存在：路径按当前 shell 的 cwd 解析，给绝对路径最稳",
        false,
    ),
    (
        "output_exists",
        "输出文件已存在：换一个 --out，或设 ATB_IO_OVERWRITE=backup/force",
        false,
    ),
    // 2026-11-13 改进轮新增。
    (
        "focus_failed",
        "标签页切前台失败：先用 tabs --ids 确认标签仍存在；扩展过旧时重新加载扩展",
        true,
    ),
    (
        "permission_required",
        "扩展未授权 management：打开扩展设置页 → 高级权限 → 授权读取扩展列表，然后重试 atb extensions",
        false,
    ),
    (
        "daemon_outdated",
        "常驻 daemon 还是旧二进制：运行 atb restart 加载新路由后重试",
        true,
    ),
    (
        "syntax_error",
        "脚本本地语法检查失败：按提示的行列修正；确认无误时用 --no-preflight 或 ATB_JS_CHECK=off 交给浏览器执行",
        false,
    ),
    (
        "inject_script_threw",
        "脚本已执行但抛错：看 threw 字段；这是脚本自身异常，不是注入通道失败",
        false,
    ),
    (
        "unsupported_selector",
        "该选择器形态不受支持：改用 find 拿到 CSS 选择器，或对 shadow DOM 用 host >>> inner",
        false,
    ),
    (
        "exec_result_not_found",
        "exec 的迟到结果不存在或已过期：确认 exec_id；长任务可加大 ATB_EXEC_RESULT_TTL_SEC 或改用 --async + job wait",
        true,
    ),
    (
        "cdp_required",
        "指定了 --require-cdp 但 CDP 不可用：页面可能被其它扩展 iframe 挡住，先关掉挡路扩展或去掉 --require-cdp",
        false,
    ),
    (
        "invalid_encoding",
        "脚本文件不是 UTF-8：转成 UTF-8，或用 --file - 从 stdin 传入",
        false,
    ),
    (
        "script_file_too_large",
        "脚本文件超过上限：拆分为多个 --file，或提高 ATB_SCRIPT_FILE_MAX_MB",
        false,
    ),
];

/// 会把操作下发到扩展页面的命令：都可能收到 WS 下行通道的两类失败
// 。集中在这里补，避免每加一条页面命令都漏。
const PAGE_OP_COMMANDS: &[&str] = &[
    "click",
    "mouse-click",
    "fill",
    "send-keys",
    "screenshot",
    "save-pdf",
    "scan",
    "exec",
    "cookies",
    "upload",
    "snapshot",
    "read",
    "find",
    "measure",
    "inject",
    "frames",
    "wait",
    "hover",
    "select",
    "check",
    "scroll",
    "back",
    "reload",
];

/// 常用错误码集中在此，保持与 daemon / CLI 预检实际返回的一致口径
/// （对照 `exit.rs::is_usage_error_code` 与 daemon 各端点的 error_code 字段）。
fn schema_entries() -> Vec<CmdSchema> {
    let mut list: Vec<CmdSchema> = Vec::new();
    let mut push = |name: &'static str,
                    description: &'static str,
                    mut args: Value,
                    error_codes: &'static [&'static str]| {
        // 参数表统一带一个空对象兜底：没有参数的命令输出 "args": {}。
        if args.as_object().is_none() {
            args = json!({});
        }
        let mut codes = error_codes.to_vec();
        if PAGE_OP_COMMANDS.contains(&name) {
            for code in ["ws_backpressure", "ws_disconnected"] {
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        list.push(CmdSchema {
            name,
            description,
            args,
            error_codes: codes,
        });
    };

    // ---- 标签页与目标 ----
    push(
        "tabs",
        "列出全部标签页（含 browser_id/profile_id/tab_id）",
        json!({
            "--browser": opt("string"),
            "--ids": flag(),
            "--profile": opt("string"),
            "--full": flag(),
            // --fields / --limit / --mine：精简输出三件套（未知字段 exit 2）。
            "--fields": opt("string"),
            // clap 侧是 `Option<usize>`（无默认值）。以前写成 default 0，
            // Agent 照表传 `--limit 0` 会拿到空列表；这里改为无默认的可选整数。
            "--limit": opt("int"),
            "--mine": flag(),
            // 2026-11-13：大浏览器过滤与流式输出。
            "--url-contains": opt("string"),
            "--title-contains": opt("string"),
            "--filter": opt("string"),
            "--format": with_enum("string", "json", &["json", "jsonl", "tsv"]),
        }),
        &["no_tabs", "invalid_api_token", "unsupported_flag"],
    );
    push(
        "focus",
        "把目标标签页切到前台；后台标签页会节流定时器/懒加载",
        json!({
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--no-window": flag(),
            "--timeout": with_default("number", json!(15.0)),
        }),
        &["target_not_found", "extension_outdated", "focus_failed"],
    );
    push(
        "use",
        "把当前智能体的默认目标固定到指定标签页",
        json!({
            "<target>": required("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
        }),
        &["target_not_found", "target_ambiguous"],
    );
    push(
        "tabtree",
        "按 browser → profile → 标签页树形展示归属",
        json!({
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--full": flag(),
        }),
        &[],
    );
    push(
        "lookup",
        "按 tab/browser/profile 反查归属",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["tab", "browser", "profile"] },
            "<arg>": required("string"),
            // 子命令各自的必填位置参数与专属 flag（上层是扁平表，这里补细节）。
            "<subcommand_args>": {
                "tab": { "<arg>": required("string"), "--browser": opt("string"), "--profile": opt("string") },
                "browser": { "<arg>": required("string") },
                "profile": { "<arg>": required("string") },
            },
            // 由反射测试补齐：`lookup tab` 支持按 browser/profile 缩小范围（LookupTabArgs），
            // 旧表漏了这两个 flag，Agent 会以为 lookup 不能带目标限定。
            "--browser": opt("string"),
            "--profile": opt("string"),
        }),
        &["target_not_found"],
    );
    push(
        "profile-label",
        "给 Chrome Profile 起可读别名",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["set", "clear"] },
            "<label>": optional("string"),
            "--profile": opt("string"),
            "--browser": opt("string"),
            "--tab": opt("string"),
            "--timeout": with_default("number", json!(30.0)),
        }),
        &["target_not_found"],
    );
    push("browsers", "列出已连接的浏览器实例", json!({}), &[]);

    // ---- 页面读取 ----
    push(
        "scan",
        "内容感知：取正文、列表、页面文本",
        merge_args(
            json!({
                "--tabs-only": flag(),
                "--text-only": flag(),
                "--fast": flag(),
                "--max-nodes": opt("number"),
            }),
            page_args(60.0),
        ),
        &["no_tabs", "target_not_found", "target_out_of_range"],
    );
    push(
        "read",
        "结构化读页面：标题、正文、表单、计数",
        merge_args(
            json!({
                "--selector": opt("string"),
                "--max-chars": with_default("number", json!(20000)),
                "--links": flag(),
                "--no-forms": flag(),
                "--include-hidden": flag(),
                "--budget-ms": opt("number"),
                "--max-nodes": opt("number"),
                // daemon 侧 `ReadRequest.inner_text_limit`（DOM 节点数阈值，非字符数）。
                // 不传或传 0 → 页面用默认 30000；正整数由 daemon clamp 到 1000..=500000。
                // 字符量看 --max-chars（daemon clamp 1000..=400000）；本项越小越偏向
                // 近似可见文本（visible-approx：更快但可能少内容），是精度/速度旋钮。
                "--inner-text-limit": opt("int"),
                "--frame": opt("int"),
            }),
            page_args(30.0),
        ),
        &["no_tabs", "target_not_found", "target_out_of_range"],
    );
    push(
        "find",
        "按 CSS / 可见文本 / XPath 找元素",
        merge_args(
            json!({
                "<selector>": optional("string"),
                "--text": opt("string"),
                "--xpath": opt("string"),
                "--limit": with_default("number", json!(50)),
                "--exact": flag(),
                "--include-hidden": flag(),
                "--frame": opt("int"),
            }),
            page_args(20.0),
        ),
        &["no_tabs", "target_not_found", "target_out_of_range"],
    );
    push(
        "measure",
        "元素量测：rect / computed style / 可见性",
        json!({
            "--selector": required("string"),
            "--styles": opt("string"),
            "--with-text": flag(),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--frame": opt("int"),
            "--timeout": with_default("number", json!(30.0)),
        }),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "selector_not_found",
            "unsupported_selector",
        ],
    );
    push(
        "snapshot",
        "生成带 @e 引用的可点击树",
        merge_args(
            json!({
                "--offset": with_default("number", json!(0)),
                "--limit": with_default("number", json!(200)),
                "--details": flag(),
                "--js": flag(),
                "--interactive-only": flag(),
                "--max-nodes": with_default("number", json!(0)),
                // 保留无障碍树里的 generic 容器节点（默认跳过）。
                "--no-skip-generic": flag(),
            }),
            page_args(30.0),
        ),
        &["no_tabs", "target_not_found", "target_out_of_range"],
    );
    push(
        "frames",
        "列出页面内 iframe",
        page_args(20.0),
        &["no_tabs", "target_not_found"],
    );

    // ---- 页面执行 ----
    let wait_common = json!({
        "--monitor": flag(),
        "--wait-js": opt("string"),
        "--wait-timeout": with_default("number", json!(3.0)),
        "--wait-interval": with_default("number", json!(0.1)),
    });
    push(
        "exec",
        "执行任意 JS 并回传结果",
        merge_args(
            json!({
                "<script>": with_default("string", json!("")),
                // 2026-11-13：--file 可重复，按顺序拼接；`-` 读 stdin。
                "--file": variadic("path", false),
                "--max-bytes": opt("number"),
                "--no-preflight": flag(),
                // v11：clap 侧只接受 main/isolated（大小写不敏感），表里必须
                // 同步 enum，否则 `atb schema` 会告诉 Agent「任意字符串都行」。
                "--world": with_enum("string", "main", &["main", "isolated"]),
                "--frame": opt("int"),
                "--all-frames": flag(),
            }),
            merge_args(wait_common.clone(), page_args(60.0)),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "extension_outdated",
            "invalid_world",
            "invalid_frame",
            // 位置参数脚本与 --file 同时给。
            "unsupported_combination",
            // ：组合 wait 失败与受限页面必须能被调用方按码归类。
            "wait_js_failed",
            // CONTRACT §3.2：页面禁 eval 时 daemon 回退 CDP，走到这里说明回退也失败。
            "wait_js_eval_blocked",
            "page_protected",
        ],
    );
    push(
        "click",
        "点击元素（CSS 选择器或 @e 引用）",
        merge_args(
            json!({
                "<target>": required("string"),
                "--frame": opt("int"),
            }),
            merge_args(wait_common.clone(), page_args(30.0)),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "extension_outdated",
        ],
    );
    push(
        "mouse-click",
        "按坐标或元素发真实鼠标事件",
        merge_args(
            json!({
                "<target>": required("string"),
                "--frame": opt("int"),
            }),
            merge_args(wait_common.clone(), page_args(30.0)),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "extension_outdated",
        ],
    );
    push(
        "fill",
        "填写输入框（默认覆盖，--append 追加，--clear 清空）",
        merge_args(
            json!({
                "<target>": required("string"),
                "<value>": optional("string"),
                "--append": flag(),
                "--clear": flag(),
                "--frame": opt("int"),
            }),
            merge_args(wait_common.clone(), page_args(30.0)),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "extension_outdated",
        ],
    );
    push(
        "send-keys",
        "向焦点元素或 --target 元素发送按键",
        merge_args(
            json!({
                "<keys>": required("string"),
                "--target": opt("string"),
                "--frame": opt("int"),
            }),
            merge_args(wait_common.clone(), page_args(30.0)),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "target_out_of_range",
            "extension_outdated",
        ],
    );
    push(
        "upload",
        "给文件输入框设置本地文件",
        merge_args(
            json!({
                "<target>": required("string"),
                "<files>": variadic("path", true),
            }),
            page_args(30.0),
        ),
        &["file_not_found", "no_tabs", "target_not_found"],
    );
    push(
        "inject",
        "注入 JS；--persist 命名后每次导航自动重放",
        merge_args(
            json!({
                "<script>": optional("string"),
                "--file": opt("path"),
                "--persist": opt("string"),
                "--remove": opt("string"),
                "--list": flag(),
                // 同 exec：inject 也下发到扩展的 world，自由字符串会静默落成 MAIN。
                "--world": with_enum("string", "main", &["main", "isolated"]),
                "--match": variadic("string", false),
                "--frame": opt("int"),
                "--all-frames": flag(),
                // 2026-11-13：用户脚本模式与持久注入安全阀。
                "--userscript": flag(),
                "--gm-namespace": opt("string"),
                "--cleanup-selector": variadic("string", false),
                "--cleanup-file": opt("path"),
                "--ttl": opt("int"),
                "--persist-forever": flag(),
                "--i-know-this-is-long-lived": flag(),
                "--no-preflight": flag(),
            }),
            page_args(30.0),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "invalid_argument",
            "syntax_error",
            "inject_script_threw",
        ],
    );
    push(
        "userscript",
        "打开用户脚本安装页（Tampermonkey/Violentmonkey）",
        json!({
            "<source>": required("string"),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--parse-only": flag(),
        }),
        &[
            "file_not_found",
            "invalid_encoding",
            "unsupported_combination",
        ],
    );

    // ---- 动作类（ActionArgs）----
    let action_args = merge_args(
        json!({
            "<target>": optional("string"),
            "--index": opt("int"),
            "--value": opt("string"),
            "--label": opt("string"),
            "--dy": opt("number"),
            "--to": opt("string"),
            "--frame": opt("int"),
        }),
        page_args(15.0),
    );
    for (name, description) in [
        ("hover", "悬停元素"),
        ("select", "选择下拉框选项（--value 或 --label）"),
        ("check", "勾选或取消勾选 checkbox / radio"),
        ("scroll", "滚动页面（--to top/bottom 或 --dy 像素）"),
    ] {
        push(
            name,
            description,
            action_args.clone(),
            &[
                "no_tabs",
                "target_not_found",
                "target_out_of_range",
                "extension_outdated",
            ],
        );
    }

    // ---- 等待 / 挑战 / 反检测 ----
    push(
        "wait",
        "等待元素、文本或 JS 条件成立（默认 10s）",
        merge_args(
            json!({
                "--selector": opt("string"),
                "--text": opt("string"),
                "--js": opt("string"),
                "--visible": flag(),
                "--interval": with_default("number", json!(0.1)),
                "--frame": opt("int"),
                "--dom-stable": opt("int"),
                "--network-idle": opt("int"),
            }),
            page_args(10.0),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "wait_timeout",
            // wait --js 条件未成立（页面通道轮询到超时 / CDP 回退也超时）：netpage 已把
            // 内层码上提到顶层，调用方可以直接按码分支，不用解析中文。
            "wait_js_failed",
            // 只有「CSP 禁 eval 且没有可定位会话可回退 CDP」时才可见（契约 §3.2）。
            "wait_js_eval_blocked",
        ],
    );
    push(
        "challenge",
        "挑战页/验证码：识别、等待自动放行、需要人时转人工",
        merge_args(
            json!({
                "<action>": with_enum("string", "status", &["status", "wait", "solve", "auto", "click", "widget", "focus"]),
                "--timeout": opt("number"),
                "--interval": with_default("number", json!(0.25)),
                "--focus-after": with_default("number", json!(8.0)),
                "--no-focus": flag(),
                "--reload-after": with_default("number", json!(0.0)),
                "--at": opt("string"),
                // 挑战页识别后的确认等待（毫秒，0 = 不等待），对应服务端 confirm_ms。
                "--confirm-ms": opt("number"),
                "--frame": opt("int"),
            }),
            // `challenge` 的 `--timeout` 在 clap 里是 `Option<f64>`（不传时由
            // 服务端按 action 取默认：wait 30s / solve 120s）。以前复用了
            // `page_args(15.0)`，把「未指定」静默写成默认 15s，语义被这个表带偏。
            // 这里显式列三项通用参数，保留 `--timeout` 的「无可选默认」。
            json!({
                "--tab": opt("string"),
                "--browser": opt("string"),
                "--profile": opt("string"),
            }),
        ),
        &[
            "no_tabs",
            "target_not_found",
            "challenge_blocked",
            "wait_timeout",
            // /：探测不可用与整条点击链超时。
            "challenge_probe_failed",
            "challenge_click_timeout",
            // CLI 侧把「超时未达成」改写成这个码（wait/solve 的终态判据）。
            "challenge_timeout",
        ],
    );
    push(
        "stealth",
        "自动化痕迹：审计或清除本工具留下的痕迹",
        merge_args(
            json!({
                "<action>": with_enum("string", "audit", &["audit", "scrub"]),
                "--frame": opt("int"),
            }),
            page_args(15.0),
        ),
        &["no_tabs", "target_not_found"],
    );

    // ---- 标签页生命周期 ----
    push(
        "open",
        "打开 URL 或本地文件；多个目标并发打开",
        json!({
            "<targets>": variadic("string", true),
            "--background": flag(),
            "--window": flag(),
            "--focus": flag(),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--session": opt("string"),
            "--group-title": opt("string"),
            "--timeout": with_default("number", json!(10.0)),
            "--challenge": with_enum("string", "auto", &["auto", "wait", "off"]),
            "--challenge-timeout": with_default("number", json!(15.0)),
            // --wait-for / --warm 与 --group-title 同批加入（open 体验改进），
            // schema 表必须同步，否则 Agent 会漏用这两个 flag。
            "--wait-for": opt("string"),
            "--wait-for-timeout": opt("number"),
            "--warm": flag(),
        }),
        &[
            "invalid_open_target",
            "invalid_timeout",
            // open_target 解析器与 --wait-for 实际可发的码，以前表里漏了。
            "unsupported_scheme",
            "invalid_file_url",
            "unsupported_file_host",
            "file_not_found",
            "wait_for_timeout",
            "target_not_found",
            "target_out_of_range",
            // --challenge-timeout 越界/非法。
            "invalid_argument",
            "no_tabs",
            "extension_outdated",
            "challenge_blocked",
        ],
    );
    push(
        "close",
        "关闭指定标签页（--tab 必填；id / id 列表 / group:<标题>，group 需配 --yes 显式确认；all/url:/title: 仍拒绝）",
        json!({
            "--tab": required("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--timeout": with_default("number", json!(30.0)),
            // group:<标题> 关闭是误伤高危动作，必须 --yes 二次确认。
            "--yes": flag(),
        }),
        &[
            "target_not_found",
            "target_out_of_range",
            "unsupported_combination",
        ],
    );
    push(
        "cleanup",
        "关闭本智能体通过 open 打开的标签页",
        json!({
            "--all-agents": flag(),
            "--dry-run": flag(),
        }),
        // 清理任务 panic 不再算 timeout，回包必须有稳定码与 hint。
        &["task_panicked"],
    );
    push(
        "back",
        "浏览器后退",
        page_args(30.0),
        // 导航命令的超时校验发 invalid_timeout；目标解析发 invalid_argument。
        &[
            "no_tabs",
            "target_not_found",
            "invalid_timeout",
            "invalid_argument",
        ],
    );
    push(
        "reload",
        "重新加载当前标签页",
        page_args(30.0),
        &[
            "no_tabs",
            "target_not_found",
            "invalid_timeout",
            "invalid_argument",
        ],
    );

    // ---- 采集 / 文件 ----
    push(
        "screenshot",
        "截图当前页面",
        merge_args(
            json!({
                "--target": opt("string"),
                "--selector": opt("string"),
                "--out": opt("path"),
                "--format": with_enum("string", "png", &["png", "jpeg", "webp"]),
                "--quality": opt("int"),
                "--full-page": flag(),
                // CDP 主路径与拼图回落都按此上限截断（超出回 truncated:true）。
                "--max-height": opt("int"),
                // 2026-11-13：截图前等待与禁止静默降级。
                "--settle-ms": opt("int"),
                "--wait-selector": opt("string"),
                "--network-idle": opt("int"),
                "--require-cdp": flag(),
            }),
            page_args(30.0),
        ),
        &[
            "output_path_collision",
            "no_tabs",
            "target_not_found",
            // ：字节上限与调试器被占用。
            "capture_too_large",
            "debugger_blocked",
            "page_protected",
            "wait_for_timeout",
            "cdp_required",
        ],
    );
    push(
        "save-pdf",
        // +：`--max-height` 现在在常规 CDP 打印路径也生效（超出截断并回
        // truncated:true），不再只是图片型回落时的旋钮。
        "把当前页面另存为 PDF（--max-height 限制输出高度，CDP 主路径与图片型回落均生效）",
        merge_args(
            json!({
                "--out": opt("path"),
                "--paper": with_default("string", json!("a4")),
                "--landscape": flag(),
                "--scale": with_default("number", json!(1.0)),
                "--no-print-background": flag(),
                // 输出高度上限（设备像素），默认 20000；主路径与图片型回落都生效。
                "--max-height": opt("int"),
            }),
            page_args(30.0),
        ),
        &[
            "output_path_collision",
            "no_tabs",
            "target_not_found",
            // ：超长页切片失败与输出字节上限。
            "pdf_too_tall",
            "capture_too_large",
            "debugger_blocked",
        ],
    );
    push(
        "cookies",
        "读取/清空当前页面 Cookie",
        merge_args(
            json!({
                "--url": opt("string"),
                "--clear": flag(),
                "--domain": opt("string"),
            }),
            page_args(30.0),
        ),
        &["no_tabs", "target_not_found"],
    );

    // ---- 网络 / 下载 ----
    push(
        "network",
        "抓取页面网络请求",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["start", "list", "detail", "clear", "stop"] },
            // detail 需要 request_id；list 支持 --filter/--limit。
            "<subcommand_args>": {
                "start": {},
                "list": { "--filter": opt("string"), "--limit": with_default("number", json!(100)) },
                "detail": { "<request_id>": required("string") },
                "clear": {},
                "stop": {}
            },
            // 由反射测试补齐：这些 flag 分散在 network 的各子命令上
            // （start/clear/stop 用 TabOnlyArgs，list 另有 --filter/--limit，detail 取 request_id），
            // 旧表只写了 <subcommand>，Agent 会以为 network 不支持目标限定与过滤。
            "--filter": opt("string"),
            "--limit": with_default("number", json!(100)),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--timeout": with_default("number", json!(30.0)),
        }),
        &["no_tabs", "target_not_found"],
    );
    push(
        "console",
        "抓取页面 console 输出",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["start", "list", "clear", "stop", "history", "tail"] },
            // 由反射测试补齐：同上，start/clear/stop 用 TabOnlyArgs，
            // list 另有 --level/--limit，旧表只写了 <subcommand>。
            "--level": opt("string"),
            "--limit": with_default("number", json!(100)),
            // 2026-11-13：history/tail 的采样与过滤。
            "--settle-ms": opt("int"),
            "--types": opt("string"),
            "--clear": flag(),
            "--duration": with_default("number", json!(10.0)),
            "--interval": with_default("number", json!(0.5)),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--timeout": with_default("number", json!(30.0)),
        }),
        &["no_tabs", "target_not_found"],
    );
    push(
        "net",
        "网络拦截：fetch / hook / mock / log",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["fetch", "hook", "log", "mock", "routes", "unroute", "reset"] },
            // net 的必填位置参数按子命令区分（fetch <url> / mock|unroute <pattern>）；
            // 扁平表只有 <subcommand>，Agent 不知道后面还要跟一个参数。
            "<subcommand_args>": {
                "fetch": { "<url>": required("string") },
                "hook": {},
                "log": {},
                "mock": { "<pattern>": required("string") },
                "routes": {},
                "unroute": { "<pattern>": required("string") },
                "reset": {}
            },
            // 只对 net fetch 生效的两个页面侧开关（其余子命令忽略）。
            "--credentials": with_enum("string", "include", &["include", "omit", "same-origin"]),
            "--no-parse-json": flag(),
            // 由反射测试补齐：net 是**扁平表项 + 子命令**，下面这些 flag 分散在
            // fetch / hook / log / mock / unroute 上（另一半 tab/browser/profile/frame
            // 是各子命令共用的目标限定）。旧表只列了 fetch 的两个页面侧开关，
            // Agent 照表用 --method / --header / --persist 时根本不知道它们存在。
            // fetch / mock
            "--method": opt("string"),
            "--header": variadic("string", false),
            "--max-chars": with_default("number", json!(200000)),
            "--response-type": with_enum("string", "text", &["text", "base64"]),
            // mock
            "--status": opt("int"),
            "--abort": flag(),
            "--request-header": variadic("string", false),
            "--request-body": opt("string"),
            "--response-status": opt("int"),
            "--response-body": opt("string"),
            // hook
            "--capture-bodies": flag(),
            "--persist": flag(),
            "--max-log": opt("int"),
            // log
            "--debug-pass": flag(),
            "--clear": flag(),
            // log / mock / unroute（mock 默认 page）
            "--engine": with_enum("string", "page", &["page", "cdp", "both"]),
            // fetch / log / routes / reset / hook / mock 共用
            "--body": opt("string"),
            "--match": variadic("string", false),
            "--limit": with_default("number", json!(50)),
            "--tab": opt("string"),
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--frame": opt("int"),
            "--timeout": with_default("number", json!(30.0)),
        }),
        &[
            "no_tabs",
            "target_not_found",
            "invalid_argument",
            // 每 profile 规则条数上限（page 引擎与 CDP 引擎同源校验）。
            "too_many_rules",
            // 单条 body 超字节上限（与条数满同族，但语义不同：改 payload 而不是清规则）。
            "net_rule_too_large",
        ],
    );
    push(
        "downloads",
        "查看下载列表",
        merge_args(
            json!({
                "--limit": opt("number"),
                "--state": opt("string"),
                "--id": opt("int"),
            }),
            page_args(20.0),
        ),
        &["no_tabs", "target_not_found"],
    );
    push(
        "download",
        "发起下载并保存到本地",
        merge_args(
            json!({
                "<url>": required("string"),
                "--filename": opt("string"),
                "--dir": opt("string"),
            }),
            page_args(30.0),
        ),
        &[
            "no_tabs",
            "target_not_found",
            // ：文件名净化拒绝。
            "invalid_filename",
        ],
    );
    push(
        "download-wait",
        "等待下载完成",
        merge_args(
            json!({
                "--id": opt("int"),
                "--timeout": with_default("number", json!(120.0)),
                "--interval": with_default("number", json!(0.5)),
            }),
            json!({
                "--tab": opt("string"),
                "--browser": opt("string"),
                "--profile": opt("string"),
            }),
        ),
        &[
            "no_tabs",
            "wait_timeout",
            // 关闭自动认领后必须显式 --id。
            "download_id_required",
        ],
    );

    // ---- 编排 ----
    push(
        "job",
        "管理 --async 提交的后台任务",
        json!({
            "<subcommand>": { "type": "enum", "required": true, "enum": ["list", "get", "wait", "cancel", "tail", "result"] },
            "<id>": optional("string"),
            // get / wait / cancel 必须带 <id>（list 不需要）。
            "<subcommand_args>": {
                "list": { "--limit": with_default("number", json!(20)) },
                "get": { "<id>": required("string") },
                "wait": { "<id>": required("string"), "--timeout": with_default("number", json!(300.0)), "--interval": with_default("number", json!(0.3)) },
                "cancel": { "<id>": required("string") },
                "tail": { "<id>": required("string"), "--timeout": with_default("number", json!(600.0)), "--interval": with_default("number", json!(0.5)), "--human": flag() },
                "result": { "<id>": required("string"), "--wait": with_default("number", json!(5.0)), "--interval": with_default("number", json!(0.5)) }
            },
            "--timeout": opt("number"),
            "--limit": opt("number"),
            "--interval": opt("number"),
            "--human": flag(),
            "--wait": opt("number"),
        }),
        // 后台任务 panic 是 bug，不是超时；job get/wait 会看到该状态。
        &[
            "job_not_found",
            "wait_timeout",
            "invalid_argument",
            "task_panicked",
        ],
    );
    push(
        "run",
        "按步骤文件串行执行多步操作",
        json!({
            "<file>": required("path"),
            "--continue-on-error": flag(),
        }),
        // --continue-on-error 与文件里的 stop_on_error 冲突；invalid_steps = 文件格式错。
        &["file_not_found", "invalid_steps", "unsupported_combination"],
    );
    push(
        "batch",
        "按步骤文件并发执行（跨标签）",
        json!({
            "<file>": required("path"),
            "--concurrency": with_default("number", json!(4)),
        }),
        &["file_not_found"],
    );
    push(
        "broadcast",
        // 路径绝对化默认开启；无同名 CLI flag，用 ATB_ALLOW_RELATIVE_OUT=1 保留旧行为。
        "对所有或指定 browser/profile 并发执行同一操作（ATB_ALLOW_RELATIVE_OUT=1 可关闭输出路径绝对化）",
        json!({
            "<op>": required("string"),
            "--script": opt("string"),
            "--file": opt("path"),
            "--args": opt("string"),
            "--browser": variadic("string", false),
            "--profile": variadic("string", false),
            "--concurrency": opt("number"),
            "--timeout": with_default("number", json!(180.0)),
        }),
        &["invalid_argument", "no_tabs"],
    );

    // ---- 浏览器进程 ----
    push(
        "launch",
        "用「不弹调试横幅」的参数启动浏览器",
        json!({
            "<urls>": variadic("string", false),
            "--browser": opt("string"),
            "--user-data-dir": opt("path"),
            "--restart": flag(),
            "--no-restore": flag(),
            "--force": flag(),
            "--timeout": with_default("number", json!(20.0)),
            "--arg": variadic("string", false),
        }),
        &["file_not_found", "invalid_argument"],
    );

    // ---- daemon / 诊断 ----
    push(
        "status",
        "健康状态（退出码即结论）",
        json!({}),
        // summary 直接当 error_code：这里列的是「一眼能分支」的那几种。
        &[
            "daemon_not_running",
            "daemon_token_mismatch",
            "health_timeout",
            "health_failed",
            "extension_port_bind_failed",
            "port_mismatch",
            "extension_not_connected",
            "no_active_tabs",
        ],
    );
    push(
        "env",
        "环境快照：浏览器/Profile/扩展/标签页/持久注入/警告一次拿齐",
        json!({
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--tab": opt("string"),
            "--tab-limit": with_default("int", json!(10)),
        }),
        &[
            "daemon_not_running",
            "daemon_token_mismatch",
            "invalid_api_token",
        ],
    );
    push(
        "extensions",
        "列出已安装扩展（需在扩展设置页授权 management 可选权限）",
        json!({
            "--browser": opt("string"),
            "--profile": opt("string"),
            "--include-disabled": flag(),
            "--timeout": with_default("number", json!(15.0)),
        }),
        &[
            "permission_required",
            "extension_outdated",
            "daemon_outdated",
            "target_not_found",
        ],
    );
    push(
        "logs",
        "查看 daemon 日志尾部",
        json!({ "--tail": with_default("number", json!(100)) }),
        &["file_not_found"],
    );
    push(
        "schema",
        "输出全部命令的机器可读 schema",
        // clap 侧是普通开关（默认 false）；JSON 输出本来就是缺省行为，`--json` 只是显式声明。
        json!({ "--json": flag() }),
        &[],
    );
    push(
        "capabilities",
        "一次探测 daemon/扩展版本与能力门禁",
        json!({}),
        &[
            "daemon_not_running",
            "daemon_token_mismatch",
            // 扩展版本低于 CLI 期望 / 扩展没连上：两者结论不同，码也要分开。
            "extension_outdated",
            "extension_not_connected",
        ],
    );
    push(
        "doctor",
        "环境自检；--fix 尝试自动修复",
        json!({
            "--fix": flag(),
            "--force": flag(),
        }),
        &["doctor_checks_failed"],
    );
    push(
        "install-skill",
        "把随包的 SKILL.md 安装到 ~/.agents/skills",
        json!({
            "--dry-run": flag(),
            "--yes": flag(),
        }),
        &["file_not_found"],
    );
    push(
        "set-extension-port",
        "修改扩展 WebSocket 端口并重启 daemon",
        json!({ "<port>": required("int") }),
        &["invalid_argument"],
    );
    push(
        "trust-extension",
        "管理 WebSocket 信任配置",
        json!({
            "<extension_id>": optional("string"),
            "--remove": flag(),
            "--list": flag(),
            "--clear": flag(),
            "--token": opt("string"),
            "--clear-token": flag(),
        }),
        &["invalid_argument"],
    );
    push("stop", "停止后台 daemon", json!({}), &[]);
    push(
        "restart",
        "重启 daemon（强杀时的身份核对严格度由 ATB_FORCE_ASSUME_ATB=strict|loose 控制）",
        // 由反射测试补齐：`restart --force` 是唯一的手工救援路径（旧 daemon 占端口
        // 且 token 失配时），旧表写成空对象，Agent 根本发现不了它。
        json!({ "--force": flag() }),
        &[],
    );
    push("daemon", "前台运行 daemon（调试用）", json!({}), &[]);

    // ---- 批量抓取 ----
    push(
        "scrape",
        // CMD-*：`--out` 已存在时的覆盖策略由 ATB_IO_OVERWRITE 控制（backup/force/ask/append）。
        "批量抓取：分批开页 → 执行 JS → 收集 → 关页，失败退避重试（覆盖策略见 ATB_IO_OVERWRITE）",
        json!({
            "--urls": required("string"),
            "--js-file": opt("path"),
            "--out": required("path"),
            "--concurrency": with_default("number", json!(4)),
            "--retry": with_default("number", json!(3)),
            "--batch": with_default("number", json!(4)),
            "--timeout": with_default("number", json!(30.0)),
            "--group-title": opt("string"),
            // 默认每个 URL 只写一条最终记录；逐次写法只在诊断时用。
            "--record-attempts": flag(),
        }),
        &[
            "file_not_found",
            "no_tabs",
            "invalid_argument",
            "outcome_unknown",
        ],
    );

    list
}

pub(crate) fn cmd_schema(args: SchemaArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("schema", is_async)?;
    // `--json` 目前是缺省行为：输出本来就是 JSON，这个 flag 只是显式声明，
    // 将来若加 `--markdown` 之类的别的格式，Agent 侧用法也不用变。
    let _ = args.json;
    let entries = schema_entries();
    let mut commands = serde_json::Map::new();
    for entry in entries {
        commands.insert(
            entry.name.to_string(),
            json!({
                "description": entry.description,
                "args": entry.args,
                "error_codes": entry.error_codes,
            }),
        );
    }
    let count = commands.len();
    // 顶层错误码目录（纯新增字段）：逐命令的 error_codes 只有码名，这里是「怎么办」。
    let mut hints = serde_json::Map::new();
    for &(code, hint, retryable) in ERROR_CODE_HINTS {
        hints.insert(
            code.to_string(),
            json!({ "hint": hint, "retryable": retryable }),
        );
    }
    print_json(json!({
        "ok": true,
        "command_count": count,
        "commands": commands,
        "error_code_hints": hints,
    }));
    Ok(())
}

// ---------------------------------------------------------------------------
// 反射一致性测试
// ---------------------------------------------------------------------------
//
// 上面那张静态表是 `atb schema --json` 的唯一数据源，而 Agent 恰恰只信这张表：
// 表里漏一个 flag，Agent 就永远不知道它可用；表里多留一个已删除的 flag，Agent
// 照表调用会拿到 clap 的 invalid_argument（exit 2）。静态表与 clap 定义是两份
// 独立维护的真相，天然会漂移，所以这里用 `Cli::command()` 把真实命令树反射出来，
// 与表做**双向**比对，漂移即测试失败。
//
// 比对口径：
// 1. 顶层命令名集合必须完全相等（clap 自带的 `help` 除外）；
// 2. 每个命令的长参数（`--flag`）集合必须完全相等，双向都查；
// 3. 全局参数（`Cli` 上 `global = true` 的那几个 + clap 自带的 help/version）
//    对所有子命令都生效，逐条列进每个表项没有意义，双向排除；
// 4. 带子命令的表项（job / net / network / console / lookup / profile-label…）
//    按「整棵子树的并集」比对——表是扁平的，子命令参数只能摊进父表项。
#[cfg(test)]
mod schema_reflection_tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::{BTreeMap, BTreeSet};

    /// 全局参数：`Cli` 上的 `--async/--pretty/--dry-run/--agent/--idempotency-key`
    /// 加 clap 自动生成的 `--help/--version`。它们不属于任何单个命令的契约。
    const GLOBAL_FLAGS: &[&str] = &[
        "pretty",
        "dry-run",
        "agent",
        "async",
        "idempotency-key",
        "help",
        "version",
    ];

    fn is_global(long: &str) -> bool {
        GLOBAL_FLAGS.contains(&long)
    }

    /// 表项里的长参数集合。键名约定带 `--` 前缀；`<subcommand>` / `<target>`
    /// 这类位置参数不是 flag，不参与比对。
    fn table_flags(args: &serde_json::Value) -> BTreeSet<String> {
        args.as_object()
            .map(|map| {
                map.keys()
                    .filter(|key| key.starts_with("--"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// clap 命令树里全部长参数（含所有后代子命令），不排除全局参数。
    fn clap_flags(command: &clap::Command) -> BTreeSet<String> {
        fn walk(command: &clap::Command, out: &mut BTreeSet<String>) {
            for arg in command.get_arguments() {
                if let Some(long) = arg.get_long() {
                    out.insert(format!("--{long}"));
                }
            }
            for sub in command.get_subcommands() {
                walk(sub, out);
            }
        }
        let mut out = BTreeSet::new();
        walk(command, &mut out);
        out
    }

    /// `--name` 是否属于全局参数白名单。
    fn is_global_flag(flag: &str) -> bool {
        flag.strip_prefix("--").map(is_global).unwrap_or(false)
    }

    fn joined(set: &BTreeSet<String>) -> String {
        if set.is_empty() {
            "（无）".to_string()
        } else {
            set.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    }

    #[test]
    fn schema_matches_clap_definitions() {
        let cli = Cli::command();
        let entries = schema_entries();

        // ---- 1) 顶层命令集合 ----
        let clap_names: BTreeSet<String> = cli
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .filter(|name| name != "help")
            .collect();
        let table_names: BTreeSet<String> =
            entries.iter().map(|entry| entry.name.to_string()).collect();

        // 表项重名会让 `atb schema` 的后一项静默覆盖前一项（command_count 也会少算），
        // 所以顺手断言「表项数 == 命令名去重数」。
        assert_eq!(
            entries.len(),
            table_names.len(),
            "schema 表里有重复命令：{} 条表项但只有 {} 个不同命令：{:?}",
            entries.len(),
            table_names.len(),
            entries.iter().map(|entry| entry.name).collect::<Vec<_>>()
        );

        let only_in_table: Vec<&str> = table_names
            .difference(&clap_names)
            .map(String::as_str)
            .collect();
        let only_in_clap: Vec<&str> = clap_names
            .difference(&table_names)
            .map(String::as_str)
            .collect();
        assert!(
            only_in_table.is_empty() && only_in_clap.is_empty(),
            "顶层命令集合与 clap 定义不一致：\n  表里多出（clap 没这个命令）：{:?}\n  表里缺少（clap 有但 schema 没列）：{:?}",
            only_in_table,
            only_in_clap
        );

        // ---- 2) 每个命令的长参数集合（双向） ----
        let clap_by_name: BTreeMap<String, BTreeSet<String>> = cli
            .get_subcommands()
            .filter(|sub| sub.get_name() != "help")
            .map(|sub| (sub.get_name().to_string(), clap_flags(sub)))
            .collect();

        let mut problems: Vec<String> = Vec::new();
        for entry in &entries {
            // 命令本身缺失已在第 1 步报过，这里只管参数。
            let Some(clap_all) = clap_by_name.get(entry.name) else {
                continue;
            };
            let actual = table_flags(&entry.args);
            // 表里允许写全局参数（如 cleanup 的 --dry-run 既是全局也真有本地定义）。
            let mut acceptable = clap_all.clone();
            for global in GLOBAL_FLAGS {
                acceptable.insert(format!("--{global}"));
            }
            // 必须逐条列进表里的是「非全局参数」。
            let expected: BTreeSet<String> = clap_all
                .iter()
                .filter(|flag| !is_global_flag(flag.as_str()))
                .cloned()
                .collect();
            for flag in actual.difference(&acceptable) {
                problems.push(format!(
                    "命令 `{}`：表里多出参数 {flag}（clap 里没有这个长参数，Agent 照表调用必然 exit 2）；表里是 {}，clap 是 {}",
                    entry.name,
                    joined(&actual),
                    joined(&expected)
                ));
            }
            for flag in expected.difference(&actual) {
                problems.push(format!(
                    "命令 `{}`：表里缺少参数 {flag}（clap 有，schema 漏了）；表里是 {}，clap 是 {}",
                    entry.name,
                    joined(&actual),
                    joined(&expected)
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "schema 表与 clap 定义漂移，共 {} 处：\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    // -----------------------------------------------------------------------
    // ：名称集合一致只是最低要求。
    // 下面再逐项比对 default / type / enum / required / variadic / 位置参数，
    // 并断言命令总数；表里写错默认值（如 tabs --limit 默认 0）也会失败。
    // -----------------------------------------------------------------------

    fn value_to_default_string(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Bool(flag) => flag.to_string(),
            other => other.to_string(),
        }
    }

    /// 数值默认值统一按 f64 归一：`30` 与 `30.0` 视为相同。
    fn normalize_default(raw: &str) -> String {
        let trimmed = raw.trim();
        match trimmed.parse::<f64>() {
            Ok(number) if number.is_finite() => format!("{number}"),
            _ => trimmed.to_string(),
        }
    }

    fn clap_args_by_long(command: &clap::Command) -> BTreeMap<String, &clap::Arg> {
        command
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(|long| (long.to_string(), arg)))
            .collect()
    }

    /// 表里的「叶子位置参数」：`<target>` 这类带 `type` 的键。
    /// `<subcommand>` / `<subcommand_args>` 是分层节点，不参与位置参数比对。
    fn table_positionals(args: &serde_json::Value) -> BTreeSet<String> {
        args.as_object()
            .map(|map| {
                map.iter()
                    .filter(|(key, value)| {
                        key.starts_with('<')
                            && value.get("type").is_some()
                            && key.as_str() != "<subcommand>"
                            && key.as_str() != "<subcommand_args>"
                    })
                    .map(|(key, _)| (*key).clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn clap_positionals(command: &clap::Command) -> BTreeSet<String> {
        command
            .get_positionals()
            .map(|arg| format!("<{}>", arg.get_id().as_str()))
            .collect()
    }

    // 表里声明的 error_codes 至少要覆盖这些「实际会发」的码。
    ///
    /// 无法在测试里静态推导 daemon 端会发哪些码，所以这里列出本轮新增/易漂移的
    /// 关键对；新增分支时应同步往这里加一行。
    const REQUIRED_ERROR_CODES: &[(&str, &str)] = &[
        ("open", "invalid_open_target"),
        ("open", "invalid_timeout"),
        ("open", "unsupported_scheme"),
        ("open", "wait_for_timeout"),
        ("back", "invalid_timeout"),
        ("reload", "invalid_timeout"),
        ("capabilities", "daemon_not_running"),
        ("tabs", "no_tabs"),
        ("cleanup", "task_panicked"),
        ("job", "job_not_found"),
        ("trust-extension", "invalid_argument"),
    ];

    #[test]
    fn schema_semantics_match_clap() {
        let cli = Cli::command();
        let entries = schema_entries();

        // ---- 0) 命令总数：新增/删除命令必须同步 schema ----
        let clap_names: BTreeSet<String> = cli
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .filter(|name| name != "help")
            .collect();
        assert_eq!(
            entries.len(),
            61,
            "schema 表项数应为 61（新增命令需同步本断言）：{:?}",
            entries.iter().map(|entry| entry.name).collect::<Vec<_>>()
        );
        assert_eq!(clap_names.len(), 61, "clap 顶层命令数应为 61");

        let mut problems: Vec<String> = Vec::new();

        // ---- 1) error_codes 覆盖关键码 ----
        for (command, code) in REQUIRED_ERROR_CODES {
            let Some(entry) = entries.iter().find(|entry| entry.name == *command) else {
                problems.push(format!(
                    "schema 缺少命令 `{command}`，无法校验 error_code `{code}`"
                ));
                continue;
            };
            if !entry.error_codes.contains(code) {
                problems.push(format!(
                    "命令 `{command}` 的 error_codes 缺少 `{code}`（实际会发但表里没有）"
                ));
            }
        }

        // ---- 2) 逐命令语义比对（只覆盖无子命令的叶子命令） ----
        for entry in &entries {
            let Some(command) = cli
                .get_subcommands()
                .find(|sub| sub.get_name() == entry.name)
            else {
                continue;
            };
            // 带子命令的表项是扁平并集，default/enum 在不同子命令上本就不同，
            // 这里只做名称集合比对（第 1 个测试已覆盖）。
            if command.get_subcommands().next().is_some() {
                continue;
            }
            let args = clap_args_by_long(command);
            let table = entry.args.as_object();
            if let Some(table) = table {
                for (key, spec) in table {
                    let Some(long) = key.strip_prefix("--") else {
                        continue;
                    };
                    let Some(arg) = args.get(long) else {
                        continue; // 缺参数已由另一个测试报出
                    };
                    let schema_type = spec
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let action = arg.get_action();
                    let schema_required = spec
                        .get("required")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if schema_required != arg.is_required_set() {
                        problems.push(format!(
                            "`{} {key}`：required 不一致（表={schema_required}，clap={}）",
                            entry.name,
                            arg.is_required_set()
                        ));
                    }
                    // `Arg::get_action()` 返回 `&ArgAction`（该枚举不是 Copy/PartialEq），
                    // 用 matches! 做引用匹配最稳。
                    if matches!(action, clap::ArgAction::SetTrue) {
                        // 布尔开关：表里必须是 bool，默认只能是 false。
                        if schema_type != "bool" {
                            problems.push(format!(
                                "`{} {key}`：clap 是开关，表里 type={schema_type}（应为 bool）",
                                entry.name
                            ));
                        }
                        if let Some(default) = spec.get("default") {
                            if default != &serde_json::Value::Bool(false) {
                                problems.push(format!(
                                    "`{} {key}`：布尔开关的默认值应为 false，表里是 {default}",
                                    entry.name
                                ));
                            }
                        }
                    } else {
                        if schema_type == "bool" {
                            problems.push(format!(
                                "`{} {key}`：表里写成 bool，但 clap 需要取值（不是开关）",
                                entry.name
                            ));
                        }
                        let schema_default = spec.get("default").map(value_to_default_string);
                        let clap_default = arg
                            .get_default_values()
                            .first()
                            .map(|value| value.to_string_lossy().into_owned());
                        match (&schema_default, &clap_default) {
                            (None, None) => {}
                            (Some(a), Some(b)) if normalize_default(a) == normalize_default(b) => {}
                            _ => problems.push(format!(
                                "`{} {key}`：default 不一致（表={:?}，clap={:?}）",
                                entry.name, schema_default, clap_default
                            )),
                        }
                    }
                    // enum：clap 声明的可选值必须都在表里（表可以更宽松，但不能漏）。
                    //
                    // 布尔开关（SetTrue）的 `true`/`false` 是 clap 自动派生的取值，
                    // 不是业务枚举；表里的 `type:"bool"` 已经表达了这个语义，
                    // 把它当成「漏写 enum」会让每个开关都误报。
                    let clap_values: Vec<String> = if matches!(action, clap::ArgAction::SetTrue) {
                        Vec::new()
                    } else {
                        arg.get_possible_values()
                            .iter()
                            .filter(|value| !value.is_hide_set())
                            .map(|value| value.get_name().to_string())
                            .collect()
                    };
                    if !clap_values.is_empty() {
                        let schema_values: Vec<String> = spec
                            .get("enum")
                            .and_then(serde_json::Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        for value in clap_values {
                            if !schema_values.contains(&value) {
                                problems.push(format!(
                                    "`{} {key}`：clap 可选值 `{value}` 未写进表里的 enum {:?}",
                                    entry.name, schema_values
                                ));
                            }
                        }
                    }
                    // variadic：Append 型参数（Vec）必须在表里标 variadic。
                    let schema_variadic = spec
                        .get("variadic")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let clap_variadic = matches!(action, clap::ArgAction::Append);
                    if schema_variadic != clap_variadic {
                        problems.push(format!(
                            "`{} {key}`：variadic 不一致（表={schema_variadic}，clap={clap_variadic}）",
                            entry.name
                        ));
                    }
                }
            }
            // 位置参数：名字、数量与 required 都要对上（`<target>` ↔ clap 的 `target` 字段）。
            let table_pos = table_positionals(&entry.args);
            let clap_pos = clap_positionals(command);
            if table_pos != clap_pos {
                problems.push(format!(
                    "`{}`：位置参数不一致（表={:?}，clap={:?}）",
                    entry.name, table_pos, clap_pos
                ));
            }
            for arg in command.get_positionals() {
                let key = format!("<{}>", arg.get_id().as_str());
                let Some(spec) = entry.args.get(key.as_str()) else {
                    continue;
                };
                let schema_required = spec
                    .get("required")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if schema_required != arg.is_required_set() {
                    problems.push(format!(
                        "`{} {key}`：位置参数 required 不一致（表={schema_required}，clap={}）",
                        entry.name,
                        arg.is_required_set()
                    ));
                }
            }
        }

        assert!(
            problems.is_empty(),
            "schema 语义漂移，共 {} 处：\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    /// 本轮新增的错误码必须「某条命令会发」+「顶层目录里有可执行的下一步」。
    ///
    /// 只加码不加 hint，Agent 拿到 `ok:false` 仍然只能猜；只加 hint 不加码，
    /// 逐命令的清单就查不到出处。
    #[test]
    fn error_code_hints_cover_new_codes() {
        let entries = schema_entries();
        let catalogue: BTreeMap<&str, (&str, bool)> = ERROR_CODE_HINTS
            .iter()
            .map(|&(code, hint, retryable)| (code, (hint, retryable)))
            .collect();

        // (code, retryable, 必须声明它的命令)
        let expected: &[(&str, bool, &[&str])] = &[
            (
                "ws_backpressure",
                true,
                &["click", "exec", "wait", "snapshot"],
            ),
            ("ws_disconnected", true, &["click", "read", "find"]),
            ("task_panicked", true, &["cleanup", "job"]),
            ("wait_js_failed", true, &["exec", "wait"]),
            ("wait_js_eval_blocked", false, &["exec", "wait"]),
            ("too_many_rules", false, &["net"]),
            ("net_rule_too_large", false, &["net"]),
        ];
        for &(code, retryable, commands) in expected {
            let (hint, flag) = catalogue
                .get(code)
                .unwrap_or_else(|| panic!("error_code_hints 缺少 {code}"));
            assert_eq!(*flag, retryable, "{code} 的 retryable 口径不对");
            assert!(!hint.trim().is_empty(), "{code} 的 hint 不能为空");
            for command in commands {
                let entry = entries
                    .iter()
                    .find(|entry| entry.name == *command)
                    .unwrap_or_else(|| panic!("schema 缺少命令 {command}"));
                assert!(
                    entry.error_codes.contains(&code),
                    "`{command}` 的 error_codes 缺少 {code}"
                );
            }
        }

        // 页面命令一律带上 WS 下行通道的两类失败码（push 里自动补全，新增命令不会漏）。
        for name in [
            "click",
            "mouse-click",
            "fill",
            "send-keys",
            "screenshot",
            "save-pdf",
            "scan",
            "exec",
            "cookies",
            "upload",
            "snapshot",
            "read",
            "find",
            "inject",
            "frames",
            "wait",
            "hover",
            "select",
            "check",
            "scroll",
            "back",
            "reload",
        ] {
            let entry = entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("schema 缺少命令 {name}"));
            for code in ["ws_backpressure", "ws_disconnected"] {
                assert!(entry.error_codes.contains(&code), "`{name}` 缺少 {code}");
            }
        }

        // 目录自洽：没有重复码、每条 hint 都非空。
        let mut seen = BTreeMap::new();
        for &(code, hint, _) in ERROR_CODE_HINTS {
            assert!(!hint.trim().is_empty(), "{code} 的 hint 不能为空");
            assert!(
                seen.insert(code, ()).is_none(),
                "error_code_hints 里 {code} 重复"
            );
        }
    }

    // v11：`--world` 只接受 main / isolated（大小写不敏感），非法值解析期 exit 2。
    ///
    /// exec 与 inject 都走扩展的 world 选择：自由字符串会被静默落成 MAIN，
    /// 用户以为切到了 isolated（不受页面 CSP 影响）其实没有。
    #[test]
    fn world_flag_rejects_unknown_values() {
        use clap::Parser;
        for command in ["exec", "inject"] {
            for bad in ["foo", "isolate", "MAIN ", "main,isolated"] {
                assert!(
                    Cli::try_parse_from(["atb", command, "return 1", "--world", bad]).is_err(),
                    "`{command} --world {bad}` 应该被拒绝"
                );
            }
            for good in ["main", "isolated", "ISOLATED", "Main"] {
                assert!(
                    Cli::try_parse_from(["atb", command, "return 1", "--world", good]).is_ok(),
                    "`{command} --world {good}` 应该被接受"
                );
            }
        }
    }
}
