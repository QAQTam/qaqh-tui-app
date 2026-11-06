//! DynamicValue 求值：`@path` 绑定 / `@call` 白名单函数 / truthy。
//!
//! 对位移植桌面端 `genui/v2/eval2.ts` + `SurfaceView2` 的 display/truthy 语义：
//! - `@path` 走 JSON Pointer（`/s/<key>` 状态域约定）；空路径 = 模板当前项；
//! - `@call` 只认白名单（`qaqh:arith.*` / `qaqh:logic.*`），白名单外必须拒绝——
//!   这是「UI 源码按不可信文本处理」的执行面闸门；
//! - `+` 在任一侧非数字时降级为字符串拼接（与 JS `+` 一致）。

use serde_json::{Map, Value};

use super::wire::Dyn;

/// 模板作用域：repeat 展开时的循环下标与当前项。
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub index: usize,
    pub item: Value,
}

pub fn resolve(dynv: &Dyn, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    match dynv {
        Dyn::Lit(v) => Ok(v.clone()),
        Dyn::Path(p) => resolve_path(p, dm, scope),
        Dyn::Call { name, args } => call_fn(name, args, dm, scope),
    }
}

pub fn resolve_value(v: &Value, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    resolve(&super::wire::to_dyn(v), dm, scope)
}

fn resolve_path(p: &str, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    if p.is_empty() {
        // 模板当前项（translate 的 item 表达式 → 相对空路径）。
        return scope
            .map(|s| s.item.clone())
            .ok_or_else(|| "空路径绑定但无模板作用域".to_string());
    }
    if p == "/@index" {
        return Ok(scope.map(|s| Value::from(s.index as u64)).unwrap_or(Value::Null));
    }
    let ptr = if p == "/" { "" } else { p };
    Ok(dm.pointer(ptr).cloned().unwrap_or(Value::Null))
}

fn call_fn(
    name: &str,
    args: &Map<String, Value>,
    dm: &Value,
    scope: Option<&Scope>,
) -> Result<Value, String> {
    let arg = |k: &str| -> Result<Value, String> {
        match args.get(k) {
            Some(v) => resolve_value(v, dm, scope),
            None => Err(format!("函数 {name} 缺参数 {k}")),
        }
    };
    match name {
        "qaqh:arith.add" => {
            let (l, r) = (arg("left")?, arg("right")?);
            // 整数进整数出（避免 1.0 显示漂移）；任一侧非数字 → 字符串拼接。
            match (as_int(&l), as_int(&r)) {
                (Some(a), Some(b)) => Ok(Value::from(a + b)),
                _ if l.is_number() && r.is_number() => {
                    Ok(num_value(l.as_f64().unwrap_or(0.0) + r.as_f64().unwrap_or(0.0)))
                }
                _ => Ok(Value::String(format!("{}{}", display(&l), display(&r)))),
            }
        }
        "qaqh:arith.sub" | "qaqh:arith.mul" | "qaqh:arith.div" => {
            let (l, r) = (arg("left")?, arg("right")?);
            let op = name.trim_start_matches("qaqh:arith.");
            if let (Some(a), Some(b)) = (as_int(&l), as_int(&r)) {
                match op {
                    "sub" => Ok(Value::from(a - b)),
                    "mul" => Ok(Value::from(a * b)),
                    _ => {
                        if b == 0 {
                            return Err("qaqh:arith.div 除零".into());
                        }
                        Ok(Value::from(a / b))
                    }
                }
            } else {
                let (Some(a), Some(b)) = (l.as_f64(), r.as_f64()) else {
                    return Err(format!("{name} 需要数字参数"));
                };
                Ok(num_value(match op {
                    "sub" => a - b,
                    "mul" => a * b,
                    _ => {
                        if b == 0.0 {
                            return Err("qaqh:arith.div 除零".into());
                        }
                        a / b
                    }
                }))
            }
        }
        "qaqh:logic.eq" => Ok(Value::Bool(arg("left")? == arg("right")?)),
        "qaqh:logic.neq" => Ok(Value::Bool(arg("left")? != arg("right")?)),
        "qaqh:logic.gt" => Ok(Value::Bool(
            num_cmp(&arg("left")?, &arg("right")?) == Some(std::cmp::Ordering::Greater),
        )),
        "qaqh:logic.lt" => Ok(Value::Bool(
            num_cmp(&arg("left")?, &arg("right")?) == Some(std::cmp::Ordering::Less),
        )),
        "qaqh:logic.and" => Ok(Value::Bool(truthy(&arg("left")?) && truthy(&arg("right")?))),
        "qaqh:logic.or" => Ok(Value::Bool(truthy(&arg("left")?) || truthy(&arg("right")?))),
        "qaqh:logic.not" => Ok(Value::Bool(!truthy(&arg("value")?))),
        other => Err(format!("白名单外函数 {other}")),
    }
}

/// JSON 数值：整数进整数出（对齐 JS `JSON.stringify(Number)` 的序列化形状）。
pub fn num_value(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9.007_199_254_740_991e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn as_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
}

fn num_cmp(l: &Value, r: &Value) -> Option<std::cmp::Ordering> {
    match (l.as_f64(), r.as_f64()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => None,
    }
}

/// JS 风格 truthy（桌面端 truthy 的近似）：null/false/0/空串为假，容器恒真。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// 值的展示形态（对齐桌面端 display：字符串原样，bool "true"，null 空）。
pub fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dm() -> Value {
        json!({"s": {"count": 2, "tags": ["alpha"], "note": ""}})
    }

    #[test]
    fn paths_calls_and_truthiness() {
        let dm = dm();
        let v = resolve_value(&json!({"@path": "/s/count"}), &dm, None).unwrap();
        assert_eq!(v, json!(2));
        let v = resolve_value(
            &json!({"@call": "qaqh:arith.add", "args": {"left": "进度：", "right": {"@path": "/s/count"}}}),
            &dm,
            None,
        )
        .unwrap();
        assert_eq!(v, json!("进度：2"));
        let v = resolve_value(
            &json!({"@call": "qaqh:logic.eq", "args": {"left": {"@path": "/s/note"}, "right": ""}}),
            &dm,
            None,
        )
        .unwrap();
        assert_eq!(v, json!(true));
        assert!(truthy(&Value::String("x".into())));
        assert!(!truthy(&Value::String(String::new())));
        assert!(!truthy(&Value::Null));
    }

    #[test]
    fn integer_arith_stays_integer() {
        let dm = dm();
        let v = resolve_value(
            &json!({"@call": "qaqh:arith.add", "args": {"left": {"@path": "/s/count"}, "right": 1}}),
            &dm,
            None,
        )
        .unwrap();
        assert_eq!(v, json!(3));
        assert!(v.is_i64(), "整数运算不得漂移成浮点");
    }

    #[test]
    fn template_scope_resolves_empty_path_and_index() {
        let dm = dm();
        let scope = Scope { index: 3, item: json!("beta") };
        let v = resolve_value(&json!({"@path": ""}), &dm, Some(&scope)).unwrap();
        assert_eq!(v, json!("beta"));
        let v = resolve_value(&json!({"@path": "/@index"}), &dm, Some(&scope)).unwrap();
        assert_eq!(v, json!(3));
    }

    #[test]
    fn whitelist_rejects_unknown_functions() {
        let dm = dm();
        assert!(
            resolve_value(&json!({"@call": "window.alert"}), &dm, None).is_err(),
            "白名单外必须拒绝"
        );
    }
}
