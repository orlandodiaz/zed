//! Inline formula evaluation for the markdown preview.
//!
//! An inline code span starting with `= ` is a formula (Dataview-style
//! trigger, so other renderers degrade to a readable code chip). Functions use
//! Excel names and semantics, plus `FIELD("name")` to reference values from
//! the page itself (YAML frontmatter keys and two-column table rows) and
//! `AGE(date)` as sugar for `DATEDIF(date, TODAY(), "Y")`.
//!
//! The language is a closed expression evaluator — it can compute, but it
//! cannot touch the system, so evaluating on preview is safe by construction.

use std::collections::HashMap;

use chrono::{Datelike, Local, NaiveDate};

/// Scans a page's source for `FIELD(...)` targets: frontmatter `key: value`
/// lines and the first two cells of table rows, keyed case-insensitively.
/// Fenced code blocks are skipped. First occurrence of a key wins.
pub fn build_field_index(source: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let mut lines = source.lines().peekable();

    if lines.peek().map(|line| line.trim_end()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                insert_field(&mut fields, key, value);
            }
        }
    }

    let mut in_fence = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('|') {
            continue;
        }
        // `\|` (an escaped pipe inside a cell) must not split the row.
        let unescaped = trimmed.replace("\\|", "\u{0}");
        let mut cells = unescaped
            .split('|')
            .map(|cell| cell.replace('\u{0}', "|"))
            .skip(1);
        let (Some(key), Some(value)) = (cells.next(), cells.next()) else {
            continue;
        };
        // Skip `|---|---|` separator rows.
        if key.trim().chars().all(|char| matches!(char, '-' | ':' | ' ')) {
            continue;
        }
        insert_field(&mut fields, &key, &value);
    }
    fields
}

fn insert_field(fields: &mut HashMap<String, String>, key: &str, value: &str) {
    let key = strip_markup(key);
    let value = strip_markup(value);
    if key.is_empty() || value.is_empty() {
        return;
    }
    fields.entry(key.to_ascii_lowercase()).or_insert(value);
}

/// Strips inline markup (`**`, `` ` ``, wikilink brackets) so
/// `**Date of birth**` and `` `06/15/1990` `` match/read as plain values.
fn strip_markup(text: &str) -> String {
    let mut text = text.trim().to_string();
    text.retain(|char| !matches!(char, '*' | '`' | '[' | ']'));
    text.trim().to_string()
}

/// The frontmatter's key/value pairs in document order, for the preview's
/// properties panel. Machine-only keys (`icon`) are skipped.
pub fn frontmatter_pairs(source: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut lines = source.lines();
    if lines.next().map(|line| line.trim_end()) != Some("---") {
        return pairs;
    }
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() || key.eq_ignore_ascii_case("icon") {
                continue;
            }
            pairs.push((key.to_string(), value.to_string()));
        }
    }
    pairs
}

/// Key under which a cross-page field is stored in the combined index:
/// `page\0field`, both lowercased. `\0` can't appear in either name.
pub fn cross_page_key(page: &str, field: &str) -> String {
    format!(
        "{}\u{0}{}",
        page.trim().to_ascii_lowercase(),
        field.trim().to_ascii_lowercase()
    )
}

/// Page names referenced by two-argument `FIELD("page", "field")` calls, so
/// the preview can load and index exactly those pages ahead of evaluation.
pub fn referenced_pages(source: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let upper = source.to_ascii_uppercase();
    let mut search = 0;
    while let Some(found) = upper[search..].find("FIELD") {
        let mut rest = source[search + found + "FIELD".len()..].trim_start();
        search += found + "FIELD".len();
        if !rest.starts_with('(') {
            continue;
        }
        rest = rest[1..].trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        let first_argument = &rest[..end];
        if rest[end + 1..].trim_start().starts_with(',') {
            let page = first_argument.trim().to_string();
            if !page.is_empty() && !pages.contains(&page) {
                pages.push(page);
            }
        }
    }
    pages
}

/// Evaluates a formula (the text after the `= ` trigger) against the page's
/// field index. Always returns display text; errors render Excel-style.
pub fn evaluate(expression: &str, fields: &HashMap<String, String>) -> String {
    let tokens = match tokenize(expression) {
        Ok(tokens) => tokens,
        Err(message) => return format!("#ERROR: {message}"),
    };
    match (Parser { tokens, position: 0, fields }).parse() {
        Ok(value) => value.display(),
        Err(message) => format!("#ERROR: {message}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Number(f64),
    Text(String),
    Date(NaiveDate),
    Bool(bool),
}

impl Value {
    fn display(&self) -> String {
        match self {
            Value::Number(number) => {
                if number.fract() == 0.0 && number.abs() < 1e15 {
                    format!("{}", *number as i64)
                } else {
                    let formatted = format!("{number:.6}");
                    formatted
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .to_string()
                }
            }
            Value::Text(text) => text.clone(),
            Value::Date(date) => date.format("%Y-%m-%d").to_string(),
            Value::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_string(),
        }
    }

    fn to_number(&self) -> Result<f64, String> {
        match self {
            Value::Number(number) => Ok(*number),
            Value::Bool(value) => Ok(*value as u8 as f64),
            Value::Text(text) => text
                .trim()
                .replace(',', "")
                .parse()
                .map_err(|_| format!("\"{text}\" is not a number")),
            Value::Date(_) => Err("expected a number, got a date".into()),
        }
    }

    fn to_date(&self) -> Result<NaiveDate, String> {
        match self {
            Value::Date(date) => Ok(*date),
            Value::Text(text) => parse_date(text).ok_or_else(|| {
                format!("\"{text}\" is not a date (try MM/DD/YYYY or YYYY-MM-DD)")
            }),
            _ => Err(format!("expected a date, got {}", self.display())),
        }
    }

    fn to_bool(&self) -> Result<bool, String> {
        match self {
            Value::Bool(value) => Ok(*value),
            Value::Number(number) => Ok(*number != 0.0),
            _ => Err(format!("expected TRUE/FALSE, got \"{}\"", self.display())),
        }
    }
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    // Written-month forms accept an optional comma; normalizing it away lets
    // one format string cover "April 29, 2026" and "April 29 2026".
    let text = text.trim().replace(',', " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for format in [
        "%m/%d/%Y",
        "%m/%d/%y",
        "%m-%d-%Y",
        "%Y-%m-%d",
        "%Y/%m/%d",
        // Written months are unambiguous, unlike European day-first numerics.
        "%B %d %Y",
        "%b %d %Y",
        "%d %B %Y",
        "%d %b %Y",
    ] {
        if let Ok(date) = NaiveDate::parse_from_str(&text, format) {
            return Some(date);
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Text(String),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Ampersand,
    OpenParen,
    CloseParen,
    Comma,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

fn tokenize(expression: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = expression.char_indices().peekable();
    while let Some((index, char)) = chars.next() {
        match char {
            ' ' | '\t' => {}
            '+' => tokens.push(Token::Plus),
            '-' => tokens.push(Token::Minus),
            '*' => tokens.push(Token::Star),
            '/' => tokens.push(Token::Slash),
            '&' => tokens.push(Token::Ampersand),
            '(' => tokens.push(Token::OpenParen),
            ')' => tokens.push(Token::CloseParen),
            ',' => tokens.push(Token::Comma),
            '=' => tokens.push(Token::Equal),
            '<' => match chars.peek() {
                Some((_, '=')) => {
                    chars.next();
                    tokens.push(Token::LessEqual);
                }
                Some((_, '>')) => {
                    chars.next();
                    tokens.push(Token::NotEqual);
                }
                _ => tokens.push(Token::Less),
            },
            '>' => {
                if let Some((_, '=')) = chars.peek() {
                    chars.next();
                    tokens.push(Token::GreaterEqual);
                } else {
                    tokens.push(Token::Greater);
                }
            }
            '"' => {
                let mut text = String::new();
                loop {
                    match chars.next() {
                        Some((_, '"')) => break,
                        Some((_, char)) => text.push(char),
                        None => return Err("unclosed string".into()),
                    }
                }
                tokens.push(Token::Text(text));
            }
            '0'..='9' | '.' => {
                let start = index;
                let mut end = index + char.len_utf8();
                while let Some((next_index, next)) = chars.peek().copied() {
                    if next.is_ascii_digit() || next == '.' {
                        end = next_index + next.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                let literal = &expression[start..end];
                tokens.push(Token::Number(
                    literal
                        .parse()
                        .map_err(|_| format!("bad number \"{literal}\""))?,
                ));
            }
            char if char.is_alphabetic() || char == '_' => {
                let start = index;
                let mut end = index + char.len_utf8();
                while let Some((next_index, next)) = chars.peek().copied() {
                    if next.is_alphanumeric() || next == '_' {
                        end = next_index + next.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(expression[start..end].to_string()));
            }
            other => return Err(format!("unexpected character \"{other}\"")),
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    fields: &'a HashMap<String, String>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self) -> Result<Value, String> {
        if self.tokens.is_empty() {
            return Err("empty formula".into());
        }
        let value = self.comparison()?;
        if self.position != self.tokens.len() {
            return Err("unexpected trailing input".into());
        }
        Ok(value)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn expect(&mut self, token: Token, context: &str) -> Result<(), String> {
        if self.next().as_ref() == Some(&token) {
            Ok(())
        } else {
            Err(format!("expected {token:?} {context}"))
        }
    }

    /// comparison := expression (("=" | "<>" | "<" | "<=" | ">" | ">=") expression)?
    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.expression()?;
        let Some(operator) = self.peek().cloned() else {
            return Ok(left);
        };
        let compare = |ordering: std::cmp::Ordering, operator: &Token| match operator {
            Token::Less => ordering.is_lt(),
            Token::LessEqual => ordering.is_le(),
            Token::Greater => ordering.is_gt(),
            Token::GreaterEqual => ordering.is_ge(),
            _ => unreachable!(),
        };
        match operator {
            Token::Equal | Token::NotEqual => {
                self.next();
                let right = self.expression()?;
                let equal = values_equal(&left, &right);
                Ok(Value::Bool(if matches!(operator, Token::Equal) {
                    equal
                } else {
                    !equal
                }))
            }
            Token::Less | Token::LessEqual | Token::Greater | Token::GreaterEqual => {
                self.next();
                let right = self.expression()?;
                let ordering = match (&left, &right) {
                    (Value::Date(_), _) | (_, Value::Date(_)) => {
                        left.to_date()?.cmp(&right.to_date()?)
                    }
                    (Value::Text(left), Value::Text(right)) => left.cmp(right),
                    _ => left
                        .to_number()?
                        .partial_cmp(&right.to_number()?)
                        .ok_or("cannot compare these values")?,
                };
                Ok(Value::Bool(compare(ordering, &operator)))
            }
            _ => Ok(left),
        }
    }

    /// expression := term (("+" | "-" | "&") term)*
    fn expression(&mut self) -> Result<Value, String> {
        let mut left = self.term()?;
        while let Some(operator) = self.peek().cloned() {
            match operator {
                Token::Plus | Token::Minus => {
                    self.next();
                    let right = self.term()?;
                    left = add_or_subtract(left, right, matches!(operator, Token::Minus))?;
                }
                Token::Ampersand => {
                    self.next();
                    let right = self.term()?;
                    left = Value::Text(format!("{}{}", left.display(), right.display()));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// term := factor (("*" | "/") factor)*
    fn term(&mut self) -> Result<Value, String> {
        let mut left = self.factor()?;
        while let Some(operator) = self.peek().cloned() {
            match operator {
                Token::Star | Token::Slash => {
                    self.next();
                    let right = self.factor()?.to_number()?;
                    let left_number = left.to_number()?;
                    left = if matches!(operator, Token::Star) {
                        Value::Number(left_number * right)
                    } else if right == 0.0 {
                        return Err("division by zero".into());
                    } else {
                        Value::Number(left_number / right)
                    };
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn factor(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Number(number)) => Ok(Value::Number(number)),
            Some(Token::Text(text)) => Ok(Value::Text(text)),
            Some(Token::Minus) => Ok(Value::Number(-self.factor()?.to_number()?)),
            Some(Token::OpenParen) => {
                let value = self.comparison()?;
                self.expect(Token::CloseParen, "to close \"(\"")?;
                Ok(value)
            }
            Some(Token::Ident(name)) => {
                match name.to_ascii_uppercase().as_str() {
                    "TRUE" if self.peek() != Some(&Token::OpenParen) => {
                        return Ok(Value::Bool(true));
                    }
                    "FALSE" if self.peek() != Some(&Token::OpenParen) => {
                        return Ok(Value::Bool(false));
                    }
                    _ => {}
                }
                self.expect(Token::OpenParen, &format!("after {name}"))?;
                let mut arguments = Vec::new();
                if self.peek() == Some(&Token::CloseParen) {
                    self.next();
                } else {
                    loop {
                        arguments.push(self.comparison()?);
                        match self.next() {
                            Some(Token::Comma) => continue,
                            Some(Token::CloseParen) => break,
                            _ => return Err(format!("expected \",\" or \")\" in {name}(...)")),
                        }
                    }
                }
                self.call(&name, arguments)
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn call(&mut self, name: &str, arguments: Vec<Value>) -> Result<Value, String> {
        let arity = |expected: usize| -> Result<(), String> {
            if arguments.len() == expected {
                Ok(())
            } else {
                Err(format!(
                    "{} takes {expected} argument(s), got {}",
                    name.to_ascii_uppercase(),
                    arguments.len()
                ))
            }
        };
        let variadic_numbers = || -> Result<Vec<f64>, String> {
            if arguments.is_empty() {
                return Err(format!("{} needs at least one argument", name.to_ascii_uppercase()));
            }
            arguments.iter().map(Value::to_number).collect()
        };
        match name.to_ascii_uppercase().as_str() {
            "TODAY" => {
                arity(0)?;
                Ok(Value::Date(Local::now().date_naive()))
            }
            "DATE" => {
                arity(3)?;
                let year = arguments[0].to_number()? as i32;
                let month = arguments[1].to_number()? as u32;
                let day = arguments[2].to_number()? as u32;
                NaiveDate::from_ymd_opt(year, month, day)
                    .map(Value::Date)
                    .ok_or_else(|| format!("invalid date {year}-{month}-{day}"))
            }
            "DATEDIF" => {
                arity(3)?;
                let start = arguments[0].to_date()?;
                let end = arguments[1].to_date()?;
                let unit = arguments[2].display().to_ascii_uppercase();
                date_difference(start, end, &unit).map(Value::Number)
            }
            "AGE" => {
                arity(1)?;
                let birth = arguments[0].to_date()?;
                date_difference(birth, Local::now().date_naive(), "Y").map(Value::Number)
            }
            "ROUND" => {
                if arguments.is_empty() || arguments.len() > 2 {
                    return Err("ROUND takes 1 or 2 arguments".into());
                }
                let number = arguments[0].to_number()?;
                let digits = arguments
                    .get(1)
                    .map(Value::to_number)
                    .transpose()?
                    .unwrap_or(0.0) as i32;
                let factor = 10f64.powi(digits);
                Ok(Value::Number((number * factor).round() / factor))
            }
            "ABS" => {
                arity(1)?;
                Ok(Value::Number(arguments[0].to_number()?.abs()))
            }
            "SUM" => Ok(Value::Number(variadic_numbers()?.into_iter().sum())),
            "MIN" => Ok(Value::Number(
                variadic_numbers()?.into_iter().fold(f64::INFINITY, f64::min),
            )),
            "MAX" => Ok(Value::Number(
                variadic_numbers()?
                    .into_iter()
                    .fold(f64::NEG_INFINITY, f64::max),
            )),
            "YEAR" => {
                arity(1)?;
                Ok(Value::Number(arguments[0].to_date()?.year() as f64))
            }
            "MONTH" => {
                arity(1)?;
                Ok(Value::Number(arguments[0].to_date()?.month() as f64))
            }
            "DAY" => {
                arity(1)?;
                Ok(Value::Number(arguments[0].to_date()?.day() as f64))
            }
            "IF" => {
                arity(3)?;
                if arguments[0].to_bool()? {
                    Ok(arguments[1].clone())
                } else {
                    Ok(arguments[2].clone())
                }
            }
            "AND" => {
                let mut result = true;
                for argument in &arguments {
                    result &= argument.to_bool()?;
                }
                Ok(Value::Bool(result))
            }
            "OR" => {
                let mut result = false;
                for argument in &arguments {
                    result |= argument.to_bool()?;
                }
                Ok(Value::Bool(result))
            }
            "NOT" => {
                arity(1)?;
                Ok(Value::Bool(!arguments[0].to_bool()?))
            }
            "UPPER" => {
                arity(1)?;
                Ok(Value::Text(arguments[0].display().to_uppercase()))
            }
            "LOWER" => {
                arity(1)?;
                Ok(Value::Text(arguments[0].display().to_lowercase()))
            }
            "LEN" => {
                arity(1)?;
                Ok(Value::Number(arguments[0].display().chars().count() as f64))
            }
            "FIELD" => match arguments.len() {
                1 => {
                    let key = arguments[0].display();
                    self.fields
                        .get(&key.to_ascii_lowercase())
                        .cloned()
                        .map(Value::Text)
                        .ok_or_else(|| format!("no field named \"{key}\" on this page"))
                }
                2 => {
                    let page = arguments[0].display();
                    let key = arguments[1].display();
                    self.fields
                        .get(&cross_page_key(&page, &key))
                        .cloned()
                        .map(Value::Text)
                        .ok_or_else(|| {
                            format!("no field named \"{key}\" on page \"{page}\"")
                        })
                }
                _ => Err("FIELD takes 1 or 2 arguments".into()),
            },
            other => Err(format!("unknown function {other}")),
        }
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    if left == right {
        return true;
    }
    // Cross-type equality through coercion: FIELD text vs number/date.
    if let (Ok(left), Ok(right)) = (left.to_date(), right.to_date()) {
        return left == right;
    }
    if let (Ok(left), Ok(right)) = (left.to_number(), right.to_number()) {
        return left == right;
    }
    left.display().eq_ignore_ascii_case(&right.display())
}

fn date_difference(start: NaiveDate, end: NaiveDate, unit: &str) -> Result<f64, String> {
    if end < start {
        return Err("DATEDIF: end date is before start date".into());
    }
    match unit {
        "Y" => {
            let mut years = end.year() - start.year();
            if (end.month(), end.day()) < (start.month(), start.day()) {
                years -= 1;
            }
            Ok(years as f64)
        }
        "M" => {
            let mut months =
                (end.year() - start.year()) * 12 + end.month() as i32 - start.month() as i32;
            if end.day() < start.day() {
                months -= 1;
            }
            Ok(months as f64)
        }
        "D" => Ok((end - start).num_days() as f64),
        other => Err(format!("DATEDIF: unknown unit \"{other}\"")),
    }
}

fn add_or_subtract(left: Value, right: Value, subtract: bool) -> Result<Value, String> {
    match (&left, &right) {
        // Date arithmetic: date ± days, and date - date = days.
        (Value::Date(date), _) => {
            if let Value::Date(other) = right {
                if subtract {
                    return Ok(Value::Number((*date - other).num_days() as f64));
                }
                return Err("cannot add two dates".into());
            }
            let mut days = right.to_number()? as i64;
            if subtract {
                days = -days;
            }
            date.checked_add_signed(chrono::Duration::days(days))
                .map(Value::Date)
                .ok_or_else(|| "date out of range".into())
        }
        _ => {
            let left = left.to_number()?;
            let right = right.to_number()?;
            Ok(Value::Number(if subtract { left - right } else { left + right }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> HashMap<String, String> {
        build_field_index(
            "---\nicon: person\nstarted: 2020-03-15\n---\n\n\
             | Who | Value |\n|---|---|\n| **Date of birth** | `06/15/1990` |\n| Team | CS |\n\
             | Price | 1,200 |\n",
        )
    }

    #[test]
    fn field_index_reads_frontmatter_and_tables() {
        let fields = fields();
        assert_eq!(fields.get("date of birth").unwrap(), "06/15/1990");
        assert_eq!(fields.get("team").unwrap(), "CS");
        assert_eq!(fields.get("started").unwrap(), "2020-03-15");
    }

    #[test]
    fn arithmetic_round_and_text() {
        let fields = HashMap::new();
        assert_eq!(evaluate("1 + 2 * 3", &fields), "7");
        assert_eq!(evaluate("ROUND(10 / 3, 2)", &fields), "3.33");
        assert_eq!(evaluate("\"a\" & \"b\" & 3", &fields), "ab3");
        assert_eq!(evaluate("SUM(1, 2, 3, 4)", &fields), "10");
        assert_eq!(evaluate("MAX(3, 9, 4)", &fields), "9");
        assert_eq!(evaluate("LEN(\"hello\")", &fields), "5");
    }

    #[test]
    fn date_functions() {
        let fields = fields();
        assert_eq!(
            evaluate("DATEDIF(DATE(1990, 6, 15), DATE(2026, 6, 15), \"Y\")", &fields),
            "36"
        );
        assert_eq!(
            evaluate(
                "DATEDIF(FIELD(\"Date of birth\"), DATE(2026, 3, 31), \"Y\")",
                &fields
            ),
            "35"
        );
        assert_eq!(evaluate("DATE(2026, 1, 10) - DATE(2026, 1, 1)", &fields), "9");
        assert_eq!(evaluate("DATE(2026, 1, 1) + 30", &fields), "2026-01-31");
        assert_eq!(evaluate("YEAR(DATE(2026, 5, 4))", &fields), "2026");
    }

    #[test]
    fn logic_and_comparisons() {
        let fields = fields();
        assert_eq!(evaluate("IF(2 > 1, \"yes\", \"no\")", &fields), "yes");
        assert_eq!(
            evaluate("IF(FIELD(\"Team\") = \"cs\", \"ours\", \"theirs\")", &fields),
            "ours"
        );
        assert_eq!(evaluate("AND(TRUE, 1 < 2)", &fields), "TRUE");
        assert_eq!(evaluate("NOT(1 = 2)", &fields), "TRUE");
        assert_eq!(evaluate("FIELD(\"Price\") * 2", &fields), "2400");
    }

    #[test]
    fn cross_page_fields() {
        let mut fields = fields();
        fields.insert(
            cross_page_key("Team Roster", "Date of birth"),
            "06/15/1990".into(),
        );
        assert_eq!(
            evaluate(
                "YEAR(FIELD(\"Team Roster\", \"Date of birth\"))",
                &fields
            ),
            "1990"
        );
        assert!(
            evaluate("FIELD(\"Other page\", \"x\")", &fields).starts_with("#ERROR")
        );
        assert_eq!(
            referenced_pages(
                "text `= FIELD(\"Page One\", \"a\") + field(\"Page Two\", \"b\") + FIELD(\"local\")`"
            ),
            vec!["Page One".to_string(), "Page Two".to_string()]
        );
    }

    #[test]
    fn written_month_dates() {
        let fields = HashMap::new();
        for date in [
            "\"April 29, 2026\"",
            "\"Apr 29 2026\"",
            "\"29 April 2026\"",
            "\"04/29/2026\"",
            "\"04-29-2026\"",
        ] {
            assert_eq!(
                evaluate(&format!("YEAR({date})"), &fields),
                "2026",
                "failed for {date}"
            );
        }
    }

    #[test]
    fn errors_render_inline() {
        let fields = HashMap::new();
        assert!(evaluate("FIELD(\"missing\")", &fields).starts_with("#ERROR"));
        assert!(evaluate("NOPE(1)", &fields).starts_with("#ERROR"));
        assert!(evaluate("1 / 0", &fields).starts_with("#ERROR"));
        assert!(evaluate("", &fields).starts_with("#ERROR"));
    }
}
