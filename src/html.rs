use html5ever::tendril::TendrilSink;
use html5ever::{parse_document, serialize, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};
use std::io::Cursor;

pub fn js_opt_html() -> &'static str {
    include_str!("../assets/simphtml_opt.js")
}

/// 页面结构化读取（标题/正文/元信息/表单/链接）。
pub fn js_page_read() -> &'static str {
    include_str!("../assets/page_read.js")
}

/// 元素查找（CSS / 文本 / XPath → 可操作信息）。
pub fn js_page_find() -> &'static str {
    include_str!("../assets/page_find.js")
}

/// 单往返动作（hover / click / select / check / scroll / setValue / focus）。
pub fn js_page_action() -> &'static str {
    include_str!("../assets/page_action.js")
}

/// DOM 版快照（CDP 不可用时的回退），产出带 CSS 选择器的 @e 列表。
pub fn js_page_snapshot() -> &'static str {
    include_str!("../assets/page_snapshot.js")
}

/// 页面网络：fetch / hook / log / route / reset。
pub fn js_page_net() -> &'static str {
    include_str!("../assets/page_net.js")
}

/// 页面内等待（MutationObserver，一次注入完成）。
pub fn js_page_wait() -> &'static str {
    include_str!("../assets/page_wait.js")
}

/// 挑战页 / 验证码识别（识别与等待）。
pub fn js_page_challenge() -> &'static str {
    include_str!("../assets/page_challenge.js")
}

/// 自动化痕迹审计与自查清理（stealth audit / scrub）。
pub fn js_page_stealth() -> &'static str {
    include_str!("../assets/page_stealth.js")
}

/// 列表识别脚本（当前未启用）。
/// 它会做全量 DOM 遍历并强制触发布局，不能像以前那样"跑一次然后丢掉结果"；
/// 重新启用时应作为 `scan --list` 的独立能力，把结果返回给调用方。
#[allow(dead_code)]
pub fn js_find_main_list() -> &'static str {
    include_str!("../assets/simphtml_find_list.js")
}

/// 输入 HTML 的字节上限。
///
/// 超过这个量级就不再交给 html5ever：8MB+ 的文档既可能是几百万层嵌套，也可能是
/// 生成式页面吐出来的巨型内联脚本，解析出来的 DOM 本身就能把内存和栈吃满。
/// 降级成纯文本仍然可用，且成本只是 O(n) 的字节扫描，不会有栈深度问题。
const MAX_HTML_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// 解析后允许的 DOM 节点数上限。
///
/// 输入字节上限挡不住「节点爆炸」：2 MB 的 `<div></div>` 重复串就能生成 30 万个
/// 元素节点，而每个节点都要分配 `Node` + `Vec<Handle>` + 属性表，实际内存是输入的
/// 十几倍；再叠加后续 `clean_node` 和序列化，很容易把 daemon 的常驻内存顶穿。
/// 20 万节点已经远超正常页面正文的量级（真实页面通常在几千到几万），
/// 超限时降级成纯文本比「优化到一半 OOM」更可用。
const MAX_HTML_NODES: usize = 200_000;

pub fn optimize_html_for_tokens(html: &str) -> String {
    if html.len() > MAX_HTML_INPUT_BYTES {
        // 超限**不解析**：直接给纯文本降级结果，并显式标注原因，避免调用方以为
        // 拿到的是「优化过但内容完整」的 HTML。
        return format!("[html truncated: input > 8MB] {}", strip_tags(html));
    }
    let dom = parse_document(RcDom::default(), ParseOpts::default()).one(html.to_string());
    if count_nodes(&dom.document) > MAX_HTML_NODES {
        // 超限**不继续优化**：直接给纯文本降级结果，并显式标注原因。
        //
        // 丢弃前必须先迭代拆链：`RcDom` 的节点是 `Rc<Node>`，直接让它离开作用域会沿
        // children 递归析构，深链/宽树都可能把栈打爆。
        discard_subtree(&dom.document);
        return format!("[html nodes > 200k] {}", strip_tags(html));
    }
    clean_node(&dom.document);
    let mut bytes = Vec::new();
    let serializable = SerializableHandle::from(dom.document.clone());
    if serialize(&mut bytes, &serializable, Default::default()).is_ok() {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        html.to_string()
    }
}

/// 迭代遍历统计节点数（深度优先 + 显式栈），超过上限立刻返回。
///
/// 刻意不递归：这个函数的调用点正是「树可能极深」的场景，递归在数完之前就会先栈溢出。
/// 提前返回也顺手把宽树的遍历成本砍到上限附近。
fn count_nodes(root: &Handle) -> usize {
    let mut count = 0usize;
    let mut pending: Vec<Handle> = vec![root.clone()];
    while let Some(node) = pending.pop() {
        count += 1;
        if count > MAX_HTML_NODES {
            return count;
        }
        pending.extend(node.children.borrow().iter().cloned());
    }
    count
}

/// 超限输入的降级：只保留标签外的文本。
///
/// 不解析 DOM、不递归，栈深度恒定为 1。
///
// 旧实现只做「< 到 > 之间一律丢弃」，于是 `<script>` / `<style>` /
/// `<textarea>` 的**内容**被原样当成正文，注释内容也会混进来；同时不解码实体。
/// 现在用同一条 O(n) 扫描跳过这些元素的整段内容与注释，并解码常见实体。
/// 仍然不建 DOM：降级路径的定位是「超限时还能读」，不是「精确还原」。
fn strip_tags(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(html.len() / 8);
    let mut i = 0usize;
    while i < len {
        // 注释整段丢弃（包括 `<!-- -->` 里的文字）。
        if starts_with_ignore_case(&chars, i, "<!--") {
            i += 4;
            while i < len && !starts_with_ignore_case(&chars, i, "-->") {
                i += 1;
            }
            i = (i + 3).min(len);
            continue;
        }
        if chars[i] != '<' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // 读标签名（跳过 `<`、空白与可选的 `/`）。
        let mut j = i + 1;
        while j < len && chars[j].is_whitespace() {
            j += 1;
        }
        let closing = chars.get(j) == Some(&'/');
        if closing {
            j += 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
        }
        let name_start = j;
        while j < len && (chars[j].is_ascii_alphanumeric() || matches!(chars[j], '-' | ':')) {
            j += 1;
        }
        let name: String = chars[name_start..j]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        // 找到本标签的 `>`（属性值里的 `>` 不算）。
        let mut k = j;
        let mut quote: Option<char> = None;
        while k < len {
            let ch = chars[k];
            match quote {
                Some(q) => {
                    if ch == q {
                        quote = None;
                    }
                }
                None => {
                    if ch == '"' || ch == '\'' {
                        quote = Some(ch);
                    } else if ch == '>' {
                        break;
                    }
                }
            }
            k += 1;
        }
        let after_tag = (k + 1).min(len);
        // script / style / textarea：连内容一起跳过，直到对应的闭合标签。
        if !closing && matches!(name.as_str(), "script" | "style" | "textarea") {
            let close = format!("</{name}");
            i = after_tag;
            while i < len && !starts_with_ignore_case(&chars, i, &close) {
                i += 1;
            }
            if i < len {
                while i < len && chars[i] != '>' {
                    i += 1;
                }
                i = (i + 1).min(len);
            }
            continue;
        }
        // 普通标签：丢弃标签本身，补一个空格避免相邻词粘连。
        if !out.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        i = after_tag;
    }
    decode_entities(out.trim())
}

/// 大小写不敏感的前缀匹配（输入已转成 `Vec<char>`，按字符比较避免字节边界问题）。
fn starts_with_ignore_case(chars: &[char], at: usize, needle: &str) -> bool {
    needle.chars().enumerate().all(|(offset, expected)| {
        chars
            .get(at + offset)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(&expected))
    })
}

/// 解码降级路径里最常见的 HTML 实体。
///
/// `&amp;` 必须最后处理，否则 `&amp;lt;` 会被解成 `<`（二次解码）。
fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// DOM 递归下钻的深度上限。
///
/// 取 512 的理由：
/// - 与浏览器 HTML 解析器的实际嵌套上限同量级，正常页面（含深层组件库）远到不了这个深度；
/// - 超过之后 `clean_node` 和后面的 html5ever 序列化都会递归，留着才是栈溢出风险；
/// - 命中阈值时只丢弃该节点的子树，不改变其余输出，正常页面的产物与之前逐字节一致。
const MAX_DOM_DEPTH: usize = 512;

// 是否保留 `<script>` / `<style>` 的内容。
///
/// 默认 `false`：这两个元素的子树对「页面内容理解」没有价值，却经常占掉
/// 优化后 HTML 的大半 token。需要调试页面脚本时用 `ATB_HTML_KEEP_SCRIPTS=on` 打开。
fn keep_scripts() -> bool {
    crate::settings::html_keep_scripts()
}

/// 这个节点是否应该整棵丢弃。
///
/// - 注释：纯噪音；
/// - `noscript` / `template`：要么是给无 JS 环境的重复文案，要么是未实例化的模板；
/// - `script` / `style`：默认丢，`ATB_HTML_KEEP_SCRIPTS=on` 时保留。
fn should_drop_subtree(handle: &Handle, keep_scripts: bool) -> bool {
    match &handle.data {
        NodeData::Comment { .. } => true,
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            tag.eq_ignore_ascii_case("noscript")
                || tag.eq_ignore_ascii_case("template")
                || (!keep_scripts
                    && (tag.eq_ignore_ascii_case("script") || tag.eq_ignore_ascii_case("style")))
        }
        _ => false,
    }
}

fn clean_node(handle: &Handle) {
    // 环境变量每次清理只读一次：按节点读会在几十万节点的页面上变成 O(n) 次 env 查找。
    let keep = keep_scripts();
    clean_node_at(handle, 0, keep);
}

fn clean_node_at(handle: &Handle, depth: usize, keep: bool) {
    if depth >= MAX_DOM_DEPTH {
        // 极端嵌套（恶意/生成式 DOM）：停止下钻，保留该层节点本身但丢弃子树，避免进程 panic。
        //
        // 不能直接 `children.borrow_mut().clear()`：清空会把整棵子树交给 `Rc` 递归析构，
        // 10k+ 层的深链照样把栈打爆。改成分批摘链 + 迭代 drop，每层 Drop 时
        // 它的 children 已经是空的，析构深度恒为 O(1)。
        discard_subtree(handle);
        return;
    }
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        let tag = name.local.as_ref();
        let mut attrs = attrs.borrow_mut();
        if tag.eq_ignore_ascii_case("svg") {
            // 之前整棵 svg 的属性和子树都被清空，图标按钮的可读语义（aria-label / role /
            // 内联 <title> / data-testid）全丢。现在只保留可读属性 + 可能带文案的
            // <title> 子节点，绘制路径（path/circle/g/…）照旧丢弃，token 预算不受影响。
            attrs.retain(|attr| {
                let key = attr.name.local.as_ref();
                matches!(key, "aria-label" | "aria-hidden" | "role" | "title")
                    || key.starts_with("data-test")
            });
            for attr in attrs.iter_mut() {
                // 阈值与截断必须同一口径（都按字符）：`len()` 是字节数，中文属性值
                // 会被提前截断成半截观感。
                if attr.value.chars().count() > 100 {
                    let short: String = attr.value.chars().take(50).collect();
                    attr.value.clear();
                    attr.value.push_slice(&format!("{} ...", short));
                }
            }
            drop(attrs);
            let mut children = handle.children.borrow_mut();
            children.retain(|child| {
                matches!(&child.data, NodeData::Element { name, .. }
                    if name.local.as_ref().eq_ignore_ascii_case("title"))
            });
            for child in children.iter() {
                if let NodeData::Text { contents } = &child.data {
                    let mut text = contents.borrow_mut();
                    if text.chars().count() > 120 {
                        let short: String = text.chars().take(80).collect();
                        text.clear();
                        text.push_slice(&short);
                    }
                }
            }
            return;
        }
        attrs.retain(|attr| {
            let key = attr.name.local.as_ref();
            matches!(
                key,
                "id" | "class"
                    | "name"
                    | "src"
                    | "href"
                    | "alt"
                    | "value"
                    | "type"
                    | "placeholder"
                    | "disabled"
                    | "checked"
                    | "selected"
                    | "readonly"
                    | "required"
                    | "multiple"
                    | "role"
                    | "aria-label"
                    | "aria-expanded"
                    | "aria-hidden"
                    | "contenteditable"
                    | "title"
                    | "for"
                    | "action"
                    | "method"
                    | "target"
                    | "colspan"
                    | "rowspan"
            ) || key.starts_with("data-")
        });
        for attr in attrs.iter_mut() {
            let key = attr.name.local.as_ref();
            let value = attr.value.to_string();
            // 全部按字符计数： 下 `len` 是字节数，和 `chars.take` 混用会让
            // 中文 URL / 属性值在没超阈值时就被替换成占位符。
            let value_chars = value.chars().count();
            if key == "src" {
                if value.starts_with("data:") {
                    attr.value.clear();
                    attr.value.push_slice("__img__");
                } else if value_chars > 30 {
                    attr.value.clear();
                    attr.value.push_slice("__url__");
                }
            } else if (key == "href" || key == "action") && value_chars > 30 {
                attr.value.clear();
                attr.value
                    .push_slice(if key == "href" { "__link__" } else { "__url__" });
            } else if matches!(key, "value" | "title" | "alt") && value_chars > 100 {
                attr.value.clear();
                attr.value.push_slice(&format!(
                    "{} ...",
                    value.chars().take(50).collect::<String>()
                ));
            } else if key.starts_with("data-") && !key.starts_with("data-v") && value_chars > 20 {
                attr.value.clear();
                attr.value.push_slice("__data__");
            }
        }
    }
    // 先迭代摘链再丢弃，避免深子树在 Drop 时递归爆栈。
    let children = handle.children.borrow().clone();
    let mut drop_any = false;
    for child in children {
        if should_drop_subtree(&child, keep) {
            discard_subtree(&child);
            drop_any = true;
            continue;
        }
        clean_node_at(&child, depth + 1, keep);
    }
    if drop_any {
        handle
            .children
            .borrow_mut()
            .retain(|child| !should_drop_subtree(child, keep));
    }
}

/// 迭代拆链：把 `handle` 的整棵子树拆散后丢弃。
///
/// 关键点是**先把自己的 children 摘空，再让节点被 drop**：这样 `Rc<Node>` 归零时
/// `Node::drop` 只会析构一个空的 `Vec<Handle>`，不会再沿深链递归。
fn discard_subtree(handle: &Handle) {
    let mut pending: Vec<Handle> = handle.children.borrow_mut().drain(..).collect();
    while let Some(node) = pending.pop() {
        pending.extend(node.children.borrow_mut().drain(..));
        // node 在这里离开作用域：它的 children 已经是空的，Drop 不会递归。
        drop(node);
    }
}

pub fn smart_truncate(html: String, budget: usize) -> String {
    // budget 是「字符」预算（下游按 token/可见字数设计），而 String::len 是 字节数。
    // 用字节数比较会让中文页面提前约 3 倍截断，这里统一按 chars().count() 计算。
    // 调用方可能仍然用字节长度做前置判断，所以「其实没超预算」时必须原样返回。
    if html.chars().count() <= budget {
        return html;
    }
    const MARKER: &str = " [TRUNCATED]";
    let marker_len = MARKER.chars().count();
    // 预算装不下标记时直接返回按字符截断的前缀（不加标记）：
    // 旧实现固定留 32 个字符给标记，budget < 32 时算出的 keep 是 0，
    // 结果变成 13 字符的 " [TRUNCATED]" —— 比预算还长，内容全丢。
    if budget <= marker_len {
        return html.chars().take(budget).collect();
    }
    let keep = budget - marker_len;
    let mut result: String = html.chars().take(keep).collect();
    result.push_str(MARKER);
    result
}

#[allow(dead_code)]
fn _parse_html_for_validation(html: &str) -> RcDom {
    parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut Cursor::new(html.as_bytes()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_truncate_counts_chars_not_bytes() {
        // 300 字节但只有 100 个字符：预算 100 时不该被截断。
        let html = "汉".repeat(100);
        assert!(!smart_truncate(html.clone(), 100).contains("[TRUNCATED]"));
        let truncated = smart_truncate(html, 50);
        assert!(truncated.ends_with(" [TRUNCATED]"));
        // 结果总长永远不超过预算，内容部分 = 预算 - 标记长度。
        assert_eq!(truncated.chars().count(), 50);
        let keep = 50 - " [TRUNCATED]".chars().count();
        assert!(truncated.starts_with(&"汉".repeat(keep)));
    }

    #[test]
    fn smart_truncate_keeps_prefix_when_budget_is_tiny() {
        // 预算小于标记长度时不能返回比预算更长的 " [TRUNCATED]"，要给真实前缀。
        let html = "abcdefghij".repeat(10);
        let out = smart_truncate(html.clone(), 1);
        assert_eq!(out, "a");
        let out = smart_truncate(html.clone(), 5);
        assert_eq!(out, "abcde");
        assert!(out.chars().count() <= 5);
    }

    #[test]
    fn oversized_input_degrades_without_parsing() {
        // 超过 8MB 的输入必须走降级路径：不解析、不 panic，且返回体量很小。
        let html = "<div>".repeat(MAX_HTML_INPUT_BYTES / 5 + 1);
        assert!(html.len() > MAX_HTML_INPUT_BYTES);
        let out = optimize_html_for_tokens(&html);
        assert!(out.starts_with("[html truncated: input > 8MB]"), "{out}");
        // 降级结果必须远小于输入（说明真的没有把整串复制出来）。
        assert!(out.len() < 1024, "len={}", out.len());
        assert!(!out.contains("<div>"));
    }

    #[test]
    fn oversized_input_keeps_text_content() {
        let filler = "x".repeat(MAX_HTML_INPUT_BYTES);
        let out = optimize_html_for_tokens(&format!("<p>hello</p>{filler}"));
        assert!(out.starts_with("[html truncated: input > 8MB]"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn node_explosion_degrades_to_text() {
        // 2.4 MB 的重复空 div 能生成 30 万个节点：输入没超 8 MB，但树本身会吃掉
        // 十几倍内存。必须走纯文本降级并标注原因。
        let html = "<div></div>".repeat(300_000);
        assert!(html.len() < MAX_HTML_INPUT_BYTES);
        let out = optimize_html_for_tokens(&html);
        assert!(
            out.starts_with("[html nodes > 200k]"),
            "{}",
            &out[..out.len().min(64)]
        );
        assert!(!out.contains("<div>"), "降级结果里不该再有标签");
        assert!(out.len() < 1024, "len={}", out.len());
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        // 远超深度上限的嵌套：不 panic，超出部分被丢弃、浅层内容保留。
        let depth = 10_000;
        let html = format!(
            "<p>deep</p>{}x{}",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        let out = optimize_html_for_tokens(&html);
        assert!(out.contains("deep"));
        assert!(out.matches("<div>").count() <= MAX_DOM_DEPTH + 2);
    }

    #[test]
    fn degraded_text_skips_script_style_and_comments() {
        // 降级路径不能把脚本 / 样式 / 注释内容当成正文。
        let html = "<p>hello</p><!-- secret --><script>alert('x')</script><style>.a{color:red}</style><p>world</p>";
        let out = strip_tags(html);
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("world"), "{out}");
        assert!(!out.contains("alert"), "{out}");
        assert!(!out.contains("color:red"), "{out}");
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn degraded_text_decodes_common_entities() {
        // 至少解码 &amp; &lt; &gt; &quot; &#39; &nbsp;。
        let out = strip_tags("<p>a&amp;b&nbsp;&lt;c&gt;&quot;d&#39;e</p>");
        assert_eq!(out, "a&b <c>\"d'e");
    }

    #[test]
    fn optimize_drops_script_and_style_subtrees() {
        // 正常解析路径也要丢弃 script/style/注释，而不是只清属性。
        let html =
            "<div>keep<script>var x = 1;</script><style>.a{}</style><!--c--><p>body</p></div>";
        let out = optimize_html_for_tokens(html);
        assert!(out.contains("keep"), "{out}");
        assert!(out.contains("body"), "{out}");
        assert!(!out.contains("var x"), "{out}");
        assert!(!out.contains(".a{}"), "{out}");
        assert!(!out.contains("<!--"), "{out}");
    }

    #[test]
    fn svg_keeps_readable_labels() {
        let html = r#"<button><svg aria-label="设置" role="img" data-testid="gear" viewBox="0 0 24 24" fill="none"><title>齿轮</title><path d="M0 0h24v24H0z"/></svg></button>"#;
        let out = optimize_html_for_tokens(html);
        assert!(out.contains("aria-label=\"设置\""), "{out}");
        assert!(out.contains("data-testid=\"gear\""), "{out}");
        assert!(out.contains("<title>齿轮</title>"), "{out}");
        // 绘制路径仍要丢掉，避免 token 浪费。
        assert!(!out.contains("<path"), "{out}");
        assert!(!out.contains("viewBox"), "{out}");
    }
}
