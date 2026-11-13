//! `atb measure` / `atb userscript`（2026-11-13）。
//!
//! 两条命令都复用既有 daemon 能力，不新增路由：
//! - `measure` 构造一段页面 JS，通过 `POST /exec` 量测元素的 rect / computed style；
//! - `userscript` 解析本地 `.user.js` 元数据，再通过 `POST /open` 打开安装页。

use super::*;

/// `measure` 默认返回的 computed style 名单。
const DEFAULT_STYLES: &[&str] = &["display", "position", "width", "height", "zIndex"];
const MAX_STYLES: usize = 64;

pub(crate) fn cmd_measure(args: MeasureArgs, _is_async: bool) -> Result<()> {
    let selector = args.selector.trim();
    if selector.is_empty() {
        return Err(usage_failure(
            "invalid_selector",
            "measure: --selector 不能为空",
            "传 CSS 选择器或 `host >>> inner`；先用 find / snapshot 定位元素",
        ));
    }
    if selector.starts_with('@') {
        return Err(usage_failure(
            "unsupported_selector",
            format!("measure: 不支持 @e 引用 {selector:?}"),
            "先用 find --text/--selector 或 snapshot 拿到真实 CSS 选择器，再传给 measure",
        ));
    }
    let parts = split_shadow_selector(selector)?;
    for part in &parts {
        if part.starts_with('@') {
            return Err(usage_failure(
                "unsupported_selector",
                format!("measure: 不支持 @e 引用 {part:?}"),
                "先用 find --text/--selector 或 snapshot 拿到真实 CSS 选择器，再传给 measure",
            ));
        }
        if preflight::selector_kind(part) != preflight::SelectorKind::Css {
            return Err(usage_failure(
                "unsupported_selector",
                format!("measure: 只支持 CSS 选择器，不支持 XPath 段 {part:?}"),
                "改成 CSS 选择器；shadow DOM 用 `host >>> inner` 逐层进入",
            ));
        }
        if let Err(mut problem) = preflight::validate_selector(part) {
            problem.message = format!("measure: {}", problem.message);
            problem.field = Some("selector".to_string());
            problem.example = "atb measure --selector '#id'";
            return usage_error(&problem);
        }
    }
    let styles = parse_styles(args.styles.as_deref())?;
    let script = build_measure_script(&parts, &styles, args.with_text);
    let mut payload = json!({
        "script": script,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "frame_id": args.frame,
        "timeout": args.timeout,
        "no_monitor": true,
    });
    let spec = match TargetSpec::parse(args.tab.as_deref().unwrap_or("")) {
        Ok(spec) => spec,
        Err(err) => return usage_error(&spec_problem(err)),
    };
    if let Err(problem) = finalize_page_payload("exec", &mut payload) {
        return usage_error(&problem);
    }
    // dry-run 只在本地输出计划，不需要 daemon 解析目标；这样 `measure --dry-run`
    // 在没启动 daemon 时也能用。
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "measure",
                "selector": selector,
                "parts": parts,
                "styles": styles,
                "args": payload,
                "note": "去掉 --dry-run 才会真正执行 POST /exec",
            }
        }));
        return Ok(());
    }
    // `--tab current` / `@1` / `url:` 这类目标需要 daemon 解析；复用 `/multi`
    // （op=exec）而不是在 CLI 侧重复实现选标签逻辑。
    if spec.is_multi() || spec.needs_daemon_resolution() {
        return measure_via_multi(args, payload);
    }
    ensure_server()?;
    let value = request("POST", "/exec", Some(payload), http_budget(args.timeout))?;
    print_measure_result(super::cmd_page::post_process_exec_error(value));
    Ok(())
}

/// 目标需要 daemon 解析（`current` / `@N` / `url:` / `all`）时走 `/multi`。
///
/// `op` 仍是 `exec`（不新增 daemon op）；单目标结果会被解包成与 `/exec` 相同的
/// `{ok:true,result:{...}}`，多目标则返回逐标签的 `results[]`。
fn measure_via_multi(args: MeasureArgs, payload: Value) -> Result<()> {
    let mut args_payload = payload;
    if let Some(object) = args_payload.as_object_mut() {
        // 目标由 /multi 按解析结果注入，避免两边各算一遍。
        object.remove("switch_tab_id");
    }
    let body = json!({
        "op": "exec",
        "args": args_payload,
        "target": args.tab.unwrap_or_default(),
        "browser": args.browser,
        "profile": args.profile,
        "dry_run": false,
    });
    ensure_server()?;
    let value = request("POST", "/multi", Some(body), http_budget(args.timeout))?;
    let value = super::cmd_page::post_process_exec_error(value);
    print_measure_multi_result(value);
    Ok(())
}

/// `/multi` 回包 → measure 的对外形状：
/// - 单目标：与 `/exec` 完全同形；
/// - 多目标：`{ok,count,failed,results:[{tab_id,ok,result|error...}]}`。
fn print_measure_multi_result(value: Value) {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        print_json(value);
        return;
    };
    if results.len() == 1 {
        let entry = &results[0];
        if entry.get("ok").and_then(Value::as_bool) == Some(true) && entry.get("result").is_some() {
            // 传完整 entry：`print_measure_result` 读的是 `/result/js_return` /
            // `/result/tab_id`，只传内层会让它找不到路径、把原始 exec 结果打出来。
            print_measure_result(entry.clone());
            return;
        }
        print_json(entry.clone());
        return;
    }
    let mut items: Vec<Value> = Vec::with_capacity(results.len());
    let mut failed = 0usize;
    for entry in results {
        let tab_id = entry.get("tab_id").cloned().unwrap_or(Value::Null);
        let inner = entry.get("result");
        let js_return = inner.and_then(|inner| inner.pointer("/result/js_return"));
        if entry.get("ok").and_then(Value::as_bool) == Some(true)
            && js_return
                .and_then(|js| js.get("ok"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            let mut measurement = js_return.cloned().unwrap_or(Value::Null);
            if let Some(object) = measurement.as_object_mut() {
                object.remove("ok");
            }
            items.push(json!({
                "tab_id": tab_id,
                "ok": true,
                "result": measurement,
            }));
        } else {
            failed += 1;
            let mut item = json!({
                "tab_id": tab_id,
                "ok": false,
                "error": entry
                    .get("error")
                    .or_else(|| inner.and_then(|inner| inner.get("error")))
                    .cloned()
                    .unwrap_or_else(|| json!("measure: 目标未返回量测结果")),
                "error_code": entry
                    .get("error_code")
                    .or_else(|| inner.and_then(|inner| inner.get("error_code")))
                    .cloned()
                    .unwrap_or_else(|| json!("selector_not_found")),
            });
            if let Some(hint) = entry
                .get("hint")
                .or_else(|| inner.and_then(|inner| inner.get("hint")))
            {
                item["hint"] = hint.clone();
            }
            if let Some(exec_id) = entry.get("exec_id") {
                item["exec_id"] = exec_id.clone();
            }
            items.push(item);
        }
    }
    let count = items.len();
    print_json(json!({
        "ok": failed == 0,
        "count": count,
        "failed": failed,
        "results": items,
    }));
}

/// 把 `/exec` 的 `{ok, result:{js_return}}` 解包成 measure 的对外形状。
///
/// - 命中：`{ok:true, result:{...量测字段}, tab_id}`；
/// - 选择器未命中：`{ok:false, error_code:"selector_not_found"}` → 退出码 1
///   （与 find 的「语法错 2 / 未找到 1」口径一致）；
/// - 其它错误：原样透传 daemon 的结构化错误。
fn print_measure_result(value: Value) {
    let daemon_ok = value.get("ok").and_then(Value::as_bool) == Some(true);
    let Some(js_return) = value.pointer("/result/js_return") else {
        print_json(value);
        return;
    };
    let tab_id = value
        .pointer("/result/tab_id")
        .cloned()
        .unwrap_or(Value::Null);
    // 成功回包用 `matched:true` 表达（历史脚本形状），不一定带 `ok:true`；
    // 两者任一为 true 都按命中处理，否则量测结果会被原样 `js_return` 包着返回。
    let matched_ok = js_return.get("ok").and_then(Value::as_bool) == Some(true)
        || js_return.get("matched").and_then(Value::as_bool) == Some(true);
    let matched_failed = js_return.get("ok").and_then(Value::as_bool) == Some(false)
        || js_return.get("matched").and_then(Value::as_bool) == Some(false);
    if daemon_ok && matched_ok {
        let mut result = js_return.clone();
        if let Some(object) = result.as_object_mut() {
            // 内部 ok 只是脚本回包标记，对外 `{ok:true,result}` 已经表达成功。
            object.remove("ok");
        }
        let mut output = json!({ "ok": true, "result": result });
        if !tab_id.is_null() {
            output["tab_id"] = tab_id;
        }
        print_json(output);
        return;
    }
    if daemon_ok && matched_failed {
        let code = js_return
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("selector_not_found");
        let message = js_return
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("measure: 选择器没有命中任何元素");
        let hint = js_return.get("hint").and_then(Value::as_str).unwrap_or(
            "确认元素已渲染；shadow DOM 用 `host >>> inner`；closed shadow root 无法访问，可改用坐标或 CDP 方案",
        );
        let mut output = json!({
            "ok": false,
            "error": message,
            "error_code": code,
            "hint": hint,
        });
        if !tab_id.is_null() {
            output["tab_id"] = tab_id;
        }
        print_json(output);
        return;
    }
    print_json(value);
}

/// 解析 `--styles`；默认 `display,position,width,height,zIndex`。
fn parse_styles(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_STYLES.iter().map(|item| item.to_string()).collect());
    };
    let mut styles: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    if styles.is_empty() {
        return Err(usage_failure(
            "invalid_argument",
            "measure: --styles 不能为空",
            "省略 --styles 使用默认 display,position,width,height,zIndex；或写 --styles display,color",
        ));
    }
    if styles.len() > MAX_STYLES {
        return Err(usage_failure(
            "invalid_argument",
            format!(
                "measure: --styles 最多 {MAX_STYLES} 项（收到 {}）",
                styles.len()
            ),
            "只取真正需要的 computed style，避免一条命令返回过多字段",
        ));
    }
    // 保序去重：`display,display,color` 只保留第一次出现。
    let mut seen = std::collections::HashSet::new();
    styles.retain(|style| seen.insert(style.clone()));
    Ok(styles)
}

/// 拆分 `host >>> inner >>> leaf`：只在引号/括号之外、深度为 0 的 `>>>` 处分段。
///
/// 简单 `split(">>>")` 会把 `[data-x="a>>>b"]` 这类合法 CSS 属性值切断。
fn split_shadow_selector(selector: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = selector.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut bracket_depth = 0i32;
    let mut paren_depth = 0i32;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active) = quote {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            current.push(ch);
            index += 1;
            continue;
        }
        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            _ => {}
        }
        if ch == '>'
            && bracket_depth == 0
            && paren_depth == 0
            && chars.get(index + 1) == Some(&'>')
            && chars.get(index + 2) == Some(&'>')
        {
            let part = current.trim().to_string();
            if part.is_empty() {
                return Err(usage_failure(
                    "invalid_selector",
                    "measure: shadow 选择器里有空段（>>> 前后不能为空）",
                    "写成 `host >>> inner`，每段都是完整 CSS 选择器",
                ));
            }
            parts.push(part);
            current.clear();
            index += 3;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    let last = current.trim().to_string();
    if last.is_empty() {
        return Err(usage_failure(
            "invalid_selector",
            "measure: shadow 选择器以 >>> 结尾，缺少最后一段",
            "写成 `host >>> inner`；最后一段必须是非空 CSS 选择器",
        ));
    }
    parts.push(last);
    Ok(parts)
}

/// 生成 measure 的页面脚本。
///
/// 顶层只有 `return (() => { ... })()`，不引入 `const/let` 全局声明；
/// 与 `exec` 的 `wrap_user_script` 兼容（脚本自己已经 return）。
fn build_measure_script(parts: &[String], styles: &[String], with_text: bool) -> String {
    let parts_json = serde_json::to_string(parts).unwrap_or_else(|_| "[]".to_string());
    let styles_json = serde_json::to_string(styles).unwrap_or_else(|_| "[]".to_string());
    let template = r#"return (() => {
  const parts = __PARTS__;
  const styles = __STYLES__;
  let root = document;
  let el = null;
  const shadowPath = [];
  for (let i = 0; i < parts.length; i++) {
    let node;
    try {
      node = root.querySelector(parts[i]);
    } catch (e) {
      return {matched:false, ok:false, error_code:"invalid_selector", error:String(e), segment:i+1, selector:parts[i]};
    }
    if (!node) {
      return {matched:false, ok:false, error_code:"selector_not_found", error:"measure: 选择器未命中: " + parts[i], segment:i+1, selector:parts[i]};
    }
    el = node;
    if (i < parts.length - 1) {
      const shadow = node.shadowRoot;
      if (!shadow) {
        return {matched:false, ok:false, error_code:"shadow_closed_unreachable", error:"measure: 第 " + (i+1) + " 段命中的元素没有 open shadow root（closed shadow root 无法访问）: " + parts[i], segment:i+1, selector:parts[i]};
      }
      shadowPath.push({tag:node.tagName.toLowerCase(), id:node.id || "", class:(typeof node.className === "string" ? node.className : (node.getAttribute("class") || ""))});
      root = shadow;
    }
  }
  const cs = window.getComputedStyle(el);
  const rect = el.getBoundingClientRect();
  const stylesOut = {};
  for (let i = 0; i < styles.length; i++) {
    const name = String(styles[i]);
    const prop = name.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase());
    stylesOut[name] = cs.getPropertyValue(prop);
  }
  const out = {
    matched: true,
    tag: el.tagName.toLowerCase(),
    id: el.id || "",
    class: (typeof el.className === "string" ? el.className : (el.getAttribute("class") || "")),
    shadow_path: shadowPath,
    rect: {x:rect.x, y:rect.y, width:rect.width, height:rect.height, top:rect.top, left:rect.left},
    viewport: {w:window.innerWidth, h:window.innerHeight, scrollX:window.scrollX, scrollY:window.scrollY},
    visible: rect.width > 0 && rect.height > 0 && cs.display !== "none" && cs.visibility !== "hidden",
    display: cs.display,
    visibility: cs.visibility,
    styles: stylesOut
  };
  __TEXT__
  return out;
})();"#;
    let text_line = if with_text {
        "out.text = String(el.innerText || el.textContent || \"\").slice(0, 200);"
    } else {
        ""
    };
    template
        .replace("__PARTS__", &parts_json)
        .replace("__STYLES__", &styles_json)
        .replace("__TEXT__", text_line)
}

/// `.user.js` 的 `==UserScript==` 元数据（只取本轮用得到的字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UserscriptMetadata {
    pub(crate) valid: bool,
    pub(crate) name: Option<String>,
    pub(crate) namespace: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) matches: Vec<String>,
    pub(crate) grants: Vec<String>,
    pub(crate) run_at: Option<String>,
}

impl UserscriptMetadata {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "namespace": self.namespace,
            "version": self.version,
            "match": self.matches,
            "grant": self.grants,
            "run_at": self.run_at,
        })
    }
}

/// 解析 `==UserScript==` 块；缺失时 `valid:false`，字段全空。
///
/// 支持行注释与块注释里的 `@key value` 写法；同一 key 出现多次时（`@match` /
/// `@grant`）按出现顺序收集。
pub(crate) fn parse_userscript_metadata(code: &str) -> UserscriptMetadata {
    let mut metadata = UserscriptMetadata::default();
    let mut inside = false;
    for raw_line in code.lines() {
        let mut line = raw_line.trim();
        if line.starts_with("//") {
            line = line.trim_start_matches("//").trim();
        } else if line.starts_with("/*") {
            line = line.trim_start_matches("/*").trim();
        }
        if line.ends_with("*/") {
            line = line.trim_end_matches("*/").trim();
        }
        if line == "==/UserScript==" {
            inside = false;
            continue;
        }
        if line == "==UserScript==" {
            inside = true;
            metadata.valid = true;
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = line.strip_prefix('@') else {
            continue;
        };
        let mut pieces = rest.splitn(2, char::is_whitespace);
        let key = pieces
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let value = pieces.next().unwrap_or_default().trim();
        if value.is_empty() {
            continue;
        }
        let value = value.to_string();
        match key.as_str() {
            "name" => metadata.name = Some(value),
            "namespace" => metadata.namespace = Some(value),
            "version" => metadata.version = Some(value),
            "match" => metadata.matches.push(value),
            "grant" => metadata.grants.push(value),
            "run-at" => metadata.run_at = Some(value),
            _ => {}
        }
    }
    metadata
}

pub(crate) fn cmd_userscript(args: UserscriptArgs, _is_async: bool) -> Result<()> {
    let source = args.source.trim();
    if source.is_empty() {
        return Err(usage_failure(
            "invalid_argument",
            "userscript: 需要本地 .user.js 文件或 http(s) URL",
            "例如 atb userscript ./script.user.js",
        ));
    }
    let lower = source.to_ascii_lowercase();
    let remote = lower.starts_with("http://") || lower.starts_with("https://");
    let file_url = lower.starts_with("file://");
    let mut metadata: Option<UserscriptMetadata> = None;
    let url = if remote || file_url {
        if args.parse_only {
            return Err(usage_failure(
                "unsupported_combination",
                "userscript --parse-only 只支持本地文件",
                "先把 .user.js 下载到本地，再 atb userscript ./script.user.js --parse-only",
            ));
        }
        source.to_string()
    } else {
        let absolute = absolutize_path(source);
        let path = PathBuf::from(&absolute);
        let code = super::cmd_page::read_script_file("userscript", &path)?;
        let parsed = parse_userscript_metadata(&code);
        if !parsed.valid {
            eprintln!("warning: 脚本没有 ==UserScript== 元数据块；安装页可能不会识别为油猴脚本");
        }
        metadata = Some(parsed);
        path_to_file_url(Path::new(&absolute))
    };
    if args.parse_only {
        let metadata = metadata.unwrap_or_default();
        print_json(json!({
            "ok": true,
            "result": {
                "metadata": metadata.to_json(),
                "valid": metadata.valid,
                "note": if metadata.valid {
                    "已解析 ==UserScript== 元数据"
                } else {
                    "没有找到 ==UserScript== 元数据块；脚本仍可安装，但油猴管理器可能不识别"
                },
            }
        }));
        return Ok(());
    }
    let metadata_json = metadata
        .as_ref()
        .map(UserscriptMetadata::to_json)
        .unwrap_or(Value::Null);
    let mut payload = json!({
        "target": url,
        "active": true,
    });
    if let Some(tab) = args.tab {
        payload["switch_tab_id"] = json!(tab);
    }
    if let Some(browser) = args.browser {
        payload["browser"] = json!(browser);
    }
    if let Some(profile) = args.profile {
        payload["profile"] = json!(profile);
    }
    if DRY_RUN.load(Ordering::Relaxed) {
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "userscript",
                "url": url,
                "metadata": metadata_json,
                "payload": payload,
                "note": "去掉 --dry-run 才会打开安装页",
            }
        }));
        return Ok(());
    }
    ensure_server()?;
    let mut value = request("POST", "/open", Some(payload), 30.0)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("metadata".to_string(), metadata_json);
        object.insert(
            "note".to_string(),
            json!("在 Tampermonkey/Violentmonkey 弹出的安装页确认安装"),
        );
    }
    print_json(value);
    Ok(())
}

/// 本地绝对路径 → `file:///...` URL。
///
/// - Windows：`D:\a b\c.user.js` → `file:///D:/a%20b/c.user.js`（盘符前三个斜杠）；
/// - Unix：`/home/me/c.user.js` → `file:///home/me/c.user.js`；
/// - UNC：`\\server\share\c.user.js` → `file://server/share/c.user.js`。
pub(crate) fn path_to_file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let raw = raw.strip_prefix("//?/").unwrap_or(&raw);
    let encoded = percent_encode_path(raw);
    if let Some(rest) = encoded.strip_prefix("//") {
        format!("file://{rest}")
    } else if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// RFC 3986 path 编码：保留 `/`、`:` 与 unreserved 字符，其余按 UTF-8 字节百分号编码。
fn percent_encode_path(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        let ch = byte as char;
        if byte.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~' | '/' | ':') {
            encoded.push(ch);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_shadow_selector_ignores_quoted_arrows() {
        assert_eq!(
            split_shadow_selector("my-app >>> #inner").unwrap(),
            vec!["my-app".to_string(), "#inner".to_string()]
        );
        assert_eq!(
            split_shadow_selector("div[data-x=\"a>>>b\"] >>> span").unwrap(),
            vec!["div[data-x=\"a>>>b\"]".to_string(), "span".to_string()]
        );
        assert_eq!(
            split_shadow_selector(":is(a, b) >>> .leaf").unwrap(),
            vec![":is(a, b)".to_string(), ".leaf".to_string()]
        );
        assert!(split_shadow_selector("host >>>").is_err());
        assert!(split_shadow_selector(">>> inner").is_err());
    }

    #[test]
    fn measure_script_embeds_parts_and_visibility_rule() {
        let parts = vec!["my-app".to_string(), "#inner".to_string()];
        let styles = DEFAULT_STYLES
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>();
        let script = build_measure_script(&parts, &styles, true);
        assert!(script.contains("\"my-app\""), "{script}");
        assert!(script.contains("\"#inner\""), "{script}");
        assert!(script.contains("shadowRoot"), "{script}");
        assert!(script.contains("getBoundingClientRect"), "{script}");
        assert!(
            script.contains("rect.width > 0 && rect.height > 0"),
            "{script}"
        );
        assert!(script.contains("out.text = String("), "{script}");
        assert!(!script.contains("__PARTS__"), "{script}");
        assert!(!script.contains("__TEXT__"), "{script}");
        // 生成的脚本必须能通过 CLI 自己的 JS 配平检查。
        assert!(preflight::validate_js(&script).is_ok());
    }

    #[test]
    fn styles_default_and_override() {
        assert_eq!(
            parse_styles(None).unwrap(),
            DEFAULT_STYLES
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_styles(Some("display, z-index ,color")).unwrap(),
            vec![
                "display".to_string(),
                "z-index".to_string(),
                "color".to_string()
            ]
        );
        assert_eq!(
            parse_styles(Some("display,display, color")).unwrap(),
            vec!["display".to_string(), "color".to_string()]
        );
        assert!(parse_styles(Some(" , ,")).is_err());
    }

    #[test]
    fn file_url_uses_three_slashes_for_drive_paths() {
        assert_eq!(
            path_to_file_url(Path::new("D:\\Download\\a b#1.user.js")),
            "file:///D:/Download/a%20b%231.user.js"
        );
        assert_eq!(
            path_to_file_url(Path::new("/home/me/a.user.js")),
            "file:///home/me/a.user.js"
        );
        assert_eq!(
            path_to_file_url(Path::new("\\\\server\\share\\a b.js")),
            "file://server/share/a%20b.js"
        );
        assert_eq!(
            path_to_file_url(Path::new("C:\\work\\100%.user.js")),
            "file:///C:/work/100%25.user.js"
        );
    }

    #[test]
    fn parses_userscript_metadata() {
        let code = r#"// ==UserScript==
// @name         Demo Script
// @namespace    demo
// @version      1.2.3
// @match        https://a.example/*
// @match        https://b.example/*
// @grant        GM_getValue
// @run-at       document-start
// ==/UserScript==
console.log("hi");
"#;
        let metadata = parse_userscript_metadata(code);
        assert!(metadata.valid);
        assert_eq!(metadata.name.as_deref(), Some("Demo Script"));
        assert_eq!(metadata.namespace.as_deref(), Some("demo"));
        assert_eq!(metadata.version.as_deref(), Some("1.2.3"));
        assert_eq!(
            metadata.matches,
            vec!["https://a.example/*", "https://b.example/*"]
        );
        assert_eq!(metadata.grants, vec!["GM_getValue"]);
        assert_eq!(metadata.run_at.as_deref(), Some("document-start"));
    }

    #[test]
    fn missing_metadata_block_is_invalid_but_parses() {
        let metadata = parse_userscript_metadata("console.log('plain js');");
        assert!(!metadata.valid);
        assert!(metadata.name.is_none());
        assert!(metadata.matches.is_empty());
    }
}
