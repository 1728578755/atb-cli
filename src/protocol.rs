use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// daemon ↔ CLI 的 API 协议版本。
///
/// 只在「请求/响应语义有不兼容变化」时 +1。CLI 连上 daemon 后先比对两边版本，
/// 不一致就明确告诉用户该重启 daemon 还是该升级 CLI，而不是让命令以奇怪的方式失败。
pub const PROTOCOL_VERSION: u32 = 5;

/// 扩展协议版本：扩展与 daemon 之间的 WebSocket 消息契约。
///
/// 与 `PROTOCOL_VERSION` 分开：CLI 升级不影响扩展，扩展升级也不强制重启 daemon。
pub const EXTENSION_PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: String,
    /// Chrome 窗口 id：同一个 Profile 可以开多个窗口，按窗口批量操作时用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<i64>,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub browser_id: String,
    #[serde(default)]
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_label: Option<String>,
    /// 可读的浏览器标识：「Edge · Windows · 工作机」。多浏览器/多 profile 同时连着时
    /// 用来看懂「这条标签属于哪个浏览器」；旧扩展不上报时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_label: Option<String>,
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "type", default = "default_tab_type")]
    pub tab_type: String,
    /// Chrome 标签组标题（扩展未上报时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_title: Option<String>,
    /// 浏览器当前激活的标签页。
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub connected_at: Option<f64>,
}

fn default_tab_type() -> String {
    "ext_ws".to_string()
}

#[derive(Debug, Clone)]
pub struct Session {
    pub session_key: String,
    pub tab_id: String,
    pub browser_id: String,
    pub profile_id: String,
    pub profile_label: Option<String>,
    pub extension_version: Option<String>,
    /// 扩展上报的消息协议版本；旧扩展不报，视为 0（未知/过旧）。
    pub extension_protocol: u32,
    pub file_scheme_access: Option<bool>,
    pub info: TabInfo,
    /// 这条会话所属扩展连接的下行通道。
    ///
    // 有界通道。发送必须走 `server::send_downstream`，让「队列满」
    /// 变成结构化 `ws_backpressure`，而不是在内存里无限积压。
    pub sender: mpsc::Sender<String>,
    pub disconnected_at: Option<Instant>,
    // 最近一次注册这条会话的 WebSocket 连接 id。
    ///
    /// `session_key` 是确定性拼接（browser/profile/tab），扩展重载后新连接会
    /// **拿到同一个 key**。断开清理若只看 key 就会把新连接刚注册的会话标成断开再删掉，
    /// 于是「刚重连就报没有可用标签页」。记下登记者的连接 id 之后，旧连接收尾时
    /// 先比对 `conn_id`，不等就整条跳过（连 `forget_session` 都不做）。
    pub conn_id: u64,
}

impl Session {
    pub fn is_active(&self) -> bool {
        self.disconnected_at.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// 保留字段（当前恒为 `None`，无写入点）。
    ///
    /// 扩展回包只有 `data` / `newTabs` 两个真实载荷，`result` / `closed` 是早期
    /// 设计的遗留：读取点都写成 `data.or(result)`，属于历史上的空转。读取点分散在
    /// 6 处，删除属于纯清理（不改语义），为避免一次性扩大改动面暂留，标注备用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 保留字段（当前恒为 `None`，无写入点），见 `result`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "newTabs")]
    pub new_tabs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RectInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementDomInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<RectInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementRef {
    pub ref_id: String,
    /// AX 快照的来源：CDP backendNodeId；JS 快照时为 0。
    pub backend_dom_node_id: i64,
    /// JS 快照产生的 CSS 选择器；有值时 `@e` 动作可完全走页面内执行，不依赖 CDP。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    pub index: usize,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom: Option<ElementDomInfo>,
}

#[derive(Debug, Clone)]
pub struct SnapshotCache {
    pub generation: u64,
    pub url: String,
    pub refs: HashMap<String, ElementRef>,
}

#[derive(Debug)]
pub struct PendingExec {
    pub delivered_at: Option<Instant>,
    pub tx: oneshot::Sender<anyhow::Result<ExecResult>>,
}

#[derive(Default)]
pub struct DriverState {
    pub sessions: HashMap<String, Session>,
    pub snapshots: HashMap<String, SnapshotCache>,
    pub default_session_key: Option<String>,
    pub latest_session_key: Option<String>,
    pub preferred_default_session_key: Option<String>,
    // `preferred_default_session_key` 的登记时刻。
    ///
    /// 与 key **同生共死**：设置走 [`DriverState::set_preferred_default_session`]、
    /// 清理走 [`DriverState::clear_preferred_default_session`]，任何一处漏写都会让
    /// 这份记忆变成永不过期的脏数据（默认目标再也建立不起来）。
    ///
    // TTL 判定已改用下面的单调时刻，本字段只作为**兼容旧值/测试**的
    /// 回退来源（外部按 unix 秒直接写入时仍能按非负年龄判过期）。
    pub preferred_default_session_at: Option<f64>,
    // 登记时刻的单调时钟版本。
    ///
    /// 系统时钟回拨时 unix 秒差会变成负数，旧判据 `now - at < TTL` 会把一条
    /// 早已过期的记忆判成「永远有效」。`Instant` 不受墙钟调整影响，是本进程内
    /// 唯一可信的时间来源；它只在进程内存活，正好符合这份记忆的语义。
    pub(crate) preferred_default_session_at_instant: Option<Instant>,
    /// 每个 browser:profile 的 net 配置（含页面规则与网络层规则），跨标签共享。
    pub net_hooks: HashMap<String, Value>,
}

/// 「待生效默认目标」的记忆有效时长（秒）。
///
/// 6 小时：够覆盖浏览器重启、隔夜再打开这种正常节奏；又不至于让一个「标签早就没了」
/// 的记忆永久卡住默认目标的自动建立（`should_set_default_session` 见到 preferred
/// 就不再改选）。
pub const PREFERRED_DEFAULT_TTL: f64 = 6.0 * 3600.0;
/// 同上，`Duration` 形式（给单调时钟判据用）。
const PREFERRED_DEFAULT_TTL_DURATION: Duration = Duration::from_secs(6 * 3600);

impl DriverState {
    /// 当前仍然有效的「待生效默认目标」。
    ///
    /// 过期的记忆在这里**只当作不存在**，不清理字段（`&self` 清不了）：清理由
    /// [`Self::expire_preferred_default_session`] 在快照入口做一次。
    ///
    // 判据：
    /// 1. key 与登记时刻**同生共死**——key 有值但没有任何时间戳时视为无效，
    ///    不再把「字段被直接写坏」的旧值当成永久有效；
    /// 2. 优先用单调时钟 `Instant`：系统时钟回拨不会让 TTL 失效；
    /// 3. 只有单调时刻缺失（旧值/测试直接写 unix 秒）才退回 unix 秒，
    ///    且要求 `age >= 0`——负年龄（时钟回拨造成的「未来时间戳」）按过期处理。
    pub fn preferred_default_session(&self) -> Option<&str> {
        let key = self.preferred_default_session_key.as_deref()?;
        if let Some(at) = self.preferred_default_session_at_instant {
            return (at.elapsed() < PREFERRED_DEFAULT_TTL_DURATION).then_some(key);
        }
        let at = self.preferred_default_session_at?;
        let age = now_unix_secs() - at;
        (0.0..PREFERRED_DEFAULT_TTL).contains(&age).then_some(key)
    }

    /// 记忆**已经过期**就清掉，返回是否真的清理了。
    pub fn expire_preferred_default_session(&mut self) -> bool {
        if self.preferred_default_session_key.is_none() {
            return false;
        }
        if self.preferred_default_session().is_some() {
            return false;
        }
        self.clear_preferred_default_session();
        true
    }

    /// 记下「待生效默认目标」，同时刷新时间戳（两者必须一起写）。
    pub fn set_preferred_default_session(&mut self, session_key: String) {
        self.preferred_default_session_key = Some(session_key);
        self.preferred_default_session_at = Some(now_unix_secs());
        self.preferred_default_session_at_instant = Some(Instant::now());
    }

    /// 清掉记忆：key 与时间戳一起清，避免残留时间戳污染下一次设置。
    pub fn clear_preferred_default_session(&mut self) {
        self.preferred_default_session_key = None;
        self.preferred_default_session_at = None;
        self.preferred_default_session_at_instant = None;
    }
}

fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

/// 在途执行登记表：从 DriverState 拆出来单独一把锁。
///
/// 每个 exec 结果、每个 ACK 都要动这张表。以前它们和 `sessions` 共用一把全局
/// Mutex，等于所有命令的结果回传都要抢同一把锁；拆开后结果回传只碰这张小表。
#[derive(Default)]
pub struct ExecRegistry {
    pub pending: HashMap<String, PendingExec>,
    pub acked: HashSet<String>,
    /// exec_id -> session_key：扩展断开/标签关闭时按会话批量失败。
    pub active: HashMap<String, String>,
}

impl ExecRegistry {
    pub fn register(&mut self, exec_id: &str, session_key: &str, pending: PendingExec) {
        self.pending.insert(exec_id.to_string(), pending);
        self.active
            .insert(exec_id.to_string(), session_key.to_string());
    }

    /// 取走一个执行并清干净：结果、错误、超时、断开都走这里。
    pub fn take(&mut self, exec_id: &str) -> Option<PendingExec> {
        self.acked.remove(exec_id);
        self.active.remove(exec_id);
        self.pending.remove(exec_id)
    }

    /// 某个会话断开/关闭时，把它名下的在途执行全部取走。
    pub fn take_session(&mut self, session_key: &str) -> Vec<PendingExec> {
        let ids: Vec<String> = self
            .active
            .iter()
            .filter(|(_, key)| key.as_str() == session_key)
            .map(|(id, _)| id.clone())
            .collect();
        ids.iter().filter_map(|id| self.take(id)).collect()
    }

    /// 清理已失效会话名下的登记（会话被 prune 掉时用）。
    pub fn retain_sessions(&mut self, alive: &[String]) {
        let dead: Vec<String> = self
            .active
            .iter()
            .filter(|(_, key)| !alive.iter().any(|item| item == *key))
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            self.take(&id);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum WsIncoming {
    #[serde(rename = "ext_ready")]
    ExtReady {
        #[serde(default = "default_browser_id")]
        browser_id: String,
        /// 可读的浏览器标识（品牌 + 系统 + 自定义名），多浏览器并行时用来区分谁是谁。
        #[serde(default)]
        browser_label: Option<String>,
        #[serde(default = "default_profile_id")]
        profile_id: String,
        #[serde(default)]
        profile_label: Option<String>,
        #[serde(default)]
        extension_version: Option<String>,
        /// 扩展协议版本；旧扩展不报 → 0，按「过旧」处理。
        #[serde(default)]
        protocol: u32,
        #[serde(default)]
        file_scheme_access: Option<bool>,
        /// 扩展自检发现的问题（`queryTabs` / `tabGroups` / `restartSeq` 之类异常）。
        ///
        /// 扩展在 `connectWS` 的 finally 块里会发这个数组，但 daemon 侧长期没有建模，
        /// serde 直接丢弃 —— 用户看不到「为什么这次上报的 tabs 是空的」（R-19）。
        #[serde(default)]
        warnings: Vec<Value>,
        #[serde(default, deserialize_with = "deserialize_ext_tabs")]
        tabs: Vec<ExtTab>,
    },
    #[serde(rename = "tabs_update")]
    TabsUpdate {
        #[serde(default = "default_browser_id")]
        browser_id: String,
        #[serde(default)]
        browser_label: Option<String>,
        #[serde(default = "default_profile_id")]
        profile_id: String,
        #[serde(default)]
        profile_label: Option<String>,
        #[serde(default)]
        extension_version: Option<String>,
        #[serde(default)]
        protocol: u32,
        #[serde(default)]
        file_scheme_access: Option<bool>,
        #[serde(default)]
        warnings: Vec<Value>,
        #[serde(default, deserialize_with = "deserialize_ext_tabs")]
        tabs: Vec<ExtTab>,
    },
    #[serde(rename = "ack")]
    Ack { id: String },
    #[serde(rename = "result")]
    Result {
        id: String,
        result: Value,
        #[serde(rename = "newTabs")]
        new_tabs: Option<Value>,
    },
    #[serde(rename = "error")]
    Error {
        id: String,
        error: Value,
        #[serde(rename = "newTabs")]
        new_tabs: Option<Value>,
    },
    #[serde(other)]
    Other,
}

/// 扩展上报的一个标签页。
///
/// 反序列化规则见下方手写的 `Deserialize`：`id` 必须是字符串或整数。
#[derive(Debug)]
pub struct ExtTab {
    pub id: Value,
    pub window_id: Option<i64>,
    pub url: String,
    pub title: String,
    /// 扩展上报的 Chrome 标签组标题（3.5+ 才有；旧扩展为 None）。
    pub group_title: Option<String>,
    /// 是否是浏览器窗口里当前激活（可见）的标签页。
    pub active: bool,
}

/// 手写 `Deserialize`：`id` 不合法时**拒绝这一条**。
///
/// 派生实现会把任意 JSON 收进 `Value`，于是对象/数组类型的 id 会被
/// `Value::to_string()` 变成 `{"a":1}` 这种伪 id，混进会话表后按 id 查找永远对不上
// 。调用方（`Vec<ExtTab>` 的逐条 `from_value`）会把 Err 当成「丢掉这条」。
///
// **负数与非正整数同样拒绝**。Chrome 的 tabId 恒为正整数，负值只能是
/// 扩展被篡改或上游协议写错；旧实现会让它一路进会话表，直到执行时才报错，归因失真。
/// 空字符串 / 纯空白同理——它拼出来的 `b:p:` 会让所有非法标签塌缩成同一个会话 key。
impl<'de> Deserialize<'de> for ExtTab {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawTab {
            id: Value,
            #[serde(default, alias = "windowId")]
            window_id: Option<i64>,
            #[serde(default)]
            url: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            group_title: Option<String>,
            #[serde(default)]
            active: bool,
        }

        let raw = RawTab::deserialize(deserializer)?;
        let id_ok = match &raw.id {
            Value::String(text) => !text.trim().is_empty(),
            // 正整数的两种表示：`as_i64` 覆盖 i64 范围，`as_u64` 覆盖
            // u64 大于 i64::MAX 的部分（此时 `as_i64` 返回 None）。
            Value::Number(_) => raw
                .id
                .as_i64()
                .map(|id| id > 0)
                .unwrap_or_else(|| raw.id.as_u64().is_some_and(|id| id > 0)),
            _ => false,
        };
        if !id_ok {
            return Err(<D::Error as serde::de::Error>::custom(format!(
                "invalid tab id: {}",
                raw.id
            )));
        }
        Ok(ExtTab {
            id: raw.id,
            window_id: raw.window_id,
            url: raw.url,
            title: raw.title,
            group_title: raw.group_title,
            active: raw.active,
        })
    }
}

impl ExtTab {
    pub fn into_tab_info(
        self,
        browser_id: &str,
        profile_id: &str,
        profile_label: Option<String>,
    ) -> TabInfo {
        let tab_id = match self.id {
            Value::String(s) => s,
            Value::Number(number) => number.to_string(),
            // `ExtTab` 的 Deserialize 只放行「非空字符串 / 正整数」id，走到这里是 bug。
            // 不再用 `other.to_string()` 造伪 id：宁可给空串，也不让
            // `{"a":1}` / `true` 之类变成会话 key 的一部分。
            other => {
                debug_assert!(false, "invalid tab id survived deserialization: {other}");
                String::new()
            }
        };
        let session_key = make_session_key(browser_id, profile_id, &tab_id);
        TabInfo {
            id: tab_id.clone(),
            window_id: self.window_id,
            tab_id,
            browser_id: browser_id.to_string(),
            profile_id: profile_id.to_string(),
            profile_label,
            browser_label: None,
            session_key,
            url: self.url,
            title: self.title,
            group_title: self.group_title,
            active: self.active,
            tab_type: "ext_ws".to_string(),
            connected_at: None,
        }
    }
}

pub fn make_session_key(browser_id: &str, profile_id: &str, tab_id: &str) -> String {
    format!("{browser_id}:{profile_id}:{tab_id}")
}

/// `tabs` 字段的反序列化：逐个校验，`id` 类型不合法的标签**直接丢弃**。
///
/// 放在反序列化阶段而不是调用点，是因为 `tabs` 由扩展推送（不可信输入），而丢弃
// 必须在「进会话表之前」发生；这样即使调用方忘了过滤，也不会出现伪 id。
fn deserialize_ext_tabs<'de, D>(deserializer: D) -> Result<Vec<ExtTab>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        // 逐条 `from_value`：某一条 id 非法（或字段类型不对）只丢这一条，不炸整批。
        .filter_map(|value| serde_json::from_value::<ExtTab>(value).ok())
        .collect())
}

fn default_browser_id() -> String {
    "browser-default".to_string()
}

fn default_profile_id() -> String {
    "profile-default".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_tab_ids_are_dropped_at_deserialization() {
        let message: WsIncoming = serde_json::from_value(json!({
            "type": "ext_ready",
            "tabs": [
                { "id": "abc", "url": "https://a", "title": "A" },
                { "id": 42, "url": "https://b", "title": "B" },
                { "id": { "nested": 1 }, "url": "https://c" },
                { "id": null, "url": "https://d" },
                { "id": [1, 2], "url": "https://e" },
                { "id": true, "url": "https://f" }
            ]
        }))
        .expect("ext_ready");

        let WsIncoming::ExtReady { tabs, .. } = message else {
            panic!("expected ext_ready");
        };
        // 只有字符串和整数 id 的标签留下，其余直接丢弃。
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, json!("abc"));
        assert_eq!(tabs[1].id, json!(42));
    }

    #[test]
    fn numeric_tab_id_becomes_decimal_string() {
        let tab: ExtTab = serde_json::from_value(json!({
            "id": 7, "url": "u", "title": "t"
        }))
        .expect("tab");
        let info = tab.into_tab_info("b", "p", None);
        assert_eq!(info.tab_id, "7");
        assert_eq!(info.id, "7");
        assert_eq!(info.session_key, "b:p:7");
    }

    // 负 id / 0 / 空串必须在反序列化阶段就被丢掉。
    ///
    /// 旧实现只校验「是字符串或整数」，负值一路进会话表（执行时才报错，归因失真），
    /// 空串则让所有非法标签塌缩成同一个 `b:p:` 会话 key。
    #[test]
    fn non_positive_and_blank_tab_ids_are_dropped() {
        let message: WsIncoming = serde_json::from_value(json!({
            "type": "ext_ready",
            "tabs": [
                { "id": -1, "url": "https://a" },
                { "id": 0, "url": "https://b" },
                { "id": "", "url": "https://c" },
                { "id": "   ", "url": "https://d" },
                { "id": 3, "url": "https://e" },
                { "id": "42", "url": "https://f" }
            ]
        }))
        .expect("ext_ready");
        let WsIncoming::ExtReady { tabs, .. } = message else {
            panic!("expected ext_ready");
        };
        assert_eq!(tabs.len(), 2, "只保留正整数与非空字符串 id");
        assert_eq!(tabs[0].id, json!(3));
        assert_eq!(tabs[1].id, json!("42"));
    }

    #[test]
    fn tabs_update_also_filters_invalid_ids() {
        let message: WsIncoming = serde_json::from_value(json!({
            "type": "tabs_update",
            "tabs": [
                { "id": "keep" },
                { "id": { "x": 1 } }
            ]
        }))
        .expect("tabs_update");
        let WsIncoming::TabsUpdate { tabs, .. } = message else {
            panic!("expected tabs_update");
        };
        assert_eq!(tabs.len(), 1);
    }

    // key 与时间戳必须同生共死——只有 key、没有时间戳的记忆视为无效。
    #[test]
    fn preferred_default_requires_a_timestamp() {
        let mut driver = DriverState {
            preferred_default_session_key: Some("k".to_string()),
            ..DriverState::default()
        };
        assert!(
            driver.preferred_default_session().is_none(),
            "缺时间戳的 key 不能当作「永远有效」"
        );
        assert!(driver.expire_preferred_default_session());
        assert!(driver.preferred_default_session_key.is_none());
        assert!(driver.preferred_default_session_at.is_none());
        assert!(driver.preferred_default_session_at_instant.is_none());
    }

    // unix 秒回退判据要求年龄非负——时钟回拨造成的「未来时间戳」
    /// 必须按过期处理，不能变成永不过期。
    #[test]
    fn preferred_default_rejects_negative_age_after_clock_rollback() {
        let mut driver = DriverState {
            preferred_default_session_key: Some("k".to_string()),
            preferred_default_session_at: Some(now_unix_secs() + 3600.0),
            ..DriverState::default()
        };
        assert!(
            driver.preferred_default_session().is_none(),
            "登记时刻在未来（时钟回拨）时必须过期"
        );
        assert!(driver.expire_preferred_default_session());
    }

    // 正常设置走单调时钟，读取有效；清除时三个字段一起清。
    #[test]
    fn preferred_default_uses_monotonic_clock() {
        let mut driver = DriverState::default();
        driver.set_preferred_default_session("k".to_string());
        assert!(
            driver.preferred_default_session_at_instant.is_some(),
            "设置时必须写单调时刻"
        );
        assert_eq!(driver.preferred_default_session(), Some("k"));
        driver.clear_preferred_default_session();
        assert!(driver.preferred_default_session().is_none());
        assert!(driver.preferred_default_session_at_instant.is_none());
    }
}
