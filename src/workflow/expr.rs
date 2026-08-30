//! `when` gate expressions — the JS subset AgentFlow scoped (literals,
//! arithmetic, comparison, logic, ternary; no property access, no calls).
//!
//! Upstream outputs enter as `{{id.output}}` placeholders substituted with
//! JSON string literals BEFORE lexing, so `"{{a.output}}" == "yes"` compares
//! two strings. Any lex/parse/eval error is a configuration error: the node
//! fails loudly rather than silently taking a branch.

use std::collections::HashMap;

/// Substitute `{{id.output}}` placeholders with JSON-quoted string literals
/// of the upstream outputs. A missing reference is an error (validation
/// guarantees transitively-upstream refs exist by evaluation time).
pub(crate) fn substitute_refs(
    expr: &str,
    outputs: &HashMap<String, String>,
) -> Result<String, String> {
    let mut result = String::with_capacity(expr.len());
    let mut rest = expr;
    while let Some(start) = rest.find("{{") {
        let after_braces = &rest[start + 2..];
        let Some(end) = after_braces.find("}}") else {
            break;
        };
        let token = &after_braces[..end];
        let Some(id) = token.strip_suffix(".output") else {
            break;
        };
        let output = outputs.get(id).ok_or_else(|| {
            format!("when references '{{{{{id}.output}}}}' whose value is not available yet")
        })?;
        result.push_str(&rest[..start]);
        // serde_json never produces a bare `"` / newline in the quoted form.
        result.push_str(&serde_json::to_string(output).unwrap_or_default());
        rest = &after_braces[end + 2..];
    }
    result.push_str(rest);
    Ok(result)
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Self::Str(text) => !text.is_empty(),
            Self::Num(number) => *number != 0.0,
            Self::Bool(flag) => *flag,
            Self::Null => false,
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Str(text) => text.clone(),
            Self::Num(number) => format_number(*number),
            Self::Bool(flag) => flag.to_string(),
            Self::Null => "null".to_string(),
        }
    }
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn parse_num(text: &str) -> Option<f64> {
    text.trim().parse().ok()
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Str(String),
    Num(f64),
    Ident(String),
    Op(&'static str),
}

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            ' ' | '\t' | '\r' | '\n' => index += 1,
            '"' => {
                let mut text = String::new();
                index += 1;
                loop {
                    let Some(&next) = chars.get(index) else {
                        return Err("unterminated string literal in when expression".to_string());
                    };
                    index += 1;
                    match next {
                        '"' => break,
                        '\\' => {
                            let Some(&escaped) = chars.get(index) else {
                                return Err(
                                    "unterminated escape in when expression".to_string()
                                );
                            };
                            index += 1;
                            text.push(match escaped {
                                'n' => '\n',
                                't' => '\t',
                                other => other,
                            });
                        }
                        other => text.push(other),
                    }
                }
                tokens.push(Token::Str(text));
            }
            '0'..='9' => {
                let start = index;
                while matches!(chars.get(index), Some('0'..='9' | '.')) {
                    index += 1;
                }
                let text: String = chars[start..index].iter().collect();
                let number: f64 = text
                    .parse()
                    .map_err(|_| format!("invalid number '{text}' in when expression"))?;
                tokens.push(Token::Num(number));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = index;
                while matches!(chars.get(index), Some(c) if c.is_alphanumeric() || *c == '_') {
                    index += 1;
                }
                let text: String = chars[start..index].iter().collect();
                tokens.push(Token::Ident(text));
            }
            _ => {
                let two: String = chars[index..(index + 2).min(chars.len())].iter().collect();
                let op = ["==", "!=", "<=", ">=", "&&", "||"]
                    .into_iter()
                    .find(|candidate| *candidate == two);
                if let Some(op) = op {
                    tokens.push(Token::Op(op));
                    index += 2;
                } else if matches!(
                    ch,
                    '(' | ')' | '?' | ':' | '<' | '>' | '+' | '-' | '*' | '/' | '%' | '!'
                ) {
                    tokens.push(Token::Op(match ch {
                        '(' => "(",
                        ')' => ")",
                        '?' => "?",
                        ':' => ":",
                        '<' => "<",
                        '>' => ">",
                        '+' => "+",
                        '-' => "-",
                        '*' => "*",
                        '/' => "/",
                        '%' => "%",
                        '!' => "!",
                        _ => unreachable!("matched above"),
                    }));
                    index += 1;
                } else {
                    return Err(format!("unexpected character '{ch}' in when expression"));
                }
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek_op(&self) -> Option<&'static str> {
        match self.tokens.get(self.position) {
            Some(Token::Op(op)) => Some(op),
            _ => None,
        }
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.peek_op() == Some(op) {
            self.position += 1;
            Ok(())
        } else {
            Err(format!("expected '{op}' in when expression"))
        }
    }

    // ternary := or ('?' ternary ':' ternary)?
    fn ternary(&mut self) -> Result<Value, String> {
        let condition = self.or()?;
        if self.peek_op() == Some("?") {
            self.position += 1;
            let then = self.ternary()?;
            self.expect_op(":")?;
            let otherwise = self.ternary()?;
            return Ok(if condition.truthy() { then } else { otherwise });
        }
        Ok(condition)
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut left = self.and()?;
        while self.peek_op() == Some("||") {
            self.position += 1;
            let right = self.and()?;
            left = Value::Bool(left.truthy() || right.truthy());
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut left = self.equality()?;
        while self.peek_op() == Some("&&") {
            self.position += 1;
            let right = self.equality()?;
            left = Value::Bool(left.truthy() && right.truthy());
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Value, String> {
        let mut left = self.comparison()?;
        loop {
            let op = match self.peek_op() {
                Some("==" | "!=") => self.tokens[self.position].clone(),
                _ => break,
            };
            self.position += 1;
            let right = self.comparison()?;
            let Token::Op(op) = op else { unreachable!("matched above") };
            let equal = match (&left, &right) {
                (Value::Str(a), Value::Str(b)) => a == b,
                (Value::Num(a), Value::Num(b)) => a == b,
                (Value::Bool(a), Value::Bool(b)) => a == b,
                (Value::Null, Value::Null) => true,
                // Mixed types never coerce: a number 3 is not the text "3".
                _ => false,
            };
            left = Value::Bool(if op == "==" { equal } else { !equal });
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek_op() {
                Some("<" | "<=" | ">" | ">=") => self.tokens[self.position].clone(),
                _ => break,
            };
            self.position += 1;
            let right = self.additive()?;
            let Token::Op(op) = op else { unreachable!("matched above") };
            // Outputs arrive as text, so a number literal next to an output
            // reference coerces the text to a number; unparseable fails
            // loudly. Equality stays strict ("3" != 3).
            let ordered = match (&left, &right) {
                (Value::Num(a), Value::Num(b)) => Some(a.partial_cmp(b)),
                (Value::Str(a), Value::Str(b)) => Some(a.partial_cmp(b)),
                (Value::Num(a), Value::Str(b)) => parse_num(b).map(|b| a.partial_cmp(&b)),
                (Value::Str(a), Value::Num(b)) => parse_num(a).map(|a| a.partial_cmp(b)),
                _ => None,
            };
            let Some(ordering) = ordered.flatten() else {
                return Err(format!(
                    "'{op}' needs two numbers or two strings in when expression"
                ));
            };
            let holds = match op {
                "<" => ordering.is_lt(),
                "<=" => ordering.is_le(),
                ">" => ordering.is_gt(),
                ">=" => ordering.is_ge(),
                _ => unreachable!("matched above"),
            };
            left = Value::Bool(holds);
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Value, String> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek_op() {
                Some("+" | "-") => self.tokens[self.position].clone(),
                _ => break,
            };
            self.position += 1;
            let right = self.multiplicative()?;
            let Token::Op(op) = op else { unreachable!("matched above") };
            left = match (&left, &right, op) {
                (Value::Num(a), Value::Num(b), "+") => Value::Num(a + b),
                (Value::Num(a), Value::Num(b), "-") => Value::Num(a - b),
                // '+' on anything else concatenates the display forms.
                (_, _, "+") => Value::Str(format!("{}{}", left.display(), right.display())),
                (_, _, _) => {
                    return Err("'-' needs two numbers in when expression".to_string())
                }
            };
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Value, String> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek_op() {
                Some("*" | "/" | "%") => self.tokens[self.position].clone(),
                _ => break,
            };
            self.position += 1;
            let right = self.unary()?;
            let Token::Op(op) = op else { unreachable!("matched above") };
            let (Value::Num(a), Value::Num(b)) = (&left, &right) else {
                return Err(format!("'{op}' needs two numbers in when expression"));
            };
            left = match op {
                "*" => Value::Num(a * b),
                "/" => {
                    if *b == 0.0 {
                        return Err("division by zero in when expression".to_string());
                    }
                    Value::Num(a / b)
                }
                _ => Value::Num(a % b),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.peek_op() == Some("!") {
            self.position += 1;
            let operand = self.unary()?;
            return Ok(Value::Bool(!operand.truthy()));
        }
        if self.peek_op() == Some("-") {
            self.position += 1;
            let operand = self.unary()?;
            let Value::Num(number) = operand else {
                return Err("unary '-' needs a number in when expression".to_string());
            };
            return Ok(Value::Num(-number));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.bump() {
            Some(Token::Str(text)) => Ok(Value::Str(text)),
            Some(Token::Num(number)) => Ok(Value::Num(number)),
            Some(Token::Ident(ident)) => match ident.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                other => Err(format!("unknown identifier '{other}' in when expression")),
            },
            Some(Token::Op("(")) => {
                let inner = self.ternary()?;
                self.expect_op(")")?;
                Ok(inner)
            }
            other => Err(format!(
                "expected a value in when expression, found {other:?}"
            )),
        }
    }
}

/// Evaluate a `when` expression against the available outputs: substitute
/// refs, lex, parse, evaluate. The caller decides what a `false` means
/// (branch prune); errors are node-fatal configuration failures.
pub(crate) fn evaluate_when(expr: &str, outputs: &HashMap<String, String>) -> Result<bool, String> {
    let substituted = substitute_refs(expr, outputs)?;
    let tokens = lex(&substituted)?;
    let mut parser = Parser { tokens, position: 0 };
    let value = parser.ternary()?;
    if parser.position != parser.tokens.len() {
        return Err("trailing tokens after when expression".to_string());
    }
    Ok(value.truthy())
}

/// Syntax-check a `when` gate at workflow-parse time: refs are replaced
/// with dummy string literals, then the expression must lex and parse.
/// Semantic errors (type mismatches) only surface at dispatch, where the
/// real output types are known.
pub(crate) fn validate_when_syntax(expr: &str) -> Result<(), String> {
    let mut dummy = String::new();
    let mut rest = expr;
    while let Some(start) = rest.find("{{") {
        let after_braces = &rest[start + 2..];
        let Some(end) = after_braces.find("}}") else {
            break;
        };
        if !after_braces[..end].ends_with(".output") {
            break;
        }
        dummy.push_str(&rest[..start]);
        dummy.push_str("\"0\"");
        rest = &after_braces[end + 2..];
    }
    dummy.push_str(rest);
    let tokens = lex(&dummy)?;
    let mut parser = Parser { tokens, position: 0 };
    parser.ternary()?;
    if parser.position != parser.tokens.len() {
        return Err("trailing tokens after when expression".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_refs_as_quoted_literals() {
        let substituted = substitute_refs(
            r#"{{a.output}} == "yes""#,
            &outputs(&[("a", "yes"), ("b", "he said \"hi\"\n")]),
        )
        .unwrap();
        assert_eq!(substituted, r#""yes" == "yes""#);
    }

    #[test]
    fn missing_ref_is_an_error() {
        let err = substitute_refs("{{ghost.output}} == \"x\"", &HashMap::new()).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn evaluates_comparisons_and_logic() {
        let outs = outputs(&[("a", "go"), ("n", "3.5")]);
        assert!(evaluate_when(r#"{{a.output}} == "go""#, &outs).unwrap());
        assert!(!evaluate_when(r#"{{a.output}} == "stop""#, &outs).unwrap());
        assert!(evaluate_when("{{n.output}} > 3", &outs).unwrap());
        assert!(evaluate_when("{{n.output}} >= 3.5 && {{n.output}} < 4", &outs).unwrap());
        assert!(evaluate_when(r#"{{a.output}} != "stop" || 1 > 2"#, &outs).unwrap());
        assert!(evaluate_when("!({{n.output}} > 4)", &outs).unwrap());
        assert!(evaluate_when("1 > 2 ? false : true", &outs).unwrap());
        // Numbers never coerce against text.
        assert!(!evaluate_when("{{n.output}} == 3.5 || 3 == \"3\"", &outs).unwrap());
    }

    #[test]
    fn string_concat_and_truthiness() {
        let outs = outputs(&[("a", "x")]);
        assert!(evaluate_when(r#"{{a.output}} + "y" == "xy""#, &outs).unwrap());
        assert!(evaluate_when("true && \"nonempty\"", &outs).unwrap());
        assert!(!evaluate_when("\"\" || null || 0", &outs).unwrap());
    }

    #[test]
    fn arithmetic_precedence() {
        assert!(evaluate_when("2 + 3 * 4 == 14", &HashMap::new()).unwrap());
        assert!(evaluate_when("(2 + 3) * 4 == 20", &HashMap::new()).unwrap());
        assert!(evaluate_when("10 % 3 == 1 && 7 - 2 == 5", &HashMap::new()).unwrap());
    }

    #[test]
    fn malformed_expressions_fail_loudly() {
        for bad in [
            "1 +",
            "== 3",
            "\"unterminated",
            "foo == 1",
            "1 / 0",
            "1 1",
            "(1 + 2",
            "3 & 4",
        ] {
            assert!(
                evaluate_when(bad, &HashMap::new()).is_err(),
                "expected error for {bad:?}"
            );
        }
        // A numeric comparison against text that is not a number fails.
        assert!(evaluate_when("\"abc\" > 1", &HashMap::new()).is_err());
    }

    #[test]
    fn escapes_survive_substitution() {
        let outs = outputs(&[("a", "line1\nline2 \"quoted\"")]);
        assert!(evaluate_when(
            r#"{{a.output}} == "line1\nline2 \"quoted\"""#,
            &outs
        )
        .unwrap());
    }
}
