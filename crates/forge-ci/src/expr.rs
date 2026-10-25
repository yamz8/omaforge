//! GitHub Actions expressions: `${{ github.ref == 'refs/heads/main' && success() }}`.
//!
//! Follows GitHub's rules: missing properties are `null`, `==` compares loosely
//! (converting to numbers when types differ) and ignores case for strings, and `&&` / `||`
//! return one of their operands rather than a boolean.

use anyhow::{bail, Result};
use serde_json::{Map, Value};

/// How the job (or the steps so far) is going, for `success()`, `failure()` and
/// `cancelled()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Success,
    Failure,
    Cancelled,
}

/// What an expression can see: contexts by name (`github`, `env`, `matrix`, ...) and the
/// status so far.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub values: Map<String, Value>,
    pub status: Status,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, value: Value) -> Self {
        self.values.insert(name.to_owned(), value);
        self
    }

    pub fn set(&mut self, name: &str, value: Value) {
        self.values.insert(name.to_owned(), value);
    }
}

/// Evaluate one expression (without the `${{ }}`).
pub fn eval(source: &str, ctx: &Context) -> Result<Value> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        ctx,
    };
    let value = parser.or()?;
    if parser.pos < parser.tokens.len() {
        bail!(
            "unexpected {} in expression `{source}`",
            parser.tokens[parser.pos].describe()
        );
    }
    Ok(value)
}

/// Replace each `${{ expr }}` in `text` with its value as a string.
pub fn interpolate(text: &str, ctx: &Context) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let end = find_close(after)
            .ok_or_else(|| anyhow::anyhow!("unclosed `${{{{` in `{}`", text.trim()))?;
        out.push_str(&to_string(&eval(&after[..end], ctx)?));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Evaluate a value that may be exactly one `${{ expr }}` (keeping its type, so
/// `${{ fromJSON('[1,2]') }}` is an array) or text with expressions in it (a string).
pub fn evaluate_value(text: &str, ctx: &Context) -> Result<Value> {
    let trimmed = text.trim();
    if let Some(inner) = trimmed
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
    {
        if find_close(inner).is_none() {
            return eval(inner, ctx);
        }
    }
    Ok(Value::String(interpolate(text, ctx)?))
}

/// Evaluate an `if:` condition. The `${{ }}` is optional, and a condition that doesn't
/// call a status function only applies while things are succeeding, as on GitHub.
pub fn condition(source: &str, ctx: &Context) -> Result<bool> {
    let trimmed = source.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .filter(|inner| find_close(inner).is_none())
        .unwrap_or(trimmed);
    if inner.trim().is_empty() {
        return Ok(ctx.status == Status::Success);
    }
    let value = eval(inner, ctx)?;
    let uses_status = tokenize(inner)?.windows(2).any(|pair| {
        matches!(
            (&pair[0], &pair[1]),
            (Token::Ident(name), Token::LParen)
                if ["success", "failure", "always", "cancelled"]
                    .iter()
                    .any(|f| name.eq_ignore_ascii_case(f))
        )
    });
    Ok(if uses_status {
        truthy(&value)
    } else {
        ctx.status == Status::Success && truthy(&value)
    })
}

/// Where the `}}` closing an expression is, skipping over string literals.
fn find_close(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => in_string = !in_string,
            b'}' if !in_string && bytes.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A value as it appears when interpolated into text.
pub fn to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_number(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                0.0
            } else if let Some(hex) = s.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64)
            } else {
                s.parse().unwrap_or(f64::NAN)
            }
        }
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// `==`: same-typed values compare directly (strings ignoring case), others as numbers.
pub fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.to_lowercase() == y.to_lowercase(),
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => a == b,
        _ => to_number(a) == to_number(b),
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        _ => to_number(a).partial_cmp(&to_number(b)),
    }
}

// ---- Tokens ----

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Star,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::Ident(name) => format!("`{name}`"),
            Token::Str(s) => format!("'{s}'"),
            Token::Num(n) => format_number(*n),
            other => format!("{other:?}"),
        }
    }
}

fn tokenize(source: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let two = |t| (t, 2);
        let (token, len) = match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
                continue;
            }
            '(' => (Token::LParen, 1),
            ')' => (Token::RParen, 1),
            '[' => (Token::LBracket, 1),
            ']' => (Token::RBracket, 1),
            ',' => (Token::Comma, 1),
            '*' => (Token::Star, 1),
            '.' if !next.is_some_and(|n| n.is_ascii_digit()) => (Token::Dot, 1),
            '!' if next == Some('=') => two(Token::Ne),
            '!' => (Token::Not, 1),
            '=' if next == Some('=') => two(Token::Eq),
            '<' if next == Some('=') => two(Token::Le),
            '<' => (Token::Lt, 1),
            '>' if next == Some('=') => two(Token::Ge),
            '>' => (Token::Gt, 1),
            '&' if next == Some('&') => two(Token::And),
            '|' if next == Some('|') => two(Token::Or),
            '\'' => {
                let mut s = String::new();
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None => bail!("unterminated string in expression `{source}`"),
                        Some('\'') if chars.get(j + 1) == Some(&'\'') => {
                            s.push('\'');
                            j += 2;
                        }
                        Some('\'') => break,
                        Some(&ch) => {
                            s.push(ch);
                            j += 1;
                        }
                    }
                }
                (Token::Str(s), j + 1 - i)
            }
            c if c.is_ascii_digit()
                || c == '.'
                || (c == '-' && next.is_some_and(|n| n.is_ascii_digit())) =>
            {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '.') {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                let n = if let Some(hex) = text.strip_prefix("0x") {
                    i64::from_str_radix(hex, 16).map(|n| n as f64).ok()
                } else {
                    text.parse().ok()
                };
                match n {
                    Some(n) => (Token::Num(n), j - i),
                    None => bail!("invalid number `{text}` in expression `{source}`"),
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i + 1;
                while j < chars.len()
                    && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-')
                {
                    j += 1;
                }
                (Token::Ident(chars[i..j].iter().collect()), j - i)
            }
            other => bail!("unexpected `{other}` in expression `{source}`"),
        };
        tokens.push(token);
        i += len;
    }
    Ok(tokens)
}

// ---- Parsing and evaluation ----

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    ctx: &'a Context,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token) -> Result<()> {
        if self.eat(token) {
            Ok(())
        } else {
            bail!(
                "expected {} but found {}",
                token.describe(),
                self.peek().map_or("the end".into(), Token::describe)
            )
        }
    }

    fn or(&mut self) -> Result<Value> {
        let mut left = self.and()?;
        while self.eat(&Token::Or) {
            let right = self.and()?;
            if !truthy(&left) {
                left = right;
            }
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Value> {
        let mut left = self.equality()?;
        while self.eat(&Token::And) {
            let right = self.equality()?;
            if truthy(&left) {
                left = right;
            }
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Value> {
        let mut left = self.comparison()?;
        loop {
            let negate = if self.eat(&Token::Eq) {
                false
            } else if self.eat(&Token::Ne) {
                true
            } else {
                return Ok(left);
            };
            let right = self.comparison()?;
            left = Value::Bool(loose_eq(&left, &right) != negate);
        }
    }

    fn comparison(&mut self) -> Result<Value> {
        use std::cmp::Ordering::*;
        let mut left = self.unary()?;
        loop {
            let accept: &[std::cmp::Ordering] = match self.peek() {
                Some(Token::Lt) => &[Less],
                Some(Token::Le) => &[Less, Equal],
                Some(Token::Gt) => &[Greater],
                Some(Token::Ge) => &[Greater, Equal],
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.unary()?;
            left = Value::Bool(compare(&left, &right).is_some_and(|o| accept.contains(&o)));
        }
    }

    fn unary(&mut self) -> Result<Value> {
        if self.eat(&Token::Not) {
            return Ok(Value::Bool(!truthy(&self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Value> {
        let mut value = self.primary()?;
        // After `.*`, later properties apply to each element.
        let mut filtered = false;
        loop {
            if self.eat(&Token::Dot) {
                match self.tokens.get(self.pos).cloned() {
                    Some(Token::Ident(name)) => {
                        self.pos += 1;
                        value = property(&value, &name, filtered);
                    }
                    Some(Token::Star) => {
                        self.pos += 1;
                        value = elements(&value, filtered);
                        filtered = true;
                    }
                    other => bail!(
                        "expected a property name after `.`, found {}",
                        other.map_or("the end".into(), |t| t.describe())
                    ),
                }
            } else if self.eat(&Token::LBracket) {
                if self.eat(&Token::Star) {
                    self.expect(&Token::RBracket)?;
                    value = elements(&value, filtered);
                    filtered = true;
                    continue;
                }
                let index = self.or()?;
                self.expect(&Token::RBracket)?;
                value = match (&value, &index) {
                    (Value::Array(items), Value::Number(n)) => n
                        .as_f64()
                        .filter(|n| *n >= 0.0)
                        .and_then(|n| items.get(n as usize).cloned())
                        .unwrap_or(Value::Null),
                    _ => property(&value, &to_string(&index), filtered),
                };
            } else {
                return Ok(value);
            }
        }
    }

    fn primary(&mut self) -> Result<Value> {
        let Some(token) = self.tokens.get(self.pos).cloned() else {
            bail!("expression ended unexpectedly");
        };
        self.pos += 1;
        match token {
            Token::Str(s) => Ok(Value::String(s)),
            Token::Num(n) => Ok(number(n)),
            Token::LParen => {
                let value = self.or()?;
                self.expect(&Token::RParen)?;
                Ok(value)
            }
            Token::Ident(name) => {
                if self.eat(&Token::LParen) {
                    let mut args = Vec::new();
                    if !self.eat(&Token::RParen) {
                        loop {
                            args.push(self.or()?);
                            if self.eat(&Token::RParen) {
                                break;
                            }
                            self.expect(&Token::Comma)?;
                        }
                    }
                    return call(&name, &args, self.ctx);
                }
                Ok(match name.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    _ => self
                        .ctx
                        .values
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
                        .map(|(_, value)| value.clone())
                        .unwrap_or(Value::Null),
                })
            }
            other => bail!("unexpected {}", other.describe()),
        }
    }
}

/// `value.name`, ignoring case as GitHub does. With `filtered`, `value` is the result of
/// `.*` and the property applies to each element.
fn property(value: &Value, name: &str, filtered: bool) -> Value {
    let get = |v: &Value| match v {
        Value::Object(map) => map
            .get(name)
            .or_else(|| {
                map.iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
            .cloned()
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if filtered {
        match value {
            Value::Array(items) => {
                Value::Array(items.iter().map(get).filter(|v| !v.is_null()).collect())
            }
            _ => Value::Array(Vec::new()),
        }
    } else {
        get(value)
    }
}

fn elements(value: &Value, filtered: bool) -> Value {
    let of = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(items) => items.clone(),
            Value::Object(map) => map.values().cloned().collect(),
            _ => Vec::new(),
        }
    };
    match (value, filtered) {
        (Value::Array(items), true) => Value::Array(items.iter().flat_map(of).collect()),
        _ => Value::Array(of(value)),
    }
}

fn call(name: &str, args: &[Value], ctx: &Context) -> Result<Value> {
    let arity = |min: usize, max: usize| -> Result<()> {
        if args.len() < min || args.len() > max {
            bail!(
                "{name}() takes {min}{} arguments, got {}",
                if max > min {
                    format!(" to {max}")
                } else {
                    String::new()
                },
                args.len()
            );
        }
        Ok(())
    };
    let lower = |v: &Value| to_string(v).to_lowercase();
    Ok(match name.to_ascii_lowercase().as_str() {
        "success" => {
            arity(0, 0)?;
            Value::Bool(ctx.status == Status::Success)
        }
        "failure" => {
            arity(0, 0)?;
            Value::Bool(ctx.status == Status::Failure)
        }
        "cancelled" => {
            arity(0, 0)?;
            Value::Bool(ctx.status == Status::Cancelled)
        }
        "always" => {
            arity(0, 0)?;
            Value::Bool(true)
        }
        "contains" => {
            arity(2, 2)?;
            Value::Bool(match &args[0] {
                Value::Array(items) => items.iter().any(|item| loose_eq(item, &args[1])),
                haystack => lower(haystack).contains(&lower(&args[1])),
            })
        }
        "startswith" => {
            arity(2, 2)?;
            Value::Bool(lower(&args[0]).starts_with(&lower(&args[1])))
        }
        "endswith" => {
            arity(2, 2)?;
            Value::Bool(lower(&args[0]).ends_with(&lower(&args[1])))
        }
        "format" => {
            if args.is_empty() {
                bail!("format() needs a format string");
            }
            Value::String(format(&to_string(&args[0]), &args[1..])?)
        }
        "join" => {
            arity(1, 2)?;
            let separator = args.get(1).map_or(",".to_owned(), to_string);
            match &args[0] {
                Value::Array(items) => Value::String(
                    items
                        .iter()
                        .map(to_string)
                        .collect::<Vec<_>>()
                        .join(&separator),
                ),
                other => Value::String(to_string(other)),
            }
        }
        "tojson" => {
            arity(1, 1)?;
            Value::String(serde_json::to_string_pretty(&args[0])?)
        }
        "fromjson" => {
            arity(1, 1)?;
            let text = to_string(&args[0]);
            serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("fromJSON() got invalid JSON: {e}"))?
        }
        "hashfiles" => bail!("hashFiles() isn't supported yet"),
        _ => bail!("unknown function {name}()"),
    })
}

/// `format('{0} and {1}', a, b)`, with `{{` and `}}` for literal braces.
fn format(template: &str, args: &[Value]) -> Result<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut index = String::new();
                for d in chars.by_ref() {
                    if d == '}' {
                        break;
                    }
                    index.push(d);
                }
                let i: usize = index.parse().map_err(|_| {
                    anyhow::anyhow!("format() has an invalid placeholder {{{index}}}")
                })?;
                let arg = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("format() has no argument for {{{i}}}"))?;
                out.push_str(&to_string(arg));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Context {
        Context::new()
            .with(
                "github",
                json!({ "ref": "refs/heads/main", "event_name": "push", "event": { "commits": [{ "id": "a" }, { "id": "b" }] } }),
            )
            .with("matrix", json!({ "os": "ubuntu", "node": 18 }))
            .with("env", json!({ "EMPTY": "" }))
    }

    fn e(source: &str) -> Value {
        eval(source, &ctx()).unwrap()
    }

    #[test]
    fn evaluates_github_style() {
        assert_eq!(e("github.ref == 'REFS/heads/main'"), json!(true));
        assert_eq!(e("github.missing.deeper"), json!(null));
        assert_eq!(e("matrix.node >= 16 && 'yes' || 'no'"), json!("yes"));
        assert_eq!(e("env.EMPTY || 'fallback'"), json!("fallback"));
        assert_eq!(e("'18' == matrix.node"), json!(true));
        assert_eq!(e("!github.missing"), json!(true));
        assert_eq!(e("github['event_name']"), json!("push"));
        assert_eq!(e("github.event.commits.*.id"), json!(["a", "b"]));
        assert_eq!(e("contains(github.event.commits.*.id, 'B')"), json!(true));
        assert_eq!(e("contains('Hello world', 'WORLD')"), json!(true));
        assert_eq!(e("startsWith(github.ref, 'refs/tags/')"), json!(false));
        assert_eq!(
            e("format('{0}-{1} {{x}}', matrix.os, matrix.node)"),
            json!("ubuntu-18 {x}")
        );
        assert_eq!(e("join(fromJSON('[1,2,3]'), '+')"), json!("1+2+3"));
        assert_eq!(e("fromJSON('{\"a\": [true]}').a[0]"), json!(true));
        assert_eq!(e("'it''s'"), json!("it's"));
        assert_eq!(e("1.5 < 2 && -1 < 0"), json!(true));
    }

    #[test]
    fn interpolates_text() {
        assert_eq!(
            interpolate(
                "os=${{ matrix.os }} node=${{matrix.node}} ${{ '}}' }}",
                &ctx()
            )
            .unwrap(),
            "os=ubuntu node=18 }}"
        );
        assert_eq!(
            evaluate_value("${{ fromJSON('[1]') }}", &ctx()).unwrap(),
            json!([1])
        );
        assert!(interpolate("${{ matrix.os", &ctx()).is_err());
        assert!(eval("matrix.os ==", &ctx()).is_err());
        assert!(eval("nope()", &ctx()).is_err());
    }

    #[test]
    fn conditions_follow_status() {
        let mut ctx = ctx();
        assert!(condition("github.event_name == 'push'", &ctx).unwrap());
        assert!(condition("${{ matrix.os == 'ubuntu' }}", &ctx).unwrap());
        ctx.status = Status::Failure;
        assert!(!condition("github.event_name == 'push'", &ctx).unwrap());
        assert!(!condition("", &ctx).unwrap());
        assert!(condition("failure()", &ctx).unwrap());
        assert!(condition("always() && matrix.node == 18", &ctx).unwrap());
        assert!(!condition("success()", &ctx).unwrap());
    }
}
