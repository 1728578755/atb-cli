use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

pub(crate) fn run_lookup_command(args: LookupCommand) -> Result<()> {
    ensure_server()?;
    let path = match args.target {
        LookupTarget::Tab(args) => {
            let mut path = format!("/lookup/tab/{}", query_escape(&args.tab));
            let mut query = Vec::new();
            if let Some(browser) = args.browser {
                query.push(format!("browser={}", query_escape(&browser)));
            }
            if let Some(profile) = args.profile {
                query.push(format!("profile={}", query_escape(&profile)));
            }
            if !query.is_empty() {
                path.push('?');
                path.push_str(&query.join("&"));
            }
            path
        }
        LookupTarget::Browser(args) => format!("/lookup/browser/{}", query_escape(&args.browser)),
        LookupTarget::Profile(args) => format!("/lookup/profile/{}", query_escape(&args.profile)),
    };
    print_json(request("GET", &path, None, 30.0)?);
    Ok(())
}

pub(crate) fn run_profile_label_command(args: ProfileLabelCommand) -> Result<()> {
    ensure_server()?;
    match args.action {
        ProfileLabelAction::Set(args) => {
            print_json(request(
                "POST",
                "/profile-label",
                Some(json!({
                    "label": args.label,
                    "profile": args.profile,
                    "browser": args.browser,
                    "switch_tab_id": args.tab,
                })),
                http_budget(args.timeout),
            )?);
        }
        ProfileLabelAction::Clear(args) => {
            print_json(request(
                "POST",
                "/profile-label",
                Some(json!({
                    "label": null,
                    "profile": args.profile,
                    "browser": args.browser,
                    "switch_tab_id": args.tab,
                })),
                http_budget(args.timeout),
            )?);
        }
    }
    Ok(())
}

/// `tabs --fields` 的白名单：与 daemon `/tabs` 输出字段一一对应。
/// 旧实现没有字段过滤，智能体在几十个标签的长 JSON 里翻 tab_id 很费 token；
/// 白名单校验让拼错的字段立刻报错（exit 2），而不是静默返回一列 null。
const TABS_FIELD_WHITELIST: &[&str] = &[
    "id",
    "tab_id",
    "window",
    "browser_id",
    "browser_label",
    "profile_id",
    "profile_label",
    "session_key",
    "url",
    "title",
    "type",
    "connected_at",
    "group",
    "active",
];

fn parse_tabs_fields(spec: Option<&str>) -> Result<Option<Vec<String>>> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let mut fields = Vec::new();
    for raw in spec.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if !TABS_FIELD_WHITELIST.contains(&name) {
            return Err(usage_failure(
                "unsupported_flag",
                format!("tabs --fields 不支持字段 {name:?}"),
                format!("可用字段：{}", TABS_FIELD_WHITELIST.join(",")),
            ));
        }
        fields.push(name.to_string());
    }
    if fields.is_empty() {
        return Err(usage_failure(
            "unsupported_flag",
            "tabs --fields 至少要给一个字段",
            format!(
                "例如 --fields tab_id,title,url；可用字段：{}",
                TABS_FIELD_WHITELIST.join(",")
            ),
        ));
    }
    Ok(Some(fields))
}

/// 在 daemon 返回上做字段过滤与截断（CLI 侧做，daemon 保持通用）。
///
/// 旧实现 tabs 永远回全部字段全量列表：大浏览器里一次输出几千 token，
/// 智能体其实只用 tab_id/title/url。这里统一处理 full / --ids 两条路径共用的
/// `/result/metadata/tabs`，并把 tabs_count（原始总数）与 returned_count（本次返回）
/// 分开记录。
fn apply_fields_limit(value: &mut Value, fields: Option<&[String]>, limit: Option<usize>) {
    let Some(tabs) = value
        .pointer("/result/metadata/tabs")
        .and_then(Value::as_array)
    else {
        return;
    };
    let total = tabs.len();
    let mut items: Vec<Value> = tabs
        .iter()
        .map(|tab| match fields {
            Some(fields) => {
                let mut item = serde_json::Map::new();
                for field in fields {
                    item.insert(
                        field.clone(),
                        tab.get(field).cloned().unwrap_or(Value::Null),
                    );
                }
                Value::Object(item)
            }
            None => tab.clone(),
        })
        .collect();
    // `--limit 0` 表示「不截断」：schema 里该参数没有默认值，照表显式传 0 时
    // 不能返回空列表。
    if let Some(limit) = limit.filter(|value| *value > 0) {
        items.truncate(limit);
    }
    if let Some(metadata) = value
        .pointer_mut("/result/metadata")
        .and_then(Value::as_object_mut)
    {
        // tabs_count 保持**原始总数**，本次实际返回量另用 returned_count。
        // 旧实现把 tabs_count 改写成本次截断后的数量，调用方会误以为浏览器里只有这么多标签。
        metadata.insert("tabs_count".to_string(), json!(total));
        metadata.insert("returned_count".to_string(), json!(items.len()));
        if let Some(fields) = fields {
            metadata.insert("fields".to_string(), json!(fields));
        }
        if let Some(limit) = limit {
            metadata.insert("limit".to_string(), json!(limit));
            metadata.insert("total_before_limit".to_string(), json!(total));
        }
        // 大列表提示只对「没有自行裁剪」的调用方有意义：既然已经用 --fields /
        // --limit 缩小了输出，就不该再提示同一条建议。
        if fields.is_some() || limit.is_some_and(|value| value > 0) {
            metadata.remove("suggest");
        }
    }
    if let Some(slots) = value
        .pointer_mut("/result/metadata/tabs")
        .and_then(Value::as_array_mut)
    {
        *slots = items;
    }
}

pub(crate) fn cmd_tabs(args: TabsArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("tabs", is_async)?;
    // 字段白名单在连接 daemon 之前校验：写错了直接 exit 2，不产生任何副作用。
    let fields = parse_tabs_fields(args.fields.as_deref())?;
    ensure_server()?;
    let mut path = "/tabs".to_string();
    let mut query = Vec::new();
    if let Some(browser) = args.browser.as_deref() {
        query.push(format!("browser={}", query_escape(browser)));
    }
    if let Some(profile) = args.profile.as_deref() {
        query.push(format!("profile={}", query_escape(profile)));
    }
    // 过滤在 daemon 侧执行（GET Query），CLI 只负责参数转义与输出格式。
    if let Some(url) = args.url_contains.as_deref() {
        query.push(format!("url_contains={}", query_escape(url)));
    }
    if let Some(title) = args.title_contains.as_deref() {
        query.push(format!("title_contains={}", query_escape(title)));
    }
    if let Some(filter) = args.filter.as_deref() {
        query.push(format!("filter={}", query_escape(filter)));
    }
    // tsv 的输出契约是「完整 URL」：daemon 默认把 URL 截到 80 字符，这里必须
    // 显式要 full，否则同一标签在 json / tsv 下看到的 URL 长度不同。
    if args.full || args.format == "tsv" {
        query.push("full=true".to_string());
    }
    if args.mine {
        // mine=true 由 daemon 过滤：只有 daemon 知道哪些会话是本 agent open 的
        // （agent_opened 记录），CLI 侧猜不出来。
        query.push("mine=true".to_string());
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    let value = request("GET", &path, None, 30.0)?;
    // `ok:false` 必须原样带出错误与 error_code（print_json 负责映射退出码）；
    // jsonl/tsv 也不能把错误吞成空输出，否则调用方会以为「列表就是空的」。
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        print_json(value);
        return Ok(());
    }
    let trimming = fields.is_some() || args.limit.is_some();
    match args.format.as_str() {
        "jsonl" => {
            if args.ids {
                let view = tabs_ids_view(&value, fields.as_deref(), args.limit);
                for item in view
                    .get("tabs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    write_jsonl(item);
                }
            } else {
                let mut value = value;
                if trimming {
                    apply_fields_limit(&mut value, fields.as_deref(), args.limit);
                }
                for item in tabs_metadata_array(&value) {
                    write_jsonl(item);
                }
            }
        }
        "tsv" => {
            // 列是格式的一部分：`tab_id\t标题\tURL`。`--fields` 不参与（否则列会
            // 缺项），但 `--limit` 依然生效；空列表输出空，不输出表头。
            let limit = args.limit.filter(|value| *value > 0).unwrap_or(usize::MAX);
            for item in tabs_metadata_array(&value).iter().take(limit) {
                write_tabs_tsv(item);
            }
        }
        // clap 的 value_parser 已把取值限制在 json/jsonl/tsv，这里保持防御性兜底。
        _ => {
            if args.ids {
                print_json(tabs_ids_view(&value, fields.as_deref(), args.limit));
            } else {
                let mut value = value;
                if trimming {
                    apply_fields_limit(&mut value, fields.as_deref(), args.limit);
                }
                print_json(value);
            }
        }
    }
    Ok(())
}

/// `/tabs` 回包里 `result.metadata.tabs` 的数组视图（daemon 默认结构）。
fn tabs_metadata_array(value: &Value) -> &[Value] {
    value
        .pointer("/result/metadata/tabs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// JSONL：一行一个标签页对象，不包外层响应。
fn write_jsonl(item: &Value) {
    let line = serde_json::to_string(item).unwrap_or_else(|_| "null".to_string());
    write_stdout(&line);
}

/// 单元格里的 tab / 换行会破坏 TSV 的行列结构，统一替换成空格。
fn tsv_cell(text: &str) -> String {
    text.replace(['\t', '\r', '\n'], " ")
}

/// TSV：`tab_id\t标题\tURL`（URL 由调用方确保是 full）。
fn tabs_tsv_line(item: &Value) -> String {
    let tab_id = item.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let title = item.get("title").and_then(Value::as_str).unwrap_or("");
    let url = item.get("url").and_then(Value::as_str).unwrap_or("");
    format!("{tab_id}\t{}\t{}", tsv_cell(title), tsv_cell(url))
}

fn write_tabs_tsv(item: &Value) {
    write_stdout(&tabs_tsv_line(item));
}

/// `tabs --ids` 的精简视图（与 full 视图共用 `--limit` 语义）。
///
// `--limit 0` = 不截断：full 视图早已按 / 这么定义，ids 路径以前
// 直接 `truncate(0)` 返回空列表，同一条命令只是换了输出形态就得到相反的结果。
fn tabs_ids_view(value: &Value, fields: Option<&[String]>, limit: Option<usize>) -> Value {
    let tabs = value
        .pointer("/result/metadata/tabs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 精简视图的字段集合：`--fields` 直接作用于**原始 tab 对象**，`--ids` 只是
    // 默认字段集的别名。旧实现先在精简视图里裁到 6 个字段，再应用 --fields，
    // 于是 `tabs --ids --fields session_key` 恒为 null——字段根本没被带出来。
    const IDS_DEFAULT_FIELDS: &[&str] = &["tab_id", "window", "title", "url", "active", "group"];
    let selected: Vec<String> = match fields {
        Some(fields) => fields.to_vec(),
        None => IDS_DEFAULT_FIELDS
            .iter()
            .map(|field| field.to_string())
            .collect(),
    };
    // --ids 视图也要能回答「总共多少、limit 前多少」，与 full 视图对齐。
    let total_before_limit = tabs.len();
    let mut items: Vec<Value> = tabs
        .iter()
        .map(|tab| {
            let mut item = serde_json::Map::new();
            for field in &selected {
                item.insert(
                    field.clone(),
                    tab.get(field).cloned().unwrap_or(Value::Null),
                );
            }
            Value::Object(item)
        })
        .collect();
    if let Some(limit) = limit.filter(|value| *value > 0) {
        items.truncate(limit);
    }
    json!({
        "ok": value.get("ok").cloned().unwrap_or(json!(false)),
        "count": items.len(),
        "returned_count": items.len(),
        "total_before_limit": total_before_limit,
        "total": total_before_limit,
        "tabs": items,
        "hint": "把某个 tab_id 直接传给 --tab；也可以 --tab @1 按序号、--tab window:<id> 按窗口",
    })
}

pub(crate) fn cmd_tabtree(args: TabtreeArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("tabtree", is_async)?;
    ensure_server()?;
    let mut path = "/tabtree".to_string();
    let mut query = Vec::new();
    if let Some(tab) = args.tab {
        query.push(format!("tab={}", query_escape(&tab)));
    }
    if let Some(browser) = args.browser {
        query.push(format!("browser={}", query_escape(&browser)));
    }
    if let Some(profile) = args.profile {
        query.push(format!("profile={}", query_escape(&profile)));
    }
    if args.full {
        query.push("full=true".to_string());
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    print_json(request("GET", &path, None, 30.0)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tabs() -> Value {
        json!({
            "ok": true,
            "result": { "metadata": { "tabs": [
                { "tab_id": "a", "title": "A", "url": "https://a/", "window": 1, "session_key": "s1" },
                { "tab_id": "b", "title": "B", "url": "https://b/", "window": 1, "session_key": "s2" },
                { "tab_id": "c", "title": "C", "url": "https://c/", "window": 2, "session_key": "s3" },
            ] } }
        })
    }

    /// ：`--ids --limit 0` 与 full 视图同语义（0 = 不截断）。
    #[test]
    fn ids_view_limit_zero_means_no_truncation() {
        let view = tabs_ids_view(&sample_tabs(), None, Some(0));
        assert_eq!(view["count"], json!(3));
        assert_eq!(view["total"], json!(3));
        assert_eq!(view["tabs"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn ids_view_limit_truncates_and_reports_total() {
        let view = tabs_ids_view(&sample_tabs(), None, Some(2));
        assert_eq!(view["count"], json!(2));
        assert_eq!(view["returned_count"], json!(2));
        assert_eq!(view["total_before_limit"], json!(3));
        assert_eq!(view["tabs"].as_array().unwrap().len(), 2);
    }

    /// `--fields` 作用于原始 tab 对象（不能先裁到默认字段集）。
    #[test]
    fn ids_view_applies_custom_fields() {
        let fields = vec!["tab_id".to_string(), "session_key".to_string()];
        let view = tabs_ids_view(&sample_tabs(), Some(&fields), None);
        assert_eq!(view["tabs"][0]["session_key"], json!("s1"));
        assert!(view["tabs"][0].get("title").is_none());
    }

    // TSV 固定三列；标题里的 tab / 换行替换成空格，URL 保持完整。
    #[test]
    fn tsv_line_sanitizes_title_and_keeps_full_url() {
        let tab = json!({
            "tab_id": "123",
            "title": "A\tB\nC",
            "url": "https://example.com/a/very/long/path?query=1",
        });
        assert_eq!(
            tabs_tsv_line(&tab),
            "123\tA B C\thttps://example.com/a/very/long/path?query=1"
        );
        // 缺字段时给空单元格，不 panic。
        assert_eq!(tabs_tsv_line(&json!({})), "\t\t");
    }
}
