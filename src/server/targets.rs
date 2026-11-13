use super::*;

/// 解析后的目标标签页。字段都取自 daemon 当前的会话快照，解析完就固定下来，
/// 后面并发执行时不再受「列表又变了」影响。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTarget {
    pub(crate) index: usize,
    pub(crate) session_key: String,
    pub(crate) tab_id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) group_title: Option<String>,
    pub(crate) active: bool,
}

/// 目标解析失败：带候选列表，智能体不用再跑一次 tabs 去猜。
///
/// 旧实现只有 code/message/hint：`no_tabs` 一个码背了「桥断 / 真没标签 / 标签属于
/// 另一 browser / 扩展过旧」四种病因，调用方无法决定重试策略。
/// 现在补 `retryable` + `retry_after_ms`，与 sessions.rs 的 SessionSelectError 同一契约。
pub(crate) struct TargetResolveError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) hint: String,
    pub(crate) candidates: Vec<Value>,
    pub(crate) retryable: bool,
    pub(crate) retry_after_ms: Option<u64>,
}

impl TargetResolveError {
    pub(crate) fn new(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            candidates: Vec::new(),
            // 旧路径全部按不可重试处理，行为不变；可重试病因走 `retryable()`。
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// 桥断开类失败：唯一「等一等就能好」的病因（约定 3000ms 后再试）。
    pub(crate) fn retryable(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
        retry_after_ms: u64,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            candidates: Vec::new(),
            retryable: true,
            retry_after_ms: Some(retry_after_ms),
        }
    }

    pub(crate) fn with_candidates(mut self, candidates: Vec<Value>) -> Self {
        self.candidates = candidates;
        self
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({
            "ok": false,
            "error": self.message,
            "error_code": self.code,
            "hint": self.hint,
            "retryable": self.retryable,
        });
        if let Some(delay) = self.retry_after_ms {
            value["retry_after_ms"] = json!(delay);
        }
        if !self.candidates.is_empty() {
            value["candidates"] = json!(self.candidates);
        }
        value
    }
}

/// 标签页候选摘要：只保留智能体做决定需要的字段，避免错误响应本身就很长。
///
/// 旧实现不带 browser_id / session_key：多浏览器场景下，智能体拿到 candidates
/// 还得再跑一次 lookup 才知道该补哪个 --browser。现在每项直接带归属（任务 2）。
pub(crate) fn tab_candidates(tabs: &[TabInfo], limit: usize) -> Vec<Value> {
    tabs.iter()
        .take(limit)
        .map(|tab| {
            json!({
                "tab_id": tab.tab_id,
                "session_key": tab.session_key,
                "browser_id": tab.browser_id,
                "profile_id": tab.profile_id,
                "title": truncate_for_tree(&tab.title, 60),
                "url": truncate_for_tree(&tab.url, 100),
                "group": tab.group_title,
                "active": tab.active,
            })
        })
        .collect()
}

/// tab id 是 Chrome 生成的递增整数；解析失败时排到最后，保证排序稳定。
pub(crate) fn tab_numeric_order(id: &str) -> u128 {
    id.parse::<u128>().unwrap_or(u128::MAX)
}

/// 统一顺序：browser → profile → tab id。
///
/// 会话存在 HashMap 里，直接 `values()` 收集出来的顺序每次都可能不同，
/// 会导致 `tabs` 输出乱序、`--tab @1` 指向漂移。这里固定一次，两处共用。
pub(crate) fn sort_tabs_canonical(tabs: &mut [TabInfo]) {
    tabs.sort_by(|left, right| {
        left.browser_id
            .cmp(&right.browser_id)
            .then_with(|| left.profile_id.cmp(&right.profile_id))
            .then_with(|| tab_numeric_order(&left.tab_id).cmp(&tab_numeric_order(&right.tab_id)))
            .then_with(|| left.tab_id.cmp(&right.tab_id))
    });
}

/// 目标解析结果：解析出的目标 + 被范围过滤掉的「跨 agent」目标数。
pub(crate) struct ResolvedTargets {
    pub(crate) targets: Vec<ResolvedTarget>,
    /// 因为属于**其它智能体**而被跳过的目标数；`scope:"all"` 时恒为 0。
    pub(crate) skipped_foreign: usize,
}

/// 这个标签是否属于当前 agent 打开的范围（`owned` 为 `None` 表示不限制）。
fn owned_by_agent(owned: &Option<HashSet<String>>, tab: &TabInfo) -> bool {
    owned
        .as_ref()
        .map(|keys| keys.contains(&tab.session_key))
        .unwrap_or(true)
}

/// 把 `--tab` 规格解析成具体的会话列表。
///
/// 这里是所有多目标命令的唯一入口：解析失败会给出候选标签页，
/// 成功则在执行任何页面操作之前就把目标固定下来（不会出现「执行了一半才发现目标不存在」）。
///
// `agent_scope`：`All` / `Ids` 是否按 `current_agent()` 的 `agent_opened` 过滤。
/// 默认 `true`（`/multi` 不给 scope 时由 `ATB_MULTI_SCOPE` 决定，默认 `agent`）。
pub(crate) async fn resolve_targets(
    state: &AppState,
    spec: &TargetSpec,
    browser: Option<&str>,
    profile: Option<&str>,
    agent_scope: bool,
) -> std::result::Result<ResolvedTargets, TargetResolveError> {
    wait_for_sessions(state, WAIT_SESSIONS_ACTION).await;
    let mut tabs = active_tabs_filtered(state, false, browser, profile)
        .await
        .map_err(|err| {
            // 旧码 target_unavailable 病因不明；上游 active_tabs_filtered 现在只会
            // 因 browser/profile 歧义失败，码改为 target_ambiguous（改过滤条件即可解）。
            TargetResolveError::new(
                "target_ambiguous",
                err.to_string(),
                "用 `atb tabs` 看当前有哪些标签页，再用 --tab 指定",
            )
        })?;
    sort_tabs_canonical(&mut tabs);
    let candidates = || tab_candidates(&tabs, 20);

    if tabs.is_empty() {
        // 旧实现四种病因共用一个 no_tabs：桥断开该等 3 秒重试、过滤错该改
        // --browser、真没标签该 open——调用方拿一个码分不出来。现在按快照细分。
        let driver = state.driver.read().await;
        let global_active = driver.sessions.values().filter(|s| s.is_active()).count();
        let err = if global_active > 0 && has_live_bridge(&driver) {
            // 全局有活会话、过滤后为空：标签在别的 browser/profile 名下。
            TargetResolveError::new(
                "session_mismatch",
                "当前没有可操作的标签页（标签存在，但都不匹配 --browser/--profile 过滤条件）",
                "标签属于另一个 browser/profile：去掉过滤条件，或改用标签所属的 browser（tabs 可看归属）",
            )
        } else if has_live_bridge(&driver) {
            TargetResolveError::new(
                "no_target_tabs",
                "当前没有可操作的标签页（扩展已连接但没有存活的标签页会话）",
                "先打开一个页面：`atb open https://example.com`",
            )
        } else {
            TargetResolveError::retryable(
                "extension_disconnected",
                "当前没有可操作的标签页（扩展桥未连接，浏览器可能未打开）",
                "扩展桥未连接通常是暂态：确认浏览器已打开后等 3 秒重试；持续失败用 status 检查扩展连接",
                3000,
            )
        };
        drop(driver);
        return Err(err);
    }

    let mut resolved: Vec<ResolvedTarget> = Vec::new();
    // TA：默认范围是「本 agent 通过 open 登记过的标签」。旧实现不做归属过滤，
    // `--tab all` 会把别人的标签一起操作（多智能体/多用户共用同一浏览器时串线）。
    let owned: Option<HashSet<String>> = if agent_scope {
        let opened = state.agent_opened.lock().await;
        Some(
            opened
                .get(&current_agent())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        )
    } else {
        None
    };
    let mut skipped_foreign = 0usize;
    match spec {
        TargetSpec::Default => {
            // 真实执行路径（`select_tab`）优先用「本智能体的默认目标」，dry-run 也必须
            // 用同一个，否则报告的目标与真正打的目标不是同一个（CLI 一致性）。
            let agent = current_agent();
            let agent_key = {
                let defaults = state.agent_defaults.lock().await;
                defaults
                    .get(&agent)
                    .and_then(|target| target.session_key.clone())
            };
            let default_key = state.driver.read().await.default_session_key.clone();
            let chosen = agent_key
                .as_deref()
                .and_then(|key| tabs.iter().find(|tab| tab.session_key == key))
                .or_else(|| {
                    default_key
                        .as_deref()
                        .and_then(|key| tabs.iter().find(|tab| tab.session_key == key))
                })
                .or_else(|| tabs.first());
            if let Some(tab) = chosen {
                resolved.push(resolved_from_tab(0, tab));
            }
        }
        TargetSpec::All => {
            for tab in tabs.iter() {
                if !owned_by_agent(&owned, tab) {
                    skipped_foreign += 1;
                    continue;
                }
                resolved.push(resolved_from_tab(resolved.len(), tab));
            }
        }
        TargetSpec::Latest => {
            // 语义优先级：daemon 记录的 latest_session_key（最近 open/连接）
            // → connected_at 最大 → 列表最后一个。始终给一个确定结果，不静默退回 @1。
            let latest_key = state.driver.read().await.latest_session_key.clone();
            let chosen = latest_key
                .as_deref()
                .and_then(|key| tabs.iter().find(|tab| tab.session_key == key))
                .or_else(|| {
                    tabs.iter()
                        .filter(|tab| tab.connected_at.is_some())
                        .max_by(|left, right| {
                            left.connected_at
                                .unwrap_or(f64::NEG_INFINITY)
                                .partial_cmp(&right.connected_at.unwrap_or(f64::NEG_INFINITY))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                })
                .or_else(|| tabs.last());
            match chosen {
                Some(tab) => resolved.push(resolved_from_tab(0, tab)),
                None => {
                    return Err(TargetResolveError::new(
                        "target_not_found",
                        "没有可用的最新标签页",
                        "先 `atb open <url>` 打开一个页面，再用 --tab @latest",
                    )
                    .with_candidates(candidates()));
                }
            }
        }
        TargetSpec::Current => {
            if let Some(tab) = tabs.iter().find(|tab| tab.active) {
                resolved.push(resolved_from_tab(0, tab));
            } else {
                return Err(TargetResolveError::new(
                    "target_not_found",
                    "浏览器没有上报当前激活的标签页",
                    "旧版扩展不上报 active 状态；改用 `--tab <tabId>` 或 `--tab @1` 指定",
                )
                .with_candidates(candidates()));
            }
        }
        TargetSpec::Ids(ids) => {
            let mut missing: Vec<String> = Vec::new();
            let mut ambiguous: Vec<String> = Vec::new();
            for id in ids {
                let all_matches: Vec<&TabInfo> = tabs
                    .iter()
                    .filter(|tab| tab.tab_id == *id || tab.session_key == *id)
                    .collect();
                // TA：先按归属过滤，再判 0/1/多。这样「同一个 tab_id 在别的
                // profile 里也有一份」不会被误报成歧义——不属于本 agent 的那份
                // 本来就不该被看见。
                let matches: Vec<&TabInfo> = all_matches
                    .iter()
                    .copied()
                    .filter(|tab| owned_by_agent(&owned, tab))
                    .collect();
                if matches.is_empty() && !all_matches.is_empty() {
                    // 显式点名的标签存在，但属于别的智能体：计数后跳过，
                    // 不当作「id 写错」（那会把跨 agent 访问说成参数错误）。
                    skipped_foreign += 1;
                    continue;
                }
                match matches.len() {
                    0 => missing.push(id.clone()),
                    // 同一个 tab_id 可能同时属于多个 profile；单目标端点会报歧义，
                    // `/multi` 以前静默取首个，导致「同一写法、不同目标」（CLI 一致性）。
                    1 => resolved.push(resolved_from_tab(resolved.len(), matches[0])),
                    _ => ambiguous.push(format!(
                        "{id}（{} 个会话：{}）",
                        matches.len(),
                        matches
                            .iter()
                            .map(|tab| tab.session_key.as_str())
                            .take(4)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
            if !ambiguous.is_empty() {
                return Err(TargetResolveError::new(
                    "target_ambiguous",
                    format!("目标不唯一：{}", ambiguous.join("；")),
                    "用完整的 session_key（含 profile）指定，或先 `atb lookup --tab <id>` 确认归属",
                )
                .with_candidates(candidates()));
            }
            if !missing.is_empty() {
                // 旧实现一律 target_not_found「找不到标签页」：跨 browser / 扩展重载后
                // 整批 tab_id 失效时，把「实例下线」说成「id 写错」，误导排障。
                // 现在查全表（含已断开）细分三种病因。
                let driver = state.driver.read().await;
                let active_hit = missing.iter().any(|id| {
                    driver
                        .sessions
                        .values()
                        .any(|s| (s.tab_id == *id || s.session_key == *id) && s.is_active())
                });
                let stale_hit = missing.iter().find_map(|id| {
                    driver
                        .sessions
                        .values()
                        .find(|s| (s.tab_id == *id || s.session_key == *id) && !s.is_active())
                });
                let err = if active_hit {
                    // 会话活着、只是被过滤条件排除：目标属于另一个 browser。
                    TargetResolveError::new(
                        "session_mismatch",
                        format!("找不到标签页：{}（会话存在，但不在当前 --browser/--profile 过滤范围内）", missing.join(", ")),
                        "该标签属于另一个 browser/profile：去掉过滤条件，或改用标签所属的 browser（tabs 可看归属）",
                    )
                    .with_candidates(candidates())
                } else if let Some(session) = stale_hit {
                    let dead = dead_tab_count_of_browser(&driver, &session.browser_id);
                    TargetResolveError::new(
                        "session_gone",
                        format!(
                            "找不到标签页：{}（其所属浏览器实例已下线，名下 {dead} 个标签失效）",
                            missing.join(", ")
                        ),
                        "不要复用旧 tab_id：重新 open 目标页面，或先 tabs 拿新列表",
                    )
                } else if !has_live_bridge(&driver) {
                    TargetResolveError::retryable(
                        "extension_disconnected",
                        format!("找不到标签页：{}（扩展桥未连接）", missing.join(", ")),
                        "扩展桥未连接通常是暂态：确认浏览器已打开后等 3 秒重试；持续失败用 status 检查",
                        3000,
                    )
                } else {
                    // 桥活着、全表也没有这个 id：就是 id 写错 / 标签已单独关闭。
                    TargetResolveError::new(
                        "target_not_found",
                        format!("找不到标签页：{}", missing.join(", ")),
                        "标签可能已关闭；下面 candidates 是当前可用的标签页，\
                         也可以用 `--tab @1` 按序号指定",
                    )
                    .with_candidates(candidates())
                };
                drop(driver);
                return Err(err);
            }
        }
        TargetSpec::Index(indexes) => {
            let mut out_of_range: Vec<String> = Vec::new();
            for index in indexes {
                if *index == 0 || *index > tabs.len() {
                    out_of_range.push(format!("@{index}"));
                    continue;
                }
                resolved.push(resolved_from_tab(resolved.len(), &tabs[index - 1]));
            }
            if !out_of_range.is_empty() {
                return Err(TargetResolveError::new(
                    "target_out_of_range",
                    format!(
                        "序号 {} 越界：当前共 {} 个标签页（@1..@{})",
                        out_of_range.join(", "),
                        tabs.len(),
                        tabs.len()
                    ),
                    "序号的顺序就是 `tabs` 的输出顺序；先跑 tabs 确认再传",
                )
                .with_candidates(candidates()));
            }
        }
        TargetSpec::Url(needle) | TargetSpec::Title(needle) => {
            let needle = needle.to_lowercase();
            let matched: Vec<&TabInfo> = tabs
                .iter()
                .filter(|tab| match spec {
                    TargetSpec::Url(_) => tab.url.to_lowercase().contains(&needle),
                    _ => tab.title.to_lowercase().contains(&needle),
                })
                .collect();
            if matched.is_empty() {
                return Err(TargetResolveError::new(
                    "target_not_found",
                    format!(
                        "没有标签页的{}包含 {needle:?}",
                        if matches!(spec, TargetSpec::Url(_)) {
                            " URL "
                        } else {
                            "标题"
                        }
                    ),
                    "匹配是子串、不区分大小写；下面 candidates 是当前全部标签页",
                )
                .with_candidates(candidates()));
            }
            resolved = matched
                .iter()
                .enumerate()
                .map(|(index, tab)| resolved_from_tab(index, tab))
                .collect();
        }
        TargetSpec::Window(window_id) => {
            let matched: Vec<&TabInfo> = tabs
                .iter()
                .filter(|tab| tab.window_id == Some(*window_id))
                .collect();
            if matched.is_empty() {
                return Err(TargetResolveError::new(
                    "target_not_found",
                    format!("没有属于窗口 {window_id} 的标签页"),
                    "窗口 id 见 `tabs --ids` 每项的 window 字段；窗口可能已被关闭",
                )
                .with_candidates(candidates()));
            }
            resolved = matched
                .iter()
                .enumerate()
                .map(|(index, tab)| resolved_from_tab(index, tab))
                .collect();
        }
        TargetSpec::Group(title) => {
            let wanted = title.to_lowercase();
            let matched: Vec<&TabInfo> = tabs
                .iter()
                .filter(|tab| {
                    // `wanted` 已经是小写；`eq_ignore_ascii_case` 只处理 ASCII，
                    // 非 ASCII 大写标题（CAFÉ / ЗАКАЗ）永远匹配不上。
                    // 正确做法：两组都用 `to_lowercase()`（Unicode 感知）比较。
                    tab.group_title
                        .as_deref()
                        .map(|group| group.to_lowercase() == wanted)
                        .unwrap_or(false)
                })
                .collect();
            if matched.is_empty() {
                return Err(TargetResolveError::new(
                    "target_not_found",
                    format!("没有标题为 {title:?} 的标签组"),
                    "标签组标题需要扩展 3.5+ 上报；用 `open --group-title <名字>` 创建，\
                     或用 `--tab url:...` 按 URL 选",
                )
                .with_candidates(candidates()));
            }
            resolved = matched
                .iter()
                .enumerate()
                .map(|(index, tab)| resolved_from_tab(index, tab))
                .collect();
        }
    }

    if resolved.is_empty() {
        if skipped_foreign > 0 {
            // TA：全部目标都被归属过滤掉了——这是「范围」问题，不是「没有标签」，
            // 直接说清楚并给出逃生口，否则调用方只会反复重试或以为浏览器空了。
            return Err(TargetResolveError::new(
                "target_scope_empty",
                format!(
                    "目标都属于其它智能体（按 agent 范围过滤掉 {skipped_foreign} 个），当前没有可操作目标"
                ),
                "确认要跨智能体操作时显式传 scope:\"all\"（或设 ATB_MULTI_SCOPE=all）；否则先用 atb open 打开自己的标签",
            )
            .with_candidates(candidates()));
        }
        return Err(TargetResolveError::new(
            // 旧码 no_targets 病因不明；统一为 no_target_tabs（桥/会话没问题，
            // 只是过滤或解析后没有目标），与 select_tab 路径共用同一语义。
            "no_target_tabs",
            "目标解析结果为空",
            "检查 --browser / --profile 过滤条件是否把所有标签页都排除了",
        )
        .with_candidates(candidates()));
    }
    // `--tab @1,@1` / `--tab 111,111` 会把同一个会话解析成两个目标，`/multi` 于是并发
    // 执行两次同一条命令：提交按钮被点两次、输入被追加两次。
    // 按 session_key 去重，保留首次出现的 index。
    let mut seen: HashSet<String> = HashSet::with_capacity(resolved.len());
    resolved.retain(|target| seen.insert(target.session_key.clone()));
    for (index, target) in resolved.iter_mut().enumerate() {
        target.index = index;
    }
    Ok(ResolvedTargets {
        targets: resolved,
        skipped_foreign,
    })
}

/// 这批选择器分别依赖扩展上报哪些字段。返回 (人读特性名, 最低协议版本)。
pub(crate) fn spec_requires_protocol(spec: &TargetSpec) -> Option<(&'static str, u32)> {
    match spec {
        TargetSpec::Window(_) => Some((
            "按窗口选择标签页",
            crate::protocol::EXTENSION_PROTOCOL_VERSION,
        )),
        TargetSpec::Group(_) => Some(("按标签组选择", crate::protocol::EXTENSION_PROTOCOL_VERSION)),
        TargetSpec::Current => Some((
            "选择当前激活标签页",
            crate::protocol::EXTENSION_PROTOCOL_VERSION,
        )),
        _ => None,
    }
}

/// 检查已连接的扩展协议版本是否满足某项能力。
///
/// 没有扩展连接时放行：真正的错误会在后续「没有可操作标签页」里给出，
/// 这里只负责「版本不配套」这一类可修复的问题。
pub(crate) async fn ensure_extension_protocol(
    state: &AppState,
    min_protocol: u32,
    feature: &str,
) -> std::result::Result<(), Value> {
    let (protocols, versions) = {
        let driver = state.driver.read().await;
        let mut protocols: Vec<u32> = driver
            .sessions
            .values()
            .filter(|session| session.is_active())
            .map(|session| session.extension_protocol)
            .collect();
        protocols.sort_unstable();
        protocols.dedup();
        let mut versions: Vec<String> = driver
            .sessions
            .values()
            .filter(|session| session.is_active())
            .filter_map(|session| session.extension_version.clone())
            .collect();
        versions.sort();
        versions.dedup();
        (protocols, versions)
    };
    if protocols.is_empty() || protocols.iter().all(|value| *value >= min_protocol) {
        return Ok(());
    }
    Err(json!({
        "ok": false,
        "error": format!(
            "当前扩展不支持{feature}：需要扩展协议 {min_protocol}+，已连接版本协议 {:?}（扩展 {}）",
            protocols,
            if versions.is_empty() { "未知".to_string() } else { versions.join("/") }
        ),
        "error_code": "extension_outdated",
        "hint": "扩展和 CLI 必须配套；按 fix 里的三步更新扩展后重试",
        "fix": [
            "1) 用本仓库 assets/atb_bridge 覆盖你加载的扩展目录（旧版没有这些字段）",
            "2) 打开 chrome://extensions（Edge 是 edge://extensions），对本扩展点「重新加载」",
            "3) 执行 atb status，确认 compatibility.ok=true 后再重试这条命令"
        ],
        "compatibility": compatibility_value(state).await,
    }))
}

/// 三方版本兼容报告：CLI 协议 ←→ daemon 协议 ←→ 扩展协议。
pub(crate) async fn compatibility_value(state: &AppState) -> Value {
    let (protocols, versions, extensions) = {
        let driver = state.driver.read().await;
        let mut protocols: Vec<u32> = driver
            .sessions
            .values()
            .filter(|session| session.is_active())
            .map(|session| session.extension_protocol)
            .collect();
        protocols.sort_unstable();
        protocols.dedup();
        let mut versions: Vec<String> = driver
            .sessions
            .values()
            .filter(|session| session.is_active())
            .filter_map(|session| session.extension_version.clone())
            .collect();
        versions.sort();
        versions.dedup();
        // 每个浏览器只报一条：一个 profile 下会有几十个标签页会话，
        // 按会话逐条列会让错误响应里塞满重复项。
        let mut seen: HashSet<String> = HashSet::new();
        let extensions: Vec<Value> = driver
            .sessions
            .values()
            .filter(|session| session.is_active())
            .filter(|session| {
                seen.insert(format!(
                    "{}:{}:{}",
                    session.browser_id,
                    session.extension_version.clone().unwrap_or_default(),
                    session.extension_protocol
                ))
            })
            .map(|session| {
                json!({
                    "browser_id": session.browser_id,
                    "version": session.extension_version,
                    "protocol": session.extension_protocol,
                })
            })
            .collect();
        (protocols, versions, extensions)
    };
    let expected = crate::protocol::EXTENSION_PROTOCOL_VERSION;
    let outdated: Vec<u32> = protocols
        .iter()
        .copied()
        .filter(|value| *value < expected)
        .collect();
    let mut issues: Vec<Value> = Vec::new();
    let mut fixes: Vec<String> = Vec::new();
    if !outdated.is_empty() {
        issues.push(json!({
            "kind": "extension_outdated",
            "detail": format!(
                "扩展协议 {:?} 低于 daemon 期望的 {expected}（扩展 {}）",
                outdated,
                versions.join("/")
            ),
        }));
        fixes.push(
            "用本仓库 assets/atb_bridge 覆盖扩展目录，然后在 chrome://extensions 重新加载"
                .to_string(),
        );
        fixes.push("重载后执行 atb status 确认 compatibility.ok=true".to_string());
    }
    json!({
        "ok": issues.is_empty(),
        "cli_protocol": crate::protocol::PROTOCOL_VERSION,
        "daemon_protocol": crate::protocol::PROTOCOL_VERSION,
        "extension_protocol_expected": expected,
        "extension_protocol_connected": protocols,
        "extension_versions": versions,
        "extensions": extensions,
        "issues": issues,
        "fixes": fixes,
    })
}

pub(crate) fn resolved_from_tab(index: usize, tab: &TabInfo) -> ResolvedTarget {
    ResolvedTarget {
        index,
        session_key: tab.session_key.clone(),
        tab_id: tab.tab_id.clone(),
        title: tab.title.clone(),
        url: tab.url.clone(),
        group_title: tab.group_title.clone(),
        active: tab.active,
    }
}
