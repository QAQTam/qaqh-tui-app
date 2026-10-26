//! TSX 风格受限 DSL 解析器（对位移植桌面端 `compiler/parse.ts`）。
//!
//! 语义是「解析而非执行」：`ui.*` 是编译 intrinsics，降低到 IR，绝不调用任何
//! JavaScript 函数。不支持 import/函数/任意表达式；未知语句、未知组件、未知
//! 状态引用一律具名报错（带行号），不修复半截代码。
//!
//! 语法只有 6 种顶层语句 + 受限 JSX 自闭合元素，单趟逐字符扫描足够。扫描按
//! 字节进行、只匹配 ASCII 特符，非 ASCII 字节（CJK 等）一律视为内容跳过，
//! 保证所有 `&str[..]` 切片都落在字符边界上。正则全部提升为 LazyLock 静态量
//! ——流式期间解析器每帧重入，循环内 Regex::new 是白白的重复编译。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::ir::{
    Action, BinOp, CATALOG, Expr, GenUiError, GenUiResult, IrNode, PropValue, StateDecl,
    StateType, SurfaceIr, ui_err,
};
use super::unescape;

pub const MAX_NODES: usize = 512;

// ── 语句/调用形状 ────────────────────────────────────────────────────────────
static RE_SURFACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ui\.surface\(").unwrap());
static RE_NODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ui\.node\(").unwrap());
static RE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ui\.end\(\s*\)$").unwrap());
static RE_STATE_STMT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^const\s+([A-Za-z_$][\w$]*)\s*=\s*ui\.state\("#).unwrap());
static RE_DERIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^const\s+([A-Za-z_$][\w$]*)\s*=\s*ui\.derive\(([\s\S]+)\)$"#).unwrap());
static RE_ACTION_STMT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ui\.(set|reset|followUp|copy|openLink|append|remove)\(").unwrap());
static RE_FOR_PRESCAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"ui\.for\(\s*"(?:[^"\\]|\\.)*"\s*,\s*"([^"\\]+)"\s*\)"#).unwrap());
// ── 字面量/标识符 ────────────────────────────────────────────────────────────
static RE_STR_DQ: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^"((?:[^"\\]|\\.)*)"$"#).unwrap());
static RE_STR_SQ: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^'((?:[^'\\]|\\.)*)'$"#).unwrap());
static RE_STR_WHOLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^"(?:[^"\\]|\\.)*"$"#).unwrap());
static RE_STR_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^"((?:[^"\\]|\\.)*)""#).unwrap());
static RE_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[+-]?\d+(?:\.\d+)?$").unwrap());
static RE_IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_$][\w$]*$").unwrap());
static RE_LIST_ARR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\[\s*(?:"(?:[^"\\]|\\.)*"\s*,?\s*)*\]$"#).unwrap());
static RE_OBJ_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:"((?:[^"\\]|\\.)*)"|([A-Za-z_$][\w$]*))\s*:\s*([\s\S]+)$"#).unwrap());
// ── 动作 ─────────────────────────────────────────────────────────────────────
static RE_UI_RESET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ui\.reset\(\s*\)$").unwrap());
static RE_UI_SET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.set\(\s*("(?:[^"\\]|\\.)*")\s*,\s*([\s\S]+)\s*\)$"#).unwrap());
static RE_UI_FOLLOW_UP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.followUp\(\s*("(?:[^"\\]|\\.)*")\s*(?:,\s*(\{[\s\S]*\})\s*)?\)$"#).unwrap());
static RE_UI_COPY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ui\.copy\(\s*([\s\S]+)\s*\)$").unwrap());
static RE_UI_OPEN_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.openLink\(\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap());
static RE_UI_KEYED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.(append|remove)\(\s*("(?:[^"\\]|\\.)*")\s*,\s*([\s\S]+)\s*\)$"#).unwrap());
// ── JSX ──────────────────────────────────────────────────────────────────────
static RE_JSX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<\s*([A-Za-z][\w]*)\s*([\s\S]*)/>\s*$").unwrap());
static RE_PROP_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^([A-Za-z_$][\w$]*)\s*=\s*"#).unwrap());
static RE_UI_BIND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.bind\(\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap());
static RE_UI_FOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^ui\.for\(\s*("(?:[^"\\]|\\.)*")\s*,\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap());
static RE_ACTION_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ui\.(?:set|reset|followUp|copy|openLink|append|remove)\(").unwrap());

type Statement = (String, usize); // (text, line)

/// 按「深度 0 的分号」切语句；跳过字符串与注释，记录起始行号。
/// 流式容尾（`tolerant_tail`，对应 final=false）：结尾处未终结的语句整体丢弃，
/// 已到齐的语句照常返回——半截 JSX/括号不得拖累前面的节点先渲染。
pub fn split_statements(source: &str, tolerant_tail: bool) -> Vec<Statement> {
    let mut statements: Vec<Statement> = vec![];
    let bytes = source.as_bytes();
    let mut start: Option<usize> = None;
    let mut start_line = 1usize;
    let mut line = 1usize;
    let mut depth = 0i32;
    let mut i = 0usize;
    macro_rules! push_stmt {
        ($end:expr) => {{
            if let Some(s) = start {
                let text = source[s..$end].trim();
                if !text.is_empty() {
                    statements.push((text.to_string(), start_line));
                }
            }
        }};
    }
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i + 1 >= bytes.len() && tolerant_tail {
                return statements; // 未闭合注释 = 半截
            }
            i += 2;
            continue;
        }
        // 非 ASCII 字节（CJK 等）只出现在字符串/注释内部或非法内容里：
        // 逐字节跳过，保证后续 source[i..] 切片都在字符边界上。
        if c >= 0x80 {
            i += 1;
            continue;
        }
        if start.is_none() && c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if start.is_none() {
            start = Some(i);
            start_line = line;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i >= bytes.len() && tolerant_tail {
                return statements; // 未闭合字符串 = 尾部半截语句
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        }
        if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        }
        if c == b';' && depth <= 0 {
            push_stmt!(i);
            start = None;
            i += 1;
            continue;
        }
        // 宽容换行（模型省略分号）：深度 0、当前位置是新语句关键字起始
        // （ui./const，且前一个非空白字符是换行），且上文有可终止语句的内容。
        if depth <= 0
            && start.is_some()
            && (source[i..].starts_with("ui.") || source[i..].starts_with("const "))
            && previous_non_space_is_newline(source, i)
            && ends_statement_before(source, start.unwrap(), i)
        {
            push_stmt!(i);
            start = Some(i);
            start_line = line;
            continue;
        }
        i += 1;
    }
    if tolerant_tail
        && let Some(s) = start
        && (depth > 0 || !statements_closed_tail(source, s))
    {
        return statements;
    }
    push_stmt!(source.len());
    statements
}

/// 容尾判定：statementStart 之后（到源码结尾）是否已构成「闭合」的语句形态
/// （出现 `)`，且深度已归零）。宽容换行能收尾的语句在扫描中早已 push，
/// 这里只处理「结尾悬空」的形态：语句完整（如省略分号的 ui.end()）返回 true。
fn statements_closed_tail(source: &str, start: usize) -> bool {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        }
        if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        }
        i += 1;
    }
    let tail = source[start..].trim_end();
    depth <= 0
        && (tail.ends_with(')') || tail.ends_with('}') || tail.ends_with('"') || tail.ends_with('\''))
}

/// index 前最近的非空白字符是否是换行。
fn previous_non_space_is_newline(source: &str, index: usize) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = index as isize - 1;
    while cursor >= 0
        && (bytes[cursor as usize] == b' '
            || bytes[cursor as usize] == b'\t'
            || bytes[cursor as usize] == b'\r')
    {
        cursor -= 1;
    }
    cursor >= 0 && bytes[cursor as usize] == b'\n'
}

/// 从 statementStart 到 index 之间是否存在可终止语句的内容（保守判定）。
fn ends_statement_before(source: &str, start: usize, index: usize) -> bool {
    let head = &source[start..index];
    (head.contains(')') || head.contains('"')) && !head.trim().is_empty()
}

fn parse_string_literal(text: &str, line: usize) -> GenUiResult<String> {
    let trimmed = text.trim();
    if let Some(c) = RE_STR_DQ.captures(trimmed) {
        return Ok(unescape(c.get(1).unwrap().as_str()));
    }
    if let Some(c) = RE_STR_SQ.captures(trimmed) {
        return Ok(unescape(c.get(1).unwrap().as_str()));
    }
    Err(ui_err(line, format!("需要字符串字面量:{text}")))
}

/// 顶层逗号切分（忽略括号/字符串内）。
fn split_top(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut parts: Vec<String> = vec![];
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        } else if c == b',' && depth <= 0 {
            parts.push(text[start..i].trim().to_string());
            start = i + 1;
        }
        i += 1;
    }
    if start <= text.len() {
        parts.push(text[start..].trim().to_string());
    }
    if parts.len() == 1 && parts[0].is_empty() {
        parts.pop();
    }
    parts
}

/// 取调用括号内的参数（要求语句以 prefix 开头、以 ) 收尾）。
fn call_args(statement: &Statement, prefix: &Regex) -> GenUiResult<Vec<String>> {
    let (text, line) = statement;
    if !prefix.is_match(text) {
        return Err(ui_err(*line, format!("无法识别的语句:{text}")));
    }
    let open = text.find('(').ok_or_else(|| ui_err(*line, "缺左括号"))?;
    let inner = &text[open + 1..text.rfind(')').ok_or_else(|| ui_err(*line, "缺右括号"))?];
    Ok(split_top(inner))
}

#[derive(Debug, Clone, Default)]
struct Symbol {
    key: String,
    item: bool,
    derived: bool,
    expr: Option<Expr>,
}

type Symbols = BTreeMap<String, Symbol>;

fn has_state_key(symbols: &Symbols, key: &str) -> bool {
    symbols.values().any(|s| s.key == key && !s.item)
}

/// 表达式优先级级联：or → and → 比较 → 加减/拼接 → 乘除 → 一元 ! → 原子。
fn parse_expression(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    parse_binary(text, symbols, line, 0)
}

/// 各级二元操作符：文本形式（tsx 符号）。
const BINARY_LEVELS: [&[&str]; 5] = [
    &["||"],
    &["&&"],
    &["==", "!=", ">=", "<=", ">", "<"],
    &["+", "-"],
    &["*", "/"],
];

/// 操作符文本 → Expr 二元 op。同时收 tsx 符号形式（`||`）与 jsonl wire 名
/// （`or`）——jsonl 载体的 wire_expr 复用本函数。
pub(super) fn op_kind(op: &str) -> BinOp {
    match op {
        "||" => BinOp::Or,
        "&&" => BinOp::And,
        "==" => BinOp::Eq,
        "!=" => BinOp::Neq,
        ">=" | ">" => BinOp::Gt,
        "<=" | "<" => BinOp::Lt,
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "add" => BinOp::Add,
        "sub" => BinOp::Sub,
        "mul" => BinOp::Mul,
        "div" => BinOp::Div,
        "eq" => BinOp::Eq,
        "neq" => BinOp::Neq,
        "gt" => BinOp::Gt,
        "lt" => BinOp::Lt,
        "and" => BinOp::And,
        "or" => BinOp::Or,
        _ => unreachable!(),
    }
}

fn parse_binary(text: &str, symbols: &Symbols, line: usize, level: usize) -> GenUiResult<Expr> {
    if level >= BINARY_LEVELS.len() {
        return parse_unary(text, symbols, line);
    }
    let trimmed = text.trim();
    if let Some((index, op)) = find_top(trimmed, BINARY_LEVELS[level]) {
        let left = parse_binary(&trimmed[..index], symbols, line, level)?;
        let right = parse_binary(&trimmed[index + op.len()..], symbols, line, level)?;
        return Ok(Expr::Bin(op_kind(op), Box::new(left), Box::new(right)));
    }
    parse_binary(trimmed, symbols, line, level + 1)
}

/// 深度 0、字符串外、最左出现的操作符（左结合）；多字符 op 按 ops 顺序优先。
fn find_top<'a>(text: &'a str, ops: &[&'a str]) -> Option<(usize, &'a str)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c >= 0x80 {
            i += 1;
            continue; // 非 ASCII 只在字符串内（已整体跳过）或非法内容
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' {
            depth += 1;
        } else if c == b')' || c == b']' {
            depth -= 1;
        } else if depth == 0 && i > 0 {
            for op in ops {
                if text[i..].starts_with(op) && is_operator_position(text, i, op) {
                    return Some((i, *op));
                }
            }
        }
        i += 1;
    }
    None
}

/// 操作符两侧（越过空白）必须是操作数边界；单字符 op 遇双字符前缀时让位。
fn is_operator_position(text: &str, index: usize, op: &str) -> bool {
    let bytes = text.as_bytes();
    let mut left = index as isize - 1;
    while left >= 0 && (bytes[left as usize] as char).is_ascii_whitespace() {
        left -= 1;
    }
    let mut right = index + op.len();
    while right < bytes.len() && (bytes[right] as char).is_ascii_whitespace() {
        right += 1;
    }
    if left < 0 || right >= bytes.len() {
        return false;
    }
    // 双字符 op 贪婪：> 单独出现时，若紧跟 = 则由 >= 级联处理。
    let two = &bytes[index..(index + 2).min(bytes.len())];
    let two_char =
        two == b"==" || two == b"!=" || two == b">=" || two == b"<=" || two == b"&&" || two == b"||";
    if two_char && op.len() == 1 {
        return false;
    }
    let l = bytes[left as usize];
    let r = bytes[right];
    (is_ident_char(l) || l == b')' || l == b'"')
        && (is_ident_char(r) || r == b'(' || r == b'!' || r == b'"' || r == b'-')
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn parse_unary(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    let trimmed = text.trim();
    if trimmed.starts_with('!') && !trimmed.starts_with("!=") {
        return Ok(Expr::Not(Box::new(parse_unary(&trimmed[1..], symbols, line)?)));
    }
    parse_primary(trimmed, symbols, line)
}

fn parse_primary(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    let mut trimmed = text.trim();
    let mut stripped = false;
    // 括号复合表达式：整体被一对括号包住时剥掉，重新进入完整优先级级联。
    loop {
        if !trimmed.starts_with('(') {
            break;
        }
        let bytes = trimmed.as_bytes();
        let mut depth = 0i32;
        let mut close = None;
        for (i, c) in bytes.iter().enumerate() {
            if *c == b'(' {
                depth += 1;
            }
            if *c == b')' {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
        }
        match close {
            Some(i) if i == trimmed.len() - 1 => {
                trimmed = trimmed[1..trimmed.len() - 1].trim();
                if trimmed.is_empty() {
                    return Err(ui_err(line, "空括号表达式"));
                }
                stripped = true;
            }
            _ => break,
        }
    }
    if RE_STR_WHOLE.is_match(trimmed) {
        return Ok(Expr::Lit(Value::String(unescape(&trimmed[1..trimmed.len() - 1]))));
    }
    if RE_NUM.is_match(trimmed) {
        return Ok(Expr::Lit(json_number(trimmed.parse::<f64>().unwrap())));
    }
    if trimmed == "true" || trimmed == "false" {
        return Ok(Expr::Lit(Value::Bool(trimmed == "true")));
    }
    if RE_IDENT.is_match(trimmed) {
        let symbol = symbols
            .get(trimmed)
            .ok_or_else(|| ui_err(line, format!("表达式引用未知状态 {trimmed}")))?;
        if symbol.item {
            return Ok(Expr::Item(trimmed.to_string()));
        }
        if symbol.derived {
            return Ok(symbol.expr.clone().expect("derived 必带 expr"));
        }
        return Ok(Expr::State(symbol.key.clone()));
    }
    if stripped {
        return parse_binary(trimmed, symbols, line, 0);
    }
    Err(ui_err(line, format!("不支持的表达式:{trimmed}")))
}

/// JSON 数值：整数进整数出（对齐 JS `JSON.stringify(Number)` 的序列化形状，
/// 保证与桌面端 translate 产物逐字节可比）。
pub(super) fn json_number(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9.007_199_254_740_991e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn parse_action(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Action> {
    let trimmed = text.trim();
    if RE_UI_RESET.is_match(trimmed) {
        return Ok(Action::Reset);
    }
    if let Some(c) = RE_UI_SET.captures(trimmed) {
        let key = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        if !has_state_key(symbols, &key) {
            return Err(ui_err(line, format!("动作引用未知状态 {key}")));
        }
        return Ok(Action::Set {
            key,
            value: parse_expression(c.get(2).unwrap().as_str(), symbols, line)?,
        });
    }
    // ui.followUp("prompt") / ui.followUp("prompt", { "标签": 字段名, ... })：
    // 字段值在提交事务里从绑定状态读取，不上传其余私有状态（spec §7.6）。
    if let Some(c) = RE_UI_FOLLOW_UP.captures(trimmed) {
        let prompt = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        let keys: Vec<&String> = symbols.values().map(|s| &s.key).collect();
        let mut fields: Vec<(String, String)> = vec![];
        for (label, raw) in object_entries(c.get(2).map(|m| m.as_str()).unwrap_or("{}"), line)? {
            let name = raw.trim();
            // 字段值两种写法都收：状态名（标识符）或状态 key 字符串。
            let key = if RE_IDENT.is_match(name) {
                symbols.get(name).map(|s| s.key.clone())
            } else if RE_STR_WHOLE.is_match(name) {
                Some(unescape(&name[1..name.len() - 1]))
            } else {
                None
            };
            let Some(key) = key else {
                return Err(ui_err(
                    line,
                    format!("followUp 字段 {label} 引用未知状态 {name}（字段值应为状态名或其 \"key\"）"),
                ));
            };
            if !keys.contains(&&key) {
                return Err(ui_err(line, format!("followUp 字段 {label} 引用未知状态 {name}")));
            }
            fields.push((label, key));
        }
        return Ok(Action::FollowUp { prompt, fields });
    }
    // 低风险外围动作：copy（字符串表达式）/ openLink（仅 http(s) 字面量）。
    if let Some(c) = RE_UI_COPY.captures(trimmed) {
        return Ok(Action::Copy {
            value: parse_expression(c.get(1).unwrap().as_str(), symbols, line)?,
        });
    }
    if let Some(c) = RE_UI_OPEN_LINK.captures(trimmed) {
        let url = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ui_err(line, format!("openLink 只允许 http(s) 链接:{url}")));
        }
        return Ok(Action::OpenLink(url));
    }
    // append/remove 只作用于 list 态（类型校验在 validate_surface）。
    if let Some(c) = RE_UI_KEYED.captures(trimmed) {
        let key = parse_string_literal(c.get(2).unwrap().as_str(), line)?;
        if !has_state_key(symbols, &key) {
            return Err(ui_err(line, format!("动作引用未知状态 {key}")));
        }
        let value = parse_expression(c.get(3).unwrap().as_str(), symbols, line)?;
        return Ok(if c.get(1).unwrap().as_str() == "append" {
            Action::Append { key, value }
        } else {
            Action::Remove { key, value }
        });
    }
    Err(ui_err(line, format!("不支持的动作:{trimmed}")))
}

/// 对象字面量顶层键值切分；键允许 IDENT 或带引号字符串（followUp 标签可中文）。
fn object_entries(text: &str, line: usize) -> GenUiResult<Vec<(String, String)>> {
    let stripped = text.trim().trim_start_matches('{').trim_end_matches('}');
    let mut entries = vec![];
    for part in split_top(stripped) {
        let Some(c) = RE_OBJ_KEY.captures(part.trim()) else {
            return Err(ui_err(line, format!("无法解析选项:{part}")));
        };
        let key = c.get(1).or(c.get(2)).unwrap().as_str().to_string();
        entries.push((key, c.get(3).unwrap().as_str().to_string()));
    }
    Ok(entries)
}

/// JSX 自闭合元素：`<Type name={expr} name="str" ... />`；不支持 children 语法
/// 与 spread。属性值：`{...}` 平衡花括号取值（字符串字面量整体跳过，否则
/// `{"进度："}` 里的引号会让花括号配对提前闭合），或裸字符串字面量。
fn parse_jsx_node(
    text: &str,
    symbols: &Symbols,
    line: usize,
) -> GenUiResult<(String, BTreeMap<String, PropValue>)> {
    let Some(c) = RE_JSX.captures(text.trim()) else {
        return Err(ui_err(line, format!("只支持自闭合 JSX 元素:{text}")));
    };
    let type_ = c.get(1).unwrap().as_str().to_string();
    let rest = c.get(2).map(|m| m.as_str()).unwrap_or("");
    let mut props: BTreeMap<String, PropValue> = BTreeMap::new();
    let bytes = rest.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !rest.is_char_boundary(index) {
            index += 1;
            continue;
        }
        while index < bytes.len() && (bytes[index] as char).is_ascii_whitespace() {
            index += 1;
        }
        let Some(name_match) = RE_PROP_NAME.captures(&rest[index..]) else {
            if rest[index..].trim().is_empty() {
                break;
            }
            return Err(ui_err(
                line,
                format!("无法解析属性:{}", &rest[index..(index + 30).min(rest.len())]),
            ));
        };
        let name = name_match.get(1).unwrap().as_str().to_string();
        index += name_match.get(0).unwrap().len();
        if !rest[index..].starts_with('{') {
            if let Some(sv) = RE_STR_PREFIX.captures(&rest[index..]) {
                props
                    .insert(name, PropValue::Lit(Value::String(unescape(sv.get(1).unwrap().as_str()))));
                index += sv.get(0).unwrap().len();
                continue;
            }
            return Err(ui_err(
                line,
                format!("属性值需要 {{expr}} 或字符串:{}", &rest[index..(index + 30).min(rest.len())]),
            ));
        }
        // 平衡花括号取值；扫描从 '{' 本身开始（外层计入深度，与 TS 一致）。
        let mut depth = 0i32;
        let brace_start = index;
        while index < bytes.len() {
            let ch = bytes[index];
            if ch == b'"' || ch == b'\'' || ch == b'`' {
                let quote = ch;
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    index += 1;
                }
                index += 1;
                continue;
            }
            if ch == b'{' {
                depth += 1;
            }
            if ch == b'}' {
                depth -= 1;
                if depth == 0 {
                    index += 1;
                    break;
                }
            }
            index += 1;
        }
        if depth != 0 {
            return Err(ui_err(line, format!("属性值花括号不闭合:{name}")));
        }
        let raw = rest[brace_start + 1..index - 1].trim();
        if let Some(b) = RE_UI_BIND.captures(raw) {
            // 双向绑定：值的读写都落到该状态域（spec §10）。
            props.insert(name, PropValue::Binding(parse_string_literal(b.get(1).unwrap().as_str(), line)?));
        } else if name == "repeat" {
            // for/repeat：repeat={ui.for("list态", "循环变量")}（spec §7 for 语句的属性形态）。
            let Some(f) = RE_UI_FOR.captures(raw) else {
                return Err(ui_err(line, "repeat 需要 ui.for(\"list态\", \"循环变量\")"));
            };
            let source = parse_string_literal(f.get(1).unwrap().as_str(), line)?;
            let as_ = parse_string_literal(f.get(2).unwrap().as_str(), line)?;
            props.insert(name, PropValue::Repeat(source, as_));
        } else if RE_ACTION_PREFIX.is_match(raw) {
            // 事件值是 Action 描述，不是表达式（spec §7.1）。
            props.insert(name, PropValue::Action(parse_action(raw, symbols, line)?));
        } else if RE_LIST_ARR.is_match(raw) {
            let items: Vec<String> = if raw.len() > 2 {
                raw[1..raw.len() - 1]
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| parse_string_literal(s.trim(), line))
                    .collect::<GenUiResult<_>>()?
            } else {
                vec![]
            };
            props.insert(name, PropValue::List(items));
        } else {
            props.insert(name, PropValue::Expr(parse_expression(raw, symbols, line)?));
        }
    }
    Ok((type_, props))
}

/// 解析整个 surface 源码为 IR。结构裁决（spec §6.1/§7.1）：ui.surface 必须第一条、
/// ui.end 必须最后一条，中间只允许 ui.state/ui.derive 声明与 ui.node；
/// ui.set/ui.reset 只能作为事件值出现。
pub fn parse_surface(source: &str, final_: bool) -> GenUiResult<SurfaceIr> {
    let statements = split_statements(source, !final_);
    if statements.is_empty() {
        return Err(GenUiError { message: "UI 源码为空".into(), line: None });
    }
    let mut symbols: Symbols = BTreeMap::new();
    let mut states: BTreeMap<String, StateDecl> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut nodes: BTreeMap<String, IrNode> = BTreeMap::new();
    let mut surface_id: Option<String> = None;
    let mut title = None;
    let mut fallback = None;
    let mut root_id: Option<String> = None;
    let mut ended = false;

    // 预扫描 ui.for：循环变量先注册为 item 伪符号，表达式里的标识符才能解析。
    for cap in RE_FOR_PRESCAN.captures_iter(source) {
        let var_name = cap.get(1).unwrap().as_str().to_string();
        symbols
            .entry(var_name.clone())
            .or_insert_with(|| Symbol { key: var_name.clone(), item: true, ..Default::default() });
    }

    for (index, statement) in statements.iter().enumerate() {
        let (text, line) = statement;
        if ended {
            return Err(ui_err(*line, "ui.end() 之后不允许再有语句"));
        }
        if index == 0 {
            if !text.starts_with("ui.surface(") {
                return Err(ui_err(*line, "第一条语句必须是 ui.surface(...)"));
            }
            let args = call_args(statement, &RE_SURFACE)?;
            surface_id =
                Some(parse_string_literal(args.first().map(String::as_str).unwrap_or(""), *line)?);
            if let Some(opts) = args.get(1) {
                for (key, value) in object_entries(opts, *line)? {
                    match key.as_str() {
                        "root" => root_id = Some(parse_string_literal(&value, *line)?),
                        "title" => title = Some(parse_string_literal(&value, *line)?),
                        "fallback" => fallback = Some(parse_string_literal(&value, *line)?),
                        _ => return Err(ui_err(*line, format!("ui.surface 不支持选项 {key}"))),
                    }
                }
            }
            continue;
        }
        if let Some(sm) = RE_STATE_STMT.captures(text) {
            let symbol = sm.get(1).unwrap().as_str().to_string();
            let inner_start = text.find('(').unwrap() + 1;
            let inner = &text[inner_start..text.rfind(')').ok_or_else(|| ui_err(*line, "缺右括号"))?];
            let args = split_top(inner);
            let key = parse_string_literal(args.first().map(String::as_str).unwrap_or(""), *line)?;
            let type_text =
                parse_string_literal(args.get(1).map(String::as_str).unwrap_or(""), *line)?;
            let type_ = match type_text.as_str() {
                "string" => StateType::Str,
                "number" => StateType::Num,
                "integer" => StateType::Integer,
                "boolean" => StateType::Bool,
                "list" => StateType::List,
                _ => return Err(ui_err(*line, format!("不支持的状态类型 {type_text}"))),
            };
            if args.len() != 3 {
                return Err(ui_err(*line, "ui.state(key, type, initial) 需要 3 个参数"));
            }
            let initial_src = args[2].trim();
            let initial = match type_ {
                StateType::Str => Value::String(parse_string_literal(&args[2], *line)?),
                StateType::Bool => {
                    if initial_src == "true" {
                        Value::Bool(true)
                    } else if initial_src == "false" {
                        Value::Bool(false)
                    } else {
                        return Err(ui_err(*line, "boolean 初始值必须是 true/false"));
                    }
                }
                StateType::List => {
                    if !RE_LIST_ARR.is_match(initial_src) {
                        return Err(ui_err(*line, "list 初始值必须是字符串数组"));
                    }
                    let items: Vec<String> = if initial_src.len() > 2 {
                        initial_src[1..initial_src.len() - 1]
                            .split(',')
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| parse_string_literal(s.trim(), *line))
                            .collect::<GenUiResult<_>>()?
                    } else {
                        vec![]
                    };
                    Value::Array(items.into_iter().map(Value::String).collect())
                }
                StateType::Num | StateType::Integer => {
                    let n: f64 = initial_src
                        .parse()
                        .map_err(|_| ui_err(*line, "number 初始值必须是有限数"))?;
                    if !n.is_finite() {
                        return Err(ui_err(*line, "number 初始值必须是有限数"));
                    }
                    if type_ == StateType::Integer
                        && !(n.is_finite()
                            && n.fract() == 0.0
                            && (0.0..=9.007_199_254_740_991e15).contains(&n))
                    {
                        return Err(ui_err(*line, "integer 初始值必须是安全整数"));
                    }
                    json_number(n)
                }
            };
            if symbols.contains_key(&symbol) || states.contains_key(&key) {
                return Err(ui_err(*line, format!("重复的状态声明 {symbol}/{key}")));
            }
            symbols.insert(symbol, Symbol { key: key.clone(), ..Default::default() });
            states.insert(key, StateDecl { type_, initial });
            continue;
        }
        if text.starts_with("ui.node(") {
            let args = call_args(statement, &RE_NODE)?;
            if args.len() != 2 {
                return Err(ui_err(*line, "ui.node(id, <Component ... />) 需要 2 个参数"));
            }
            let id = parse_string_literal(&args[0], *line)?;
            let (type_, props) = parse_jsx_node(&args[1], &symbols, *line)?;
            if !CATALOG.contains(&type_.as_str()) {
                return Err(ui_err(*line, format!("未知组件类型:{type_}")));
            }
            if nodes.contains_key(&id) || order.contains(&id) {
                return Err(ui_err(*line, format!("重复的节点 id {id}")));
            }
            let mut repeat = None;
            let mut props = props;
            if let Some(PropValue::Repeat(source, as_)) = props.remove("repeat") {
                repeat = Some((source, as_));
            }
            nodes.insert(id.clone(), IrNode { id: id.clone(), type_, props, line: *line, repeat });
            order.push(id);
            continue;
        }
        if let Some(dm) = RE_DERIVE.captures(text) {
            // 派生态：编译期按声明顺序内联进引用它的表达式（只出不进，天然无环）。
            let symbol = dm.get(1).unwrap().as_str().to_string();
            if symbols.contains_key(&symbol) {
                return Err(ui_err(*line, format!("重复的声明 {symbol}")));
            }
            let expr = parse_expression(dm.get(2).unwrap().as_str(), &symbols, *line)?;
            symbols.insert(
                symbol.clone(),
                Symbol { key: symbol, derived: true, expr: Some(expr), item: false },
            );
            continue;
        }
        if text.starts_with("ui.end(") {
            if !RE_END.is_match(text) {
                return Err(ui_err(*line, "ui.end() 不接受参数"));
            }
            ended = true;
            continue;
        }
        if RE_ACTION_STMT.is_match(text) {
            return Err(ui_err(*line, "动作只能作为事件值（如 onPress）出现"));
        }
        return Err(ui_err(*line, format!("无法识别的语句:{text}")));
    }

    let Some(surface_id) = surface_id else {
        return Err(GenUiError { message: "缺少 ui.surface(...)".into(), line: None });
    };
    // final=false（流式部分编译）：允许缺 ui.end，先渲染已到齐的语句。
    if !ended && final_ {
        return Err(GenUiError { message: "缺少 ui.end()".into(), line: None });
    }
    if order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    Ok(SurfaceIr { surface_id, title, fallback, root_id, states, order, nodes, warnings: vec![] })
}
