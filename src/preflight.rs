//! 命令预检：在把任何东西发给浏览器之前，先把「这条命令一定跑不通」的情况挡下来。
//!
//! 设计原则（对应「智能体一次就能写对」）：
//! 1. 只做**确定错**的判定，不做风格检查——预检误报比不检查更糟，会让智能体反复改对的命令。
//! 2. 每条错误都带 `hint`（下一步怎么做）和 `example`（可直接照抄的命令）。
//! 3. 纯本地计算，不发请求、不碰页面，所以永远不会产生副作用，也不需要回滚。
//!
//! 真正需要浏览器才能判断的事（标签页是否存在、选择器是否命中）不在这里做：
//! 那属于 `/multi` 的目标解析，同样在动手前完成。

use serde_json::{json, Value};
use std::path::Path;

/// 一条预检失败结果。`code` 会出现在 JSON 的 `error_code` 里，供脚本分支判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
    pub example: &'static str,
    pub field: Option<String>,
}

impl Problem {
    pub fn new(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            example: "",
            field: None,
        }
    }

    pub fn with_example(mut self, example: &'static str) -> Self {
        self.example = example;
        self
    }

    pub fn with_field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }

    /// 统一的错误 JSON。`ok:false` + 非 0 退出码（CLI 对 `usage_*` 用 2）。
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "ok": false,
            "error": self.message,
            "error_code": self.code,
            "hint": self.hint,
        });
        if let Some(field) = &self.field {
            value["field"] = json!(field);
        }
        if !self.example.is_empty() {
            value["example"] = json!(self.example);
        }
        value
    }
}

/// 选择器类型：CSS 与 XPath 的括号规则不同，分开检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind {
    Css,
    XPath,
}

/// 判断一个 `--target` / 选择器字符串该按哪种语法检查。
///
/// 判定顺序按 固定，避免「必然失败的输入被当成合法 CSS 静默放过」：
/// 1. 以 `(` 开头、内部含 `//` 或 `/` → 括号包着的 XPath；
/// 2. 以 `/`、`./`、`.//`、`..` 开头 → 路径式 XPath；
/// 3. `id("x")` / `name(` / `xpath(` 这类函数式写法 → XPath 函数；
/// 4. 其余才是 CSS。
pub fn selector_kind(selector: &str) -> SelectorKind {
    let trimmed = selector.trim_start();
    if trimmed.starts_with('(') && (trimmed.contains("//") || trimmed.contains('/')) {
        return SelectorKind::XPath;
    }
    if trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with(".//")
        || trimmed.starts_with("..")
    {
        return SelectorKind::XPath;
    }
    if is_xpath_function_form(trimmed) {
        return SelectorKind::XPath;
    }
    SelectorKind::Css
}

/// `id("x")` / `name(..)` / `xpath(..)` 这些是 XPath 1.0 函数，CSS 里没有对应的
/// 裸函数写法，所以按 XPath 检查括号。
fn is_xpath_function_form(trimmed: &str) -> bool {
    const FUNCTIONS: &[&str] = &[
        "id(",
        "name(",
        "xpath(",
        "count(",
        "contains(",
        "starts-with(",
        "text(",
        "local-name(",
        "namespace-uri(",
        "normalize-space(",
        "string(",
        "concat(",
        "substring(",
        "last(",
        "position(",
        "translate(",
        "not(",
    ];
    let lower = trimmed.to_ascii_lowercase();
    FUNCTIONS.iter().any(|function| lower.starts_with(function))
}

/// 选择器预检。
///
/// 允许 `@e12` 这类 snapshot 引用（由 snapshot 命令产生），其余按 CSS/XPath 检查。
pub fn validate_selector(selector: &str) -> Result<(), Problem> {
    let text = selector.trim();
    if text.is_empty() {
        return Err(Problem::new(
            "invalid_selector",
            "选择器是空的",
            "写一个 CSS 选择器，或先用 snapshot 拿到 @e 引用再传",
        )
        .with_example("atb click '#submit'")
        .with_field("target"));
    }
    if is_snapshot_ref(text) {
        return Ok(());
    }
    // Playwright 风格的 text= / role= 是智能体最常写错的一种：在这里直接给出替代命令，
    // 比让它发出去、等页面报 "not a valid selector" 再猜要快得多。
    if let Some(rest) = text.strip_prefix("text=") {
        return Err(Problem::new(
            "unsupported_selector_syntax",
            format!("`text={rest}` 是 Playwright 语法，这里不支持"),
            "按可见文本找元素用 find --text，拿到 selector 后再 click/fill",
        )
        .with_example("atb find --text '登录'")
        .with_field("target"));
    }
    if text.starts_with("xpath=") || text.starts_with("css=") {
        return Err(Problem::new(
            "unsupported_selector_syntax",
            "选择器不要带 `xpath=` / `css=` 前缀",
            "直接写 XPath（以 // 开头）或 CSS 选择器即可",
        )
        .with_example("atb click '//button[@type=\"submit\"]'")
        .with_field("target"));
    }

    match selector_kind(text) {
        SelectorKind::XPath => check_balanced(text, "XPath", &[('[', ']'), ('(', ')')]),
        SelectorKind::Css => {
            check_balanced(text, "CSS 选择器", &[('[', ']'), ('(', ')')])?;
            if text.starts_with('>') || text.starts_with('+') || text.starts_with('~') {
                return Err(Problem::new(
                    "invalid_selector",
                    format!("CSS 选择器不能以组合符 {text:?} 开头"),
                    "组合符前面要有元素，比如 `div > span`；要找子元素也可以直接写完整选择器",
                )
                .with_example("atb click 'form > button'")
                .with_field("target"));
            }
            Ok(())
        }
    }
}

fn is_snapshot_ref(text: &str) -> bool {
    let mut chars = text.chars();
    if chars.next() != Some('@') {
        return false;
    }
    let rest: String = chars.collect();
    if let Some(number) = rest.strip_prefix('e') {
        return !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit());
    }
    false
}

/// 括号 / 引号配平检查，错误里带行列号。
///
/// **先查引号，再查括号**：`input[name="q` 里看起来是第 6 列的 `[` 没闭合，
/// 真正的原因是引号把后面整个吞了。先报括号会把用户引向错误的修改方向，多行输入时
/// 括号分支还不带引号位置。
fn check_balanced(text: &str, label: &str, pairs: &[(char, char)]) -> Result<(), Problem> {
    check_quotes(text, label)?;
    check_brackets(text, label, pairs)
}

/// 引号配平；未闭合时给出「引号从第 N 行第 M 列开始」的提示。
fn check_quotes(text: &str, label: &str) -> Result<(), Problem> {
    let mut quote: Option<(char, usize)> = None;
    let mut escaped = false;
    for (offset, ch) in text.char_indices() {
        match quote {
            Some((active, _)) => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == active {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some((ch, offset));
                }
            }
        }
    }
    if let Some((active, open_at)) = quote {
        let (line, column) = line_col(text, open_at);
        return Err(Problem::new(
            "invalid_selector",
            format!("{label}第 {line} 行第 {column} 列的引号 {active:?} 没有闭合"),
            format!(
                "引号从第 {line} 行第 {column} 列开始，到结尾都没有对应的 {active}；补上引号，\
                 或写成属性选择器 [name=\"q\"]"
            ),
        )
        .with_example("atb click 'a[href*=\"login\"]'")
        .with_field("target"));
    }
    Ok(())
}

/// 括号配平（引号里的内容不参与配对，调用前 `check_quotes` 已保证引号成对）。
fn check_brackets(text: &str, label: &str, pairs: &[(char, char)]) -> Result<(), Problem> {
    let mut stack: Vec<(char, char, usize)> = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if let Some((_, close)) = pairs.iter().find(|(open, _)| *open == ch) {
            let close = *close;
            stack.push((ch, close, offset));
            continue;
        }
        if let Some(position) = pairs.iter().position(|(_, close)| *close == ch) {
            let open = pairs[position].0;
            match stack.pop() {
                Some((stacked, _, _)) if stacked == open => {}
                Some((stacked, _, _)) => {
                    let (line, column) = line_col(text, offset);
                    return Err(Problem::new(
                        "invalid_selector",
                        format!(
                            "{label}第 {line} 行第 {column} 列：{ch:?} 与前面的 {stacked:?} 不配对"
                        ),
                        format!("检查第 {line} 行附近的括号顺序，或者删掉多余的 {ch:?}"),
                    )
                    .with_field("target"));
                }
                None => {
                    let (line, column) = line_col(text, offset);
                    return Err(Problem::new(
                        "invalid_selector",
                        format!("{label}第 {line} 行第 {column} 列：多了一个 {ch:?}"),
                        "删掉多余的右括号，或补上对应的左括号",
                    )
                    .with_field("target"));
                }
            }
            continue;
        }
    }
    if let Some((open, close, open_at)) = stack.pop() {
        let (line, column) = line_col(text, open_at);
        return Err(Problem::new(
            "invalid_selector",
            format!("{label}第 {line} 行第 {column} 列的 {open:?} 没有闭合，缺一个 {close:?}"),
            format!("在末尾或属性值后补上 {close:?}；属性选择器形如 [name=\"q\"]"),
        )
        .with_example("atb click 'input[name=\"q\"]'")
        .with_field("target"));
    }
    Ok(())
}

/// 本地 JS 配平检查档位（`ATB_JS_CHECK`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsCheckPolicy {
    /// 默认：判为不配平时直接 exit 2。
    Strict,
    /// 跳过检查，但 CLI 在 stderr 提示一次；脚本交给浏览器实际执行。
    Warn,
    /// 完全跳过本地检查（与 `--no-preflight` 等价）。
    Off,
}

/// 读取 `ATB_JS_CHECK` 并映射成档位；非法值由 `settings::js_check()` 回落到 strict。
pub fn js_check_policy() -> JsCheckPolicy {
    match crate::settings::js_check().as_str() {
        "off" => JsCheckPolicy::Off,
        "warn" => JsCheckPolicy::Warn,
        _ => JsCheckPolicy::Strict,
    }
}

/// 这个 payload 是否应跳过本地 JS 配平检查。
///
/// 两个来源：显式 `no_preflight`（`exec --no-preflight` / `inject --no-preflight`），
/// 或全局档位 `ATB_JS_CHECK=warn|off`。只影响 JS 配平检查，选择器/路径/数值预检
/// 仍然照常执行。
pub fn js_check_skipped(payload: &Value) -> bool {
    payload
        .get("no_preflight")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || js_check_policy() != JsCheckPolicy::Strict
}

/// JS 预检：括号配平 + 字符串/注释跳过。
///
/// 不追求完整语法分析，只覆盖「少一个大括号」「字符串里的引号没配对」这类
/// 会让页面直接抛 SyntaxError 的确定性错误。
pub fn validate_js(code: &str) -> Result<(), Problem> {
    if code.trim().is_empty() {
        return Err(Problem::new(
            "empty_script",
            "要执行的 JS 是空的",
            "把脚本写在参数里，或用 --file - 从 stdin 读",
        )
        .with_example("atb exec 'return document.title'")
        .with_field("script"));
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Normal,
        LineComment,
        BlockComment,
        Single,
        Double,
        Template,
        /// 正则字面量 `/.../flags`：里面的括号、引号都不参与配平。
        Regex,
    }

    let chars: Vec<char> = code.chars().collect();
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut mode = Mode::Normal;
    let mut escaped = false;
    let mut index = 0usize;
    // 未闭合字面量的起始位置：EOF 时的错误必须指回「从哪开始」，只报文件末尾
    // 对排查长脚本没有帮助。
    let mut block_comment_start: Option<usize> = None;
    let mut string_start: Option<(usize, char)> = None;
    let mut template_start: Option<usize> = None;
    // 模板字符串里的 ${...} 需要回到 Normal 解析表达式，用栈记录模板深度。
    let mut template_expr_depth: Vec<usize> = Vec::new();
    // `/` 是正则开头还是除号，取决于前一个有效 token 是不是「操作数」：
    // `return /[)]/` 是正则，`a / b` 是除号。少了这个状态机，`/[)]/` 会被当成
    // 除号 + 括号，报「`[` 没有闭合」。
    let mut expect_operand = true;
    let mut regex_in_class = false;
    let mut regex_start = 0usize;
    // 前一个有效 token 是不是闭合括号（`}` / `]` / `)`）。闭合括号之后的 `/` 既可能
    // 是除号（`(a+b)/2`），也可能是「语句块结束后新起的正则」（`{a:1}\n/[)]/`），
    // 需要向前看才能分清。
    let mut last_closed_bracket = false;

    while index < chars.len() {
        let ch = chars[index];
        let next = chars.get(index + 1).copied();
        match mode {
            Mode::LineComment => {
                if ch == '\n' {
                    mode = Mode::Normal;
                }
            }
            Mode::BlockComment => {
                if ch == '*' && next == Some('/') {
                    mode = Mode::Normal;
                    index += 1;
                }
            }
            Mode::Single | Mode::Double => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if (mode == Mode::Single && ch == '\'')
                    || (mode == Mode::Double && ch == '"')
                {
                    mode = Mode::Normal;
                    // 字符串本身就是一个完整的操作数。
                    expect_operand = false;
                } else if ch == '\n' {
                    let (line, column) = line_col_from_chars(&chars, index);
                    return Err(Problem::new(
                        "invalid_js",
                        format!("第 {line} 行第 {column} 列的字符串没有闭合就换行了"),
                        "单双引号字符串不能跨行；跨行文本用模板字符串 `...`，或把换行转义成 \\n",
                    )
                    .with_example("atb exec 'return document.title'")
                    .with_field("script"));
                }
            }
            Mode::Template => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '`' {
                    mode = Mode::Normal;
                    expect_operand = false;
                } else if ch == '$' && next == Some('{') {
                    template_expr_depth.push(stack.len());
                    stack.push(('{', index));
                    mode = Mode::Normal;
                    expect_operand = true;
                    index += 1;
                }
            }
            Mode::Regex => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\n' {
                    let (line, column) = line_col_from_chars(&chars, regex_start);
                    return Err(Problem::new(
                        "invalid_js",
                        format!("第 {line} 行第 {column} 列的正则字面量没有闭合就换行了"),
                        "正则字面量不能跨行：补上结尾的 `/`，或把换行写成 \\n",
                    )
                    .with_field("script"));
                } else if ch == '[' {
                    // 字符组里的 `/` 不结束正则。
                    regex_in_class = true;
                } else if ch == ']' && regex_in_class {
                    regex_in_class = false;
                } else if ch == '/' && !regex_in_class {
                    mode = Mode::Normal;
                    expect_operand = false;
                    // 吃掉后面的 flags（gimsuy），它们属于正则字面量本身。
                    let mut end = index + 1;
                    while end < chars.len() && chars[end].is_ascii_alphabetic() {
                        end += 1;
                    }
                    index = end - 1;
                }
            }
            Mode::Normal => {
                // 先记住上一轮的状态，再为本轮 token 复位（`/` 分支还要用它）。
                let just_closed = last_closed_bracket;
                // 注释和空白不代表「新的有效 token」，其余 token 都会打断「刚闭合括号」
                // 的状态；闭合分支自己再把它置回 true。
                if !ch.is_whitespace() && !(ch == '/' && matches!(next, Some('/') | Some('*'))) {
                    last_closed_bracket = false;
                }
                if ch.is_whitespace() {
                    // 空白不改变「前一个有效 token」。
                } else if ch == '/' && next == Some('/') {
                    mode = Mode::LineComment;
                    index += 1;
                } else if ch == '/' && next == Some('*') {
                    mode = Mode::BlockComment;
                    block_comment_start = Some(index);
                    index += 1;
                } else if ch == '/' {
                    if expect_operand {
                        // 期待操作数时 `/` 是正则开头（`/[)]/`、`/["']/`、`/a/*2`
                        // 都是浏览器里合法的 JS）。按除号/块注释解析会误报。
                        mode = Mode::Regex;
                        regex_in_class = false;
                        regex_start = index;
                        escaped = false;
                    } else if just_closed && regex_has_terminator(&chars, index + 1) {
                        // `}` / `]` / `)` 之后紧跟 `/`：同一行后面还有未转义的 `/` 时
                        // 按正则处理（`{a:1}\n/[)]/.test(x)`），否则按除号处理
                        // （`(a+b) / 2`）——两者在浏览器里都合法，必须靠向前看区分。
                        mode = Mode::Regex;
                        regex_in_class = false;
                        regex_start = index;
                        escaped = false;
                    } else {
                        // 除号或 `/=`：`a / b`、`(a+b) / 2`、`x /= 2`。
                        expect_operand = true;
                    }
                } else if ch == '"' {
                    mode = Mode::Double;
                    string_start = Some((index, ch));
                } else if ch == '\'' {
                    mode = Mode::Single;
                    string_start = Some((index, ch));
                } else if ch == '`' {
                    mode = Mode::Template;
                    template_start = Some(index);
                } else if ch == '{' || ch == '(' || ch == '[' {
                    stack.push((ch, index));
                    expect_operand = true;
                } else if ch == '}' || ch == ')' || ch == ']' {
                    let expected = match ch {
                        '}' => '{',
                        ')' => '(',
                        _ => '[',
                    };
                    match stack.pop() {
                        Some((open, _)) if open == expected => {
                            // 关掉模板表达式时回到 Template 模式。
                            if ch == '}' && template_expr_depth.last().copied() == Some(stack.len())
                            {
                                template_expr_depth.pop();
                                mode = Mode::Template;
                            }
                        }
                        Some((open, at)) => {
                            let (line, column) = line_col_from_chars(&chars, at);
                            return Err(Problem::new(
                                "invalid_js",
                                format!(
                                    "第 {} 行第 {} 列的 {ch:?} 关不掉第 {line} 行第 {column} 列的 {open:?}",
                                    line_col_from_chars(&chars, index).0,
                                    line_col_from_chars(&chars, index).1
                                ),
                                "按后进先出的顺序闭合括号；注释和字符串里的括号不参与配对",
                            )
                            .with_field("script"));
                        }
                        None => {
                            let (line, column) = line_col_from_chars(&chars, index);
                            return Err(Problem::new(
                                "invalid_js",
                                format!("第 {line} 行第 {column} 列多了一个 {ch:?}"),
                                "删掉多余的右括号，或补上对应的左括号",
                            )
                            .with_example("atb exec 'return document.title'")
                            .with_field("script"));
                        }
                    }
                    expect_operand = false;
                    // 记住「刚刚闭合了括号」：后面的 `/` 需要向前看才能分清正则/除号。
                    last_closed_bracket = true;
                } else if ch.is_ascii_digit() {
                    // 数字字面量整体读完，`1e5`、`0x1f` 后面的 `/` 才是除号。
                    let mut end = index + 1;
                    while end < chars.len()
                        && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '.' | '_'))
                    {
                        end += 1;
                    }
                    expect_operand = false;
                    index = end - 1;
                } else if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                    // 读完整标识符：`return` / `case` 后面跟的正则要和变量名区分开。
                    let mut end = index + 1;
                    while end < chars.len()
                        && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '_' | '$'))
                    {
                        end += 1;
                    }
                    let word: String = chars[index..end].iter().collect();
                    expect_operand = matches!(
                        word.as_str(),
                        "return"
                            | "case"
                            | "typeof"
                            | "instanceof"
                            | "in"
                            | "of"
                            | "new"
                            | "delete"
                            | "void"
                            | "throw"
                            | "do"
                            | "else"
                            | "yield"
                            | "await"
                    );
                    index = end - 1;
                } else if ch == '+' || ch == '-' {
                    // `i++ / 2` 里的 `/` 是除号：自增/自减不改变「要不要操作数」。
                    if next == Some(ch) {
                        index += 1;
                    } else {
                        expect_operand = true;
                    }
                } else if matches!(
                    ch,
                    ',' | ';'
                        | ':'
                        | '='
                        | '!'
                        | '&'
                        | '|'
                        | '*'
                        | '%'
                        | '<'
                        | '>'
                        | '?'
                        | '~'
                        | '^'
                ) {
                    expect_operand = true;
                }
            }
        }
        index += 1;
    }

    match mode {
        Mode::BlockComment => {
            let position = block_comment_start
                .map(|start| {
                    let (line, column) = line_col_from_chars(&chars, start);
                    format!("第 {line} 行第 {column} 列开始的")
                })
                .unwrap_or_default();
            return Err(Problem::new(
                "invalid_js",
                format!("{position}块注释 /* 没有闭合"),
                "补上 */，或改成 // 行注释",
            )
            .with_field("script"));
        }
        Mode::Single | Mode::Double => {
            let position = string_start
                .map(|(start, quote)| {
                    let (line, column) = line_col_from_chars(&chars, start);
                    format!("第 {line} 行第 {column} 列的 {quote:?} 字符串没有闭合")
                })
                .unwrap_or_else(|| "字符串结尾的引号没有闭合".to_string());
            return Err(Problem::new(
                "invalid_js",
                position,
                "检查引号是否成对；shell 单引号里不能直接写单引号，可改用双引号包裹",
            )
            .with_example("atb exec \"return document.title\"")
            .with_field("script"));
        }
        Mode::Template => {
            let position = template_start
                .map(|start| {
                    let (line, column) = line_col_from_chars(&chars, start);
                    format!("第 {line} 行第 {column} 列开始的模板字符串 ` 没有闭合")
                })
                .unwrap_or_else(|| "模板字符串 ` 没有闭合".to_string());
            return Err(
                Problem::new("invalid_js", position, "补上结尾的反引号").with_field("script")
            );
        }
        Mode::Regex => {
            let (line, column) = line_col_from_chars(&chars, regex_start);
            return Err(Problem::new(
                "invalid_js",
                format!("第 {line} 行第 {column} 列开始的正则字面量没有闭合（缺结尾的 `/`）"),
                "补上结尾的 `/`；如果本意是除号，检查运算符左边是不是写成了不完整的表达式",
            )
            .with_field("script"));
        }
        Mode::Normal | Mode::LineComment => {}
    }

    if let Some((open, at)) = stack.pop() {
        let close = match open {
            '{' => '}',
            '(' => ')',
            _ => ']',
        };
        let (line, column) = line_col_from_chars(&chars, at);
        return Err(Problem::new(
            "invalid_js",
            format!("第 {line} 行第 {column} 列的 {open:?} 没有闭合，缺一个 {close:?}"),
            format!("在脚本末尾补上 {close:?}；只写表达式也行，CLI 会自动 return"),
        )
        .with_example("atb exec 'document.title'")
        .with_field("script"));
    }
    Ok(())
}

/// `start` 之后（同一行内）是否存在未转义的 `/`，并且这个 `/` 之前没有未闭合的
/// `(` / `{`。也就是「这里能不能是一个完整的正则字面量」。
///
/// 正则字面量不能跨行，所以「同一行没有结尾 `/`」就说明这里更可能是除号
// （`(a+b) / 2`）。扫描时跳过字符类 `[...]` 和转义字符。
///
// 括号深度是 的收紧：`count(a) / (total / 100)` 里，`)` 后第一个 `/`
/// 后面紧跟着未闭合的 `(`，里面的 `/` 不是正则的结尾而是除号；旧实现把它当正则
/// 起始，最终报「多了一个 `)`」。`{` 同理（`{a:1} / {b:2}`）。字符类里的括号
/// 不参与深度（`/[(]/` 仍然能正确识别为正则）。
fn regex_has_terminator(chars: &[char], start: usize) -> bool {
    let mut escaped = false;
    let mut in_class = false;
    let mut depth = 0usize;
    for &ch in chars.get(start..).unwrap_or(&[]) {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\n' | '\r' => return false,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' | '{' if !in_class => depth += 1,
            ')' | '}' if !in_class => depth = depth.saturating_sub(1),
            '/' if !in_class => return depth == 0,
            _ => {}
        }
    }
    false
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    line_col_in(text, offset)
}

fn line_col_from_chars(chars: &[char], index: usize) -> (usize, usize) {
    let prefix: String = chars[..index.min(chars.len())].iter().collect();
    line_col_in(&prefix, prefix.len())
}

fn line_col_in(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for (index, ch) in text.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// URL / 打开目标预检：只挡「一定不可能成功」的输入，规范化交给 daemon。
pub fn validate_open_target(target: &str) -> Result<(), Problem> {
    let text = target.trim();
    if text.is_empty() {
        return Err(Problem::new(
            "invalid_target",
            "打开目标是空的",
            "写 URL 或本地文件路径，多个目标用空格分隔",
        )
        .with_example("atb open https://example.com https://example.org")
        .with_field("targets"));
    }
    if text
        .chars()
        .any(|ch| ch.is_control() || ch == '\n' || ch == '\t')
    {
        return Err(Problem::new(
            "invalid_target",
            format!("打开目标 {text:?} 含控制字符"),
            "URL 里不要带换行/制表符；多行输入请每行一个目标并用 xargs 传",
        )
        .with_field("targets"));
    }
    if text.starts_with('-') {
        return Err(Problem::new(
            "invalid_target",
            format!("打开目标 {text:?} 以 - 开头，会被当成命令行参数"),
            // hint 必须是**真能跑通**的写法：之前建议 `atb open -- -draft.html`，但
            // `--` 之后的参数在这里拿不到，仍然被同一条规则拒绝，形成死循环。
            format!("加 ./ 前缀写成相对路径：`atb open ./{text}`；也可以用绝对路径"),
        )
        .with_example("atb open ./-draft.html")
        .with_field("targets"));
    }
    Ok(())
}

/// 超时类数值的安全区间（秒）。
///
/// `Duration::from_secs_f64` 遇到 NaN / 负数 / 超大值（例如 `1e300`）会直接 panic，
/// 所以任何往下传超时的地方都必须先过 `clamp_timeout_seconds`。
pub const TIMEOUT_MIN_SECONDS: f64 = 0.1;
pub const TIMEOUT_MAX_SECONDS: f64 = 3600.0;

/// 把超时收敛到 `0.1..=3600`，保证可以安全传给 `Duration::from_secs_f64`。
///
/// 非有限值（NaN/Inf）按最小值处理：宁可立刻超时并报错，也不要让命令静默挂一小时。
// 旧注释声称 daemon 侧尚未接入，实际 server/state.rs 的 secs() 与 cli/http.rs 的 secs()
// 都已走本函数，过期的 allow(dead_code) 连同该注释一并删除。
pub fn clamp_timeout_seconds(value: f64) -> f64 {
    if !value.is_finite() {
        return TIMEOUT_MIN_SECONDS;
    }
    value.clamp(TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS)
}

/// 这个超时值是否会被 `clamp_timeout_seconds` 改动（调用方可在结果里带
/// `timeout_clamped: true`）。
// 目前仅单元测试使用（生产路径尚未回报「参数被收敛过」），allow(dead_code) 仍需
// 保留：非 test 编译下它是死代码，与上面已接入的 clamp 不同。
#[allow(dead_code)]
pub fn timeout_clamped(value: f64) -> bool {
    clamp_timeout_seconds(value).to_bits() != value.to_bits()
}

/// 本地文件参数预检（上传等）。
///
/// 已接入 `preflight_payload` 的 `upload` 分支；只校验**绝对路径**——相对路径由
/// daemon 按它自己的 cwd 解析，CLI 侧判断可能和 daemon 不一致，误报比漏报更糟。
pub fn validate_existing_file(path: &str) -> Result<(), Problem> {
    let meta = std::fs::metadata(path).map_err(|err| {
        Problem::new(
            "file_not_found",
            format!("文件 {path:?} 不可读：{err}"),
            "确认路径存在；相对路径以当前工作目录为基准，可用绝对路径避免歧义",
        )
        .with_field("files")
    })?;
    if !meta.is_file() {
        return Err(Problem::new(
            "not_a_file",
            format!("{path:?} 不是普通文件"),
            "上传/读取只接受文件；目录会在这里被拒绝",
        )
        .with_field("files"));
    }
    Ok(())
}

/// 数值参数预检：NaN/Inf 报 `invalid_number`，越界报同一错误码。
///
/// 这里必须是**报错**而不是夹到边界：`json!` 会把 NaN 序列化成 `null`，daemon 侧
/// `Option<f64>` 读到 `None` 后会静默用默认值，用户永远不知道参数被忽略。
pub fn validate_number(field: &str, value: f64, min: f64, max: f64) -> Result<(), Problem> {
    if !value.is_finite() {
        return Err(Problem::new(
            "invalid_number",
            format!("{field} 不是有限数值（NaN/Infinity 无法传给 daemon）"),
            format!("把 {field} 写成普通数字，例如 {min}"),
        )
        .with_field(field));
    }
    if value < min || value > max {
        return Err(Problem::new(
            "invalid_number",
            format!("{field} 取值 {value} 超出允许范围 [{min}, {max}]"),
            format!("把 {field} 调整到 {min}..{max} 之间"),
        )
        .with_field(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_selectors() {
        for selector in [
            "#submit",
            ".btn.primary",
            "div > span",
            "input[name=\"q\"]",
            "a[href*='login']",
            "@e12",
            "//button[@type=\"submit\"]",
        ] {
            assert!(
                validate_selector(selector).is_ok(),
                "should accept {selector}: {:?}",
                validate_selector(selector)
            );
        }
    }

    #[test]
    fn rejects_unclosed_attribute() {
        let err = validate_selector("input[name=\"q\"").unwrap_err();
        assert_eq!(err.code, "invalid_selector");
        assert!(err.message.contains("没有闭合"));
        assert!(!err.hint.is_empty());
    }

    #[test]
    fn rejects_playwright_syntax_with_alternative() {
        let err = validate_selector("text=登录").unwrap_err();
        assert_eq!(err.code, "unsupported_selector_syntax");
        assert!(err.example.contains("find --text"));
    }

    #[test]
    fn js_balance_ignores_strings_and_comments() {
        assert!(validate_js("return 'a(b' + \"c{d\"").is_ok());
        assert!(validate_js("// }\nreturn 1").is_ok());
        assert!(validate_js("const t = `a${b + 1}c`; return t;").is_ok());
        assert!(validate_js("const re = /a{2}/; return re.test('aa');").is_ok());
    }

    #[test]
    fn js_reports_missing_brace_with_position() {
        let err = validate_js("if (a) {\n  return 1;\n").unwrap_err();
        assert_eq!(err.code, "invalid_js");
        assert!(err.message.contains("缺一个"));
        assert!(err.message.contains("第 1 行"));
    }

    #[test]
    fn unclosed_literals_report_start_position() {
        // EOF 类错误必须指回字面量开始处，而不是只给一句没有位置的文案。
        let comment = validate_js("const a = 1;\n/* still open").unwrap_err();
        assert!(comment.message.contains("第 2 行"), "{}", comment.message);

        let string = validate_js("const a = 1;\nconst b = 'open").unwrap_err();
        assert!(string.message.contains("第 2 行"), "{}", string.message);
        assert!(string.message.contains("第 11 列"), "{}", string.message);

        let template = validate_js("const a = 1;\nconst b = `open").unwrap_err();
        assert!(template.message.contains("第 2 行"), "{}", template.message);

        let regex = validate_js("const a = 1;\nconst r = /open").unwrap_err();
        assert!(regex.message.contains("第 2 行"), "{}", regex.message);
    }

    #[test]
    fn js_reports_extra_paren() {
        let err = validate_js("return (1));").unwrap_err();
        assert!(err.message.contains("多了一个"));
    }

    #[test]
    fn empty_script_is_rejected() {
        assert_eq!(validate_js("   ").unwrap_err().code, "empty_script");
    }

    #[test]
    fn js_regex_literals_are_not_comments_or_brackets() {
        // 三条都是浏览器里合法、但旧状态机会误报的写法。
        for code in ["return /[)]/.test(s)", "/[\"']/", "return /a/*2"] {
            assert!(validate_js(code).is_ok(), "{code}: {:?}", validate_js(code));
        }
        // 除号不能被当成正则开头。
        assert!(validate_js("return a / b / c").is_ok());
        assert!(validate_js("const x = 10 / 2; return x;").is_ok());
        // 正则里真正的写法：字符组内的 `/` 不结束正则。
        assert!(validate_js("return /[a/]/.test(s)").is_ok());
    }

    #[test]
    fn js_regex_after_closing_bracket_is_not_division() {
        // 语句块 / 数组下标结束后新起正则：旧状态机把 `/` 一律当除号，字符类里的
        // `)` / `}` 于是被当成多余括号，报 invalid_js。
        for code in [
            "{a:1}\n/[)]/.test(x)",
            "if (a) { b(); }\n/[}]/.test(s)",
            "arr[0]\n/[(]/.test(s)",
        ] {
            assert!(validate_js(code).is_ok(), "{code}: {:?}", validate_js(code));
        }
        // 真正的除号不能因为「前面是括号」就被误判成正则。
        assert!(validate_js("return (a + b) / 2").is_ok());
        assert!(validate_js("const x = (a) / 2; return x;").is_ok());
        assert!(validate_js("x = a / b; /re/.test(s)").is_ok());
        // 除号后面是括号表达式时，括号里的 `/` 不是正则终止符。
        assert!(validate_js("return count(a) / (total / 100)").is_ok());
        assert!(validate_js("const ratio = (a + b) / (c / d); return ratio;").is_ok());
        assert!(validate_js("return (a) / ({x: 1}.x / 2)").is_ok());
        // 但 `x = /re/.test(s)` 仍然是正则字面量，不能被当成除号。
        assert!(validate_js("x = /re/.test(s)").is_ok());
    }

    #[test]
    fn selector_kind_recognizes_xpath_forms() {
        for selector in [
            "( //div )[1]",
            "(//div)[1]",
            ".//div",
            "/HTML/body",
            "id(\"x\")",
            "name(//input)",
        ] {
            assert_eq!(selector_kind(selector), SelectorKind::XPath, "{selector}");
        }
        for selector in ["div > span", "#submit", "input[name=\"q\"]", ".btn"] {
            assert_eq!(selector_kind(selector), SelectorKind::Css, "{selector}");
        }
    }

    #[test]
    fn xpath_like_selectors_are_accepted() {
        for selector in [
            "( //div )[1]",
            ".//div[@id=\"a\"]",
            "/HTML/body",
            "id(\"x\")",
        ] {
            assert!(
                validate_selector(selector).is_ok(),
                "{selector}: {:?}",
                validate_selector(selector)
            );
        }
    }

    #[test]
    fn unclosed_quote_is_reported_before_brackets() {
        // 旧实现会报「第 6 列的 `[` 没有闭合」，把用户引向错误方向。
        let err = validate_selector("input[name=\"q").unwrap_err();
        assert_eq!(err.code, "invalid_selector");
        assert!(err.message.contains("引号"), "{}", err.message);
        assert!(err.message.contains("第 1 行第 12 列"), "{}", err.message);
        assert!(
            err.hint.contains("引号从第 1 行第 12 列开始"),
            "{}",
            err.hint
        );
    }

    #[test]
    fn unclosed_quote_on_second_line_carries_position() {
        let err = validate_selector("div,\nspan[title=\"x").unwrap_err();
        assert!(err.message.contains("第 2 行"), "{}", err.message);
        assert!(err.message.contains("引号"), "{}", err.message);
    }

    #[test]
    fn leading_dash_target_hint_is_actually_usable() {
        let err = validate_open_target("-draft.html").unwrap_err();
        assert!(err.hint.contains("./-draft.html"), "{}", err.hint);
        // hint 里给的写法自己必须能通过预检，否则就是死循环。
        assert!(validate_open_target("./-draft.html").is_ok());
    }

    #[test]
    fn timeout_clamp_is_safe_for_duration() {
        assert_eq!(clamp_timeout_seconds(1e300), TIMEOUT_MAX_SECONDS);
        assert!(timeout_clamped(1e300));
        assert_eq!(clamp_timeout_seconds(f64::NAN), TIMEOUT_MIN_SECONDS);
        assert!(timeout_clamped(f64::NAN));
        assert_eq!(clamp_timeout_seconds(5.0), 5.0);
        assert!(!timeout_clamped(5.0));
        // 真正构造一次 Duration，确认 clamp 之后不会再 panic。
        let _ = std::time::Duration::from_secs_f64(clamp_timeout_seconds(1e300));
    }
}

/// `preflight_payload` 检查的数值字段：`(字段名, 最小值, 最大值)`。
///
/// 这些字段在 CLI 侧一定来自 f64 参数，所以 JSON 里出现 `null` 只可能是 NaN/Inf
/// （`json!` 把非有限浮点序列化成 null）——必须报 `invalid_number`，不能默认放过，
/// 否则 daemon 的 `Option<f64>` 会读成 None 并静默使用默认值。
const NUMERIC_FIELDS: &[(&str, f64, f64)] = &[
    ("timeout", TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS),
    (
        "readiness_timeout",
        TIMEOUT_MIN_SECONDS,
        TIMEOUT_MAX_SECONDS,
    ),
    ("wait_timeout", TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS),
    // 挑战页 / 动作后的自动等待：语义就是秒，跟 timeout 同口径。
    ("challenge_timeout", 0.0, TIMEOUT_MAX_SECONDS),
    ("reload_after", 0.0, TIMEOUT_MAX_SECONDS),
    ("focus_after", 0.0, TIMEOUT_MAX_SECONDS),
    // 毫秒值：上限 60s，避免 1e300 之类的值进到 Duration::from_millis。
    ("confirm_ms", 0.0, 60_000.0),
    ("interval", 0.05, 5.0),
    ("wait_interval", 0.01, 5.0),
    ("scale", 0.1, 2.0),
    ("x", 0.0, 100_000.0),
    ("y", 0.0, 100_000.0),
    // 计数 / 字节类的 usize 参数：JSON 里是整数，但同样可能被写成 1e300 / NaN。
    // 上限取「正常用得到、又不会把内存吃光」的数量级，不要压过合法取值。
    ("limit", 0.0, 1_000_000.0),
    ("max_nodes", 0.0, 1_000_000.0),
    ("max_chars", 0.0, 10_000_000.0),
    ("max_bytes", 0.0, 10_000_000.0),
];

/// 顶层、`params` / `args` 以及 `steps[*]` 里的数值字段都要查。
///
/// 动作类命令把坐标/滚动量放在 `params` 里；batch / run 把每一步的参数放在
/// `steps[*].args`，只查顶层会漏掉整条管道里的非法超时。
fn check_numeric_fields(payload: &Value) -> Result<(), Problem> {
    let mut sources: Vec<&Value> = vec![payload];
    for key in ["params", "args"] {
        if let Some(value) = payload.get(key) {
            sources.push(value);
        }
    }
    if let Some(steps) = payload.get("steps").and_then(Value::as_array) {
        for step in steps {
            sources.push(step);
            for key in ["params", "args"] {
                if let Some(value) = step.get(key) {
                    sources.push(value);
                }
            }
        }
    }
    for source in sources {
        let Some(object) = source.as_object() else {
            continue;
        };
        for (field, min, max) in NUMERIC_FIELDS {
            let Some(value) = object.get(*field) else {
                continue;
            };
            match value {
                Value::Number(number) => {
                    let Some(number) = number.as_f64() else {
                        return Err(Problem::new(
                            "invalid_number",
                            format!("{field} 不是可解析的数值"),
                            format!("把 {field} 写成普通数字，例如 {min}"),
                        )
                        .with_field(field));
                    };
                    validate_number(field, number, *min, *max)?;
                }
                Value::Null => {
                    return Err(Problem::new(
                        "invalid_number",
                        format!("{field} 不是有限数值（NaN/Infinity 会被序列化成 null）"),
                        format!("把 {field} 写成普通数字，例如 {min}"),
                    )
                    .with_field(field));
                }
                // 字符串等类型交给服务端自己报，避免在这里制造误报。
                _ => {}
            }
        }
    }
    Ok(())
}

/// `exec` 的 script 看起来像扩展控制帧（JSON 对象 + 字符串 `cmd`），但没有通过
/// `page_data::is_extension_json`（缺 `__atb_internal_cmd:true` 或 cmd 不在白名单）。
///
/// 只对含 `cmd` 的 JSON 对象报错，避免把合法的 JS 对象字面量误判；其余 JSON 形状
/// 仍交给 `validate_js` 按普通 JS 预检。
fn non_internal_extension_json(script: &str) -> Option<Problem> {
    let trimmed = script.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    if !value.is_object() || !value.get("cmd").is_some_and(Value::is_string) {
        return None;
    }
    Some(
        Problem::new(
            "invalid_json",
            "exec 收到的是扩展控制命令 JSON，但缺少 __atb_internal_cmd:true 或 cmd 不在内部白名单内",
            "扩展控制帧只由 daemon 内部构造；执行 JS 请写普通脚本，要返回对象用 `return ({...})`",
        )
        .with_example("atb exec 'return {ok: true}'")
        .with_field("script"),
    )
}

/// 按 op 对已经构造好的 payload 做统一预检。
///
/// 放在 CLI 侧、发请求之前：错的命令不会先打到 daemon 再等一个页面级错误回来。
/// 只检查「一定错」的字段，缺省值/由服务端补全的字段一律放过。
pub fn preflight_payload(op: &str, payload: &Value) -> Result<(), Problem> {
    let selector_field = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    // exec 也接受扩展协议 JSON（内部控制帧），但那必须是 daemon 构造的：
    // 只有带 `__atb_internal_cmd:true` 且 cmd 在内部白名单内才跳过 JS 预检。
    // 用户脚本里自然写出的 `{"cmd":"status"}` 不再被当成控制命令。
    check_numeric_fields(payload)?;

    match op {
        "exec" => {
            if let Some(script) = payload.get("script").and_then(Value::as_str) {
                if !crate::server::page_data::is_extension_json(script) {
                    // 形如控制帧、但没通过内部判定：直接给结构化错误，别让它一路
                    // 走到 daemon 才报「不认识的命令」。
                    if let Some(problem) = non_internal_extension_json(script) {
                        return Err(problem);
                    }
                    // `--no-preflight` / `ATB_JS_CHECK=warn|off` 只跳过配平检查；
                    // 上面的扩展控制帧判定仍然生效（那是语义安全，不是语法预检）。
                    if !js_check_skipped(payload) {
                        validate_js(script)?;
                    }
                }
            }
        }
        "inject" => {
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("run");
            if action == "run" || action == "persist" {
                if let Some(code) = payload.get("code").and_then(Value::as_str) {
                    if !js_check_skipped(payload) {
                        validate_js(code)?;
                    }
                }
                // `--cleanup-file` 的脚本同样会在页面里执行；语法错误应在本地暴露，
                // 不要等注入成功后清理阶段才抛 SyntaxError。
                if let Some(code) = payload.get("cleanup_code").and_then(Value::as_str) {
                    if !js_check_skipped(payload) {
                        validate_js(code)?;
                    }
                }
            }
        }
        "upload" => {
            // 与 click/fill 等动作保持同一套 target 预检：upload 的
            // `target` 是文件输入框选择器，写错了同样不该先打到 daemon。
            if let Some(target) = selector_field("target") {
                validate_selector(target)?;
            }
            // 只查绝对路径：daemon 用自己的 cwd 解析相对路径，CLI 侧结论可能不同。
            if let Some(files) = payload.get("files").and_then(Value::as_array) {
                for file in files.iter().filter_map(Value::as_str) {
                    if Path::new(file).is_absolute() {
                        validate_existing_file(file)?;
                    }
                }
            }
        }
        "click" | "mouse-click" | "fill" | "hover" | "select" | "check" | "action" => {
            if let Some(target) = selector_field("target") {
                validate_selector(target)?;
            }
        }
        "scroll" => {
            if let Some(target) = selector_field("target") {
                validate_selector(target)?;
            }
        }
        "screenshot" | "snapshot" | "read" | "find" | "wait" => {
            if let Some(selector) = selector_field("selector") {
                validate_selector(selector)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn exec_payload_accepts_marked_internal_command() {
        // 只有 daemon 内部构造（显式标记 + 白名单 cmd）才跳过 JS 预检。
        let payload = json!({
            "script": "{\"__atb_internal_cmd\":true,\"cmd\":\"status\"}"
        });
        assert!(preflight_payload("exec", &payload).is_ok());
    }

    #[test]
    fn exec_payload_rejects_unmarked_control_json() {
        // 用户脚本 `{"cmd":"status"}` 不再被当成控制帧：CLI 侧提前给结构化错误。
        let payload = json!({ "script": "{\"cmd\":\"status\"}" });
        let err = preflight_payload("exec", &payload).unwrap_err();
        assert_eq!(err.code, "invalid_json");
        assert_eq!(err.field.as_deref(), Some("script"));
        assert!(!err.hint.is_empty());

        // 标记存在但 cmd 不在白名单：同样拒绝。
        let payload = json!({
            "script": "{\"__atb_internal_cmd\":true,\"cmd\":\"rm-everything\"}"
        });
        assert_eq!(
            preflight_payload("exec", &payload).unwrap_err().code,
            "invalid_json"
        );
        // 普通 JS 对象字面量不受影响。
        let payload = json!({ "script": "return ({ ok: true })" });
        assert!(preflight_payload("exec", &payload).is_ok());
    }

    #[test]
    fn exec_payload_rejects_unbalanced_js() {
        let payload = json!({ "script": "if (a) { return 1" });
        assert_eq!(
            preflight_payload("exec", &payload).unwrap_err().code,
            "invalid_js"
        );
    }

    #[test]
    fn exec_payload_no_preflight_skips_balance_check() {
        // 逃生口只跳过 JS 配平检查，数值字段等其它预检仍然生效。
        let payload = json!({ "script": "if (a) { return 1", "no_preflight": true });
        assert!(preflight_payload("exec", &payload).is_ok());
        assert!(js_check_skipped(&payload));

        let bad_number = json!({
            "script": "if (a) { return 1",
            "no_preflight": true,
            "timeout": 1e300
        });
        assert_eq!(
            preflight_payload("exec", &bad_number).unwrap_err().code,
            "invalid_number"
        );
    }

    #[test]
    fn inject_payload_no_preflight_skips_balance_check() {
        let payload = json!({
            "action": "run",
            "code": "if (a) { return 1",
            "no_preflight": true
        });
        assert!(preflight_payload("inject", &payload).is_ok());
    }

    #[test]
    fn inject_payload_checks_cleanup_code() {
        let payload = json!({
            "action": "persist",
            "code": "return 1",
            "cleanup_code": "if (a) {"
        });
        assert_eq!(
            preflight_payload("inject", &payload).unwrap_err().code,
            "invalid_js"
        );
    }

    #[test]
    fn click_payload_validates_selector() {
        let ok = json!({ "target": "#submit" });
        assert!(preflight_payload("click", &ok).is_ok());
        let bad = json!({ "target": "input[name=q" });
        assert!(preflight_payload("click", &bad).is_err());
    }

    #[test]
    fn snapshot_ref_is_not_a_css_error() {
        let payload = json!({ "target": "@e3" });
        assert!(preflight_payload("click", &payload).is_ok());
    }

    #[test]
    fn nan_timeout_is_rejected_instead_of_null() {
        // `json!` 把 NaN 序列化成 null，daemon 侧会读成 None 并静默用默认值。
        let payload = json!({ "script": "return 1", "timeout": f64::NAN });
        assert!(payload["timeout"].is_null());
        let err = preflight_payload("exec", &payload).unwrap_err();
        assert_eq!(err.code, "invalid_number");
        assert_eq!(err.field.as_deref(), Some("timeout"));
    }

    #[test]
    fn huge_timeout_is_rejected_before_duration_panic() {
        // 1e300 能通过 `is_finite && > 0`，随后 `Duration::from_secs_f64` 会 panic。
        let payload = json!({ "target": "#a", "timeout": 1e300 });
        let err = preflight_payload("click", &payload).unwrap_err();
        assert_eq!(err.code, "invalid_number");
    }

    #[test]
    fn numeric_fields_inside_params_are_checked() {
        let payload = json!({ "target": "#a", "params": { "x": -5.0 } });
        assert_eq!(
            preflight_payload("mouse-click", &payload).unwrap_err().code,
            "invalid_number"
        );
    }

    #[test]
    fn numeric_fields_inside_steps_are_checked() {
        // batch / run 的每一步参数都在 steps[*].args 里，只查顶层会整条漏掉。
        let payload = json!({
            "steps": [
                { "op": "read", "args": { "timeout": 1e300 } }
            ]
        });
        assert_eq!(
            preflight_payload("batch", &payload).unwrap_err().code,
            "invalid_number"
        );
        let nan_challenge = json!({
            "steps": [
                { "op": "open", "args": { "challenge_timeout": f64::NAN } }
            ]
        });
        assert_eq!(
            preflight_payload("batch", &nan_challenge).unwrap_err().code,
            "invalid_number"
        );
        let bad_confirm = json!({ "steps": [{ "op": "click", "args": { "confirm_ms": -1.0 } }] });
        assert_eq!(
            preflight_payload("batch", &bad_confirm).unwrap_err().code,
            "invalid_number"
        );
    }

    #[test]
    fn numeric_count_fields_accept_normal_values() {
        // 上限定太紧会把合法命令挡在门外：常见取值必须通过。
        let payload = json!({
            "limit": 5000,
            "max_nodes": 2000,
            "max_chars": 100_000,
            "max_bytes": 2_000_000
        });
        assert!(preflight_payload("read", &payload).is_ok());
        let big = json!({ "max_bytes": 1e300 });
        assert_eq!(
            preflight_payload("read", &big).unwrap_err().code,
            "invalid_number"
        );
    }

    #[test]
    fn pdf_scale_out_of_range_is_rejected() {
        let payload = json!({ "scale": 99.0, "timeout": 30.0 });
        assert_eq!(
            preflight_payload("save-pdf", &payload).unwrap_err().code,
            "invalid_number"
        );
    }

    #[test]
    fn upload_rejects_missing_absolute_file() {
        let missing = if cfg!(windows) {
            "C:/atb-definitely-missing-file.txt"
        } else {
            "/atb-definitely-missing-file.txt"
        };
        let payload = json!({ "target": "#file", "files": [missing] });
        assert_eq!(
            preflight_payload("upload", &payload).unwrap_err().code,
            "file_not_found"
        );
        // 相对路径交给 daemon 解析，CLI 侧不预判。
        let relative = json!({ "target": "#file", "files": ["./missing.txt"] });
        assert!(preflight_payload("upload", &relative).is_ok());
    }

    #[test]
    fn upload_validates_target_selector() {
        // upload 与 click/fill 一样会对 target 做选择器预检。
        let bad = json!({ "target": "input[name=\"q\"", "files": [] });
        assert_eq!(
            preflight_payload("upload", &bad).unwrap_err().code,
            "invalid_selector"
        );
        let ok = json!({ "target": "input[type=file]", "files": [] });
        assert!(preflight_payload("upload", &ok).is_ok());
    }
}
