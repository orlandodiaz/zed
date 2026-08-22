//! Notion-style database views for the markdown preview.
//!
//! A ```view fenced block queries a "database" — a folder of pages, each
//! carrying fields (frontmatter + two-column tables) — and compiles to an
//! ordinary markdown table before parsing, so sorting, icons, and wikilinks
//! all apply to the result. Other renderers show the query as a code block.
//!
//! ```view
//! from: Credit cards
//! columns: Page, Issuer, Credit limit, Status
//! where: COL("Status") = "Active"
//! sort: Credit limit desc
//! ```

use std::collections::HashMap;
use std::ops::Range;

use crate::formulas;

/// One parsed ```view block and its byte range in the source.
#[derive(Debug, PartialEq)]
pub struct ViewBlock {
    pub range: Range<usize>,
    pub from: String,
    pub columns: Vec<ViewColumn>,
    pub where_expression: Option<String>,
    /// Column name and whether to sort descending.
    pub sort: Option<(String, bool)>,
    /// Footer aggregates: function name and the column it applies to
    /// (`COUNT` may be bare, counting records).
    pub summarize: Vec<(String, Option<String>)>,
}

/// One view column: the record field it reads and an optional Notion-style
/// number format, declared as `DOLLAR(Credit limit)` / `DOLLAR(Balance, 2)` /
/// `PERCENT(APR)`. The header always shows the plain field name.
#[derive(Debug, PartialEq)]
pub struct ViewColumn {
    pub field: String,
    pub format: Option<ColumnFormat>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ColumnFormat {
    Dollar(usize),
    Percent,
    /// Computed: renders the age in years of a date field; header says "Age".
    Age,
    /// Computed: renders the age of a date field as "6 years, 3 months"
    /// (`AGE(field, ym)`); header says "Age".
    AgeYearsMonths,
    /// US phone formatting from raw digits: `(469) 203-3705`.
    Phone,
}

impl ViewColumn {
    /// The header text: the field name, except computed columns which name
    /// the computation.
    pub fn header(&self) -> &str {
        match self.format {
            Some(ColumnFormat::Age) | Some(ColumnFormat::AgeYearsMonths) => "Age",
            _ => &self.field,
        }
    }
}

fn parse_column(spec: &str) -> ViewColumn {
    let spec = spec.trim();
    if let Some((function, rest)) = spec.split_once('(')
        && let Some(inner) = rest.trim().strip_suffix(')')
    {
        let mut parts = inner.split(',');
        let field = parts.next().unwrap_or_default().trim().to_string();
        let option = parts.next().map(|option| option.trim().to_string());
        let decimals = option
            .as_deref()
            .and_then(|decimals| decimals.parse().ok())
            .unwrap_or(0);
        if !field.is_empty() {
            match function.trim().to_ascii_uppercase().as_str() {
                "DOLLAR" => {
                    return ViewColumn {
                        field,
                        format: Some(ColumnFormat::Dollar(decimals)),
                    };
                }
                "PERCENT" => {
                    return ViewColumn {
                        field,
                        format: Some(ColumnFormat::Percent),
                    };
                }
                "AGE" => {
                    let format = match option.as_deref() {
                        Some(option) if option.eq_ignore_ascii_case("ym") => {
                            ColumnFormat::AgeYearsMonths
                        }
                        _ => ColumnFormat::Age,
                    };
                    return ViewColumn {
                        field,
                        format: Some(format),
                    };
                }
                "PHONE" => {
                    return ViewColumn {
                        field,
                        format: Some(ColumnFormat::Phone),
                    };
                }
                _ => {}
            }
        }
    }
    ViewColumn {
        field: spec.to_string(),
        format: None,
    }
}

/// Applies a column format to a raw field value; non-numeric values pass
/// through unchanged.
fn format_value(value: &str, format: ColumnFormat) -> String {
    if let ColumnFormat::Age = format {
        return match formulas::age_of(value) {
            Some(age) => format_number(age),
            None => String::new(),
        };
    }
    if let ColumnFormat::AgeYearsMonths = format {
        return match formulas::age_months_of(value) {
            Some(months) => format_tenure(months),
            None => String::new(),
        };
    }
    if let ColumnFormat::Phone = format {
        return formulas::format_phone(value);
    }
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|char| !matches!(char, ',' | '$' | '%'))
        .collect();
    let Ok(number) = cleaned.parse::<f64>() else {
        return value.to_string();
    };
    match format {
        ColumnFormat::Dollar(decimals) => formulas::format_dollar(number, decimals),
        ColumnFormat::Percent => format!("{}%", format_number(number)),
        // Handled above; a bare number has no age or phone shape.
        ColumnFormat::Age | ColumnFormat::AgeYearsMonths | ColumnFormat::Phone => {
            value.to_string()
        }
    }
}

/// "6 years, 3 months" from a month count; zero components are elided
/// ("12 years", "8 months") and a brand-new date reads "0 months".
fn format_tenure(months: f64) -> String {
    let months = months.round().max(0.0) as i64;
    let years = months / 12;
    let months = months % 12;
    let year_part = match years {
        0 => None,
        1 => Some("1 year".to_string()),
        years => Some(format!("{years} years")),
    };
    let month_part = match months {
        0 if years > 0 => None,
        1 => Some("1 month".to_string()),
        months => Some(format!("{months} months")),
    };
    match (year_part, month_part) {
        (Some(year), Some(month)) => format!("{year}, {month}"),
        (Some(year), None) => year,
        (None, Some(month)) => month,
        (None, None) => "0 months".to_string(),
    }
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        let formatted = format!("{number:.4}");
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Splits on commas that aren't inside parentheses, so
/// `Page, DOLLAR(Balance, 2)` yields two entries.
fn split_top_level_commas(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for char in value.chars() {
        match char {
            '(' => {
                depth += 1;
                current.push(char);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(char);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(char),
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// One record of a database: a page and its field index.
pub struct ViewRecord {
    pub page: String,
    pub fields: HashMap<String, String>,
}

/// Whether a record passes a `where:`/criteria condition. Fails open: a
/// broken condition keeps the record. Missing fields evaluate as empty.
pub fn record_matches(record: &ViewRecord, expression: &str) -> bool {
    let mut fields = HashMap::new();
    for (key, value) in &record.fields {
        fields.insert(formulas::col_key(key), value.clone());
    }
    fields.insert(formulas::col_key("Page"), record.page.clone());
    formulas::evaluate_view_condition(expression, &fields).unwrap_or(true)
}

/// Finds ```view blocks. Blocks inside other fenced code are ignored, so a
/// wiki page can document the syntax with an outer fence.
pub fn parse_view_blocks(source: &str) -> Vec<ViewBlock> {
    let mut blocks = Vec::new();
    let mut offset = 0;
    let mut in_other_fence = false;
    let mut current: Option<(usize, ViewBlock)> = None;
    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim();
        if let Some((start, block)) = current.as_mut() {
            if trimmed == "```" {
                let mut block = std::mem::replace(
                    block,
                    ViewBlock {
                        range: 0..0,
                        from: String::new(),
                        columns: Vec::new(),
                        where_expression: None,
                        sort: None,
                        summarize: Vec::new(),
                    },
                );
                block.range = *start..offset;
                if !block.from.is_empty() && !block.columns.is_empty() {
                    blocks.push(block);
                }
                current = None;
            } else if let Some((key, value)) = trimmed.split_once(':') {
                let value = value.trim();
                match key.trim().to_ascii_lowercase().as_str() {
                    "from" => block.from = value.to_string(),
                    "columns" => {
                        block.columns = split_top_level_commas(value)
                            .iter()
                            .map(|column| parse_column(column))
                            .collect();
                    }
                    "where" => block.where_expression = Some(value.to_string()),
                    "summarize" => {
                        block.summarize = value
                            .split(',')
                            .filter_map(|entry| {
                                let entry = entry.trim();
                                if entry.is_empty() {
                                    return None;
                                }
                                match entry.split_once('(') {
                                    Some((function, rest)) => {
                                        let column = rest.trim_end_matches(')').trim();
                                        Some((
                                            function.trim().to_ascii_uppercase(),
                                            Some(column.to_string()),
                                        ))
                                    }
                                    None => Some((entry.to_ascii_uppercase(), None)),
                                }
                            })
                            .collect();
                    }
                    "sort" => {
                        let (column, descending) =
                            match value.to_ascii_lowercase().strip_suffix(" desc") {
                                Some(_) => (value[..value.len() - " desc".len()].trim(), true),
                                None => match value.to_ascii_lowercase().strip_suffix(" asc") {
                                    Some(_) => (value[..value.len() - " asc".len()].trim(), false),
                                    None => (value, false),
                                },
                            };
                        if !column.is_empty() {
                            block.sort = Some((column.to_string(), descending));
                        }
                    }
                    _ => {}
                }
            }
        } else if trimmed == "```view" && !in_other_fence {
            current = Some((
                line_start,
                ViewBlock {
                    range: 0..0,
                    from: String::new(),
                    columns: Vec::new(),
                    where_expression: None,
                    sort: None,
                    summarize: Vec::new(),
                },
            ));
        } else if trimmed.starts_with("```") {
            in_other_fence = !in_other_fence;
        }
    }
    blocks
}

/// Filters, sorts, and renders a view's records as a markdown table.
pub fn render_view_table(block: &ViewBlock, mut records: Vec<ViewRecord>) -> String {
    if let Some(expression) = &block.where_expression {
        records.retain(|record| record_matches(record, expression));
    }

    if let Some((column, descending)) = &block.sort {
        let key = |record: &ViewRecord| -> String {
            if column.eq_ignore_ascii_case("page") {
                record.page.clone()
            } else {
                record
                    .fields
                    .get(&column.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_default()
            }
        };
        records.sort_by(|left, right| {
            markdown::compare_table_cell_text(&key(left), &key(right), *descending)
        });
    }

    let mut table = String::new();
    table.push('|');
    for column in &block.columns {
        table.push_str(&format!(" {} |", column.header()));
    }
    table.push_str("\n|");
    for _ in &block.columns {
        table.push_str("---|");
    }
    table.push('\n');
    for record in &records {
        table.push('|');
        for column in &block.columns {
            let mut value = if column.field.eq_ignore_ascii_case("page") {
                format!("[[{}]]", record.page)
            } else {
                record
                    .fields
                    .get(&column.field.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_default()
            };
            if let Some(format) = column.format {
                value = format_value(&value, format);
            }
            // A raw pipe in a value would split the generated row.
            let value = value.replace('|', "\\|");
            table.push_str(&format!(" {value} |"));
        }
        table.push('\n');
    }

    // Notion-style footer aggregates: a final row, each aggregate aligned
    // under its column (bare/unmatched ones land in the first column). The
    // `table-footer` marker tells the renderer to pin it — excluded from
    // click-sorting and filtering, always last.
    if !block.summarize.is_empty() && !block.columns.is_empty() {
        let mut cells: Vec<Vec<String>> = vec![Vec::new(); block.columns.len()];
        for (function, column) in &block.summarize {
            let target = column.as_ref().and_then(|column| {
                block.columns.iter().position(|candidate| {
                    candidate.field.eq_ignore_ascii_case(column)
                        || candidate.header().eq_ignore_ascii_case(column)
                })
            });
            match target {
                Some(position) => {
                    let view_column = &block.columns[position];
                    let raw_values = || -> Vec<String> {
                        records
                            .iter()
                            .map(|record| {
                                if view_column.field.eq_ignore_ascii_case("page") {
                                    record.page.clone()
                                } else {
                                    record
                                        .fields
                                        .get(&view_column.field.to_ascii_lowercase())
                                        .cloned()
                                        .unwrap_or_default()
                                }
                            })
                            .collect()
                    };
                    let value = match view_column.format {
                        // Aggregate over the column's displayed values, so
                        // MEDIAN(Age) is the median of ages rather than of
                        // raw dates; re-format the result where the format
                        // survives aggregation (SUM under DOLLAR is dollars,
                        // but a median of ages is already an age).
                        Some(ColumnFormat::AgeYearsMonths) => {
                            // Aggregate tenures as month counts, then render
                            // the result back as "6 years, 3 months".
                            let values: Vec<String> = raw_values()
                                .iter()
                                .map(|raw| {
                                    formulas::age_months_of(raw)
                                        .map(format_number)
                                        .unwrap_or_default()
                                })
                                .collect();
                            compute_summary_over(function, &values).map(|value| {
                                if function == "COUNT" {
                                    value
                                } else {
                                    value
                                        .parse::<f64>()
                                        .map(format_tenure)
                                        .unwrap_or(value)
                                }
                            })
                        }
                        Some(format) => {
                            let values: Vec<String> = raw_values()
                                .iter()
                                .map(|raw| format_value(raw, format))
                                .collect();
                            compute_summary_over(function, &values).map(|value| match format {
                                ColumnFormat::Dollar(_) | ColumnFormat::Percent => {
                                    format_value(&value, format)
                                }
                                ColumnFormat::Age
                                | ColumnFormat::AgeYearsMonths
                                | ColumnFormat::Phone => value,
                            })
                        }
                        None => compute_summary(function, Some(&view_column.field), &records),
                    };
                    let Some(value) = value else {
                        continue;
                    };
                    cells[position].push(format!("**{function}** {value}"));
                }
                None => {
                    let Some(value) = compute_summary(function, column.as_deref(), &records)
                    else {
                        continue;
                    };
                    let label = match column {
                        Some(column) => format!("{function} {column}"),
                        None => function.clone(),
                    };
                    cells[0].push(format!("**{label}** {value}"));
                }
            }
        }
        if cells.iter().any(|parts| !parts.is_empty()) {
            table.push('|');
            for parts in &cells {
                table.push_str(&format!(" {} |", parts.join(" · ")));
            }
            table.push('\n');
            return format!("<!-- table-footer: 1 -->\n{table}");
        }
    }
    table
}

/// One footer aggregate over the view's (filtered) records. Numeric functions
/// skip records whose value doesn't parse as a number (`$`, `,`, `%`
/// tolerated); `COUNT` without a column counts records, with a column it
/// counts non-empty values.
pub fn compute_summary(
    function: &str,
    column: Option<&str>,
    records: &[ViewRecord],
) -> Option<String> {
    if function == "COUNT" && column.is_none() {
        return Some(records.len().to_string());
    }
    let column = column?;
    let values: Vec<String> = records
        .iter()
        .map(|record| {
            if column.eq_ignore_ascii_case("page") {
                record.page.clone()
            } else {
                record
                    .fields
                    .get(&column.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_default()
            }
        })
        .collect();
    compute_summary_over(function, &values)
}

/// Aggregate over already-materialized cell values (used for formatted or
/// computed columns, where the displayed value — an age, a dollar amount —
/// is the thing to aggregate rather than the raw field).
pub fn compute_summary_over(function: &str, values: &[String]) -> Option<String> {
    match function {
        "COUNT" => Some(
            values
                .iter()
                .filter(|value| !value.is_empty())
                .count()
                .to_string(),
        ),
        "SUM" | "AVG" | "AVERAGE" | "MEDIAN" | "MIN" | "MAX" => {
            let mut numbers: Vec<f64> = values
                .iter()
                .filter_map(|value| {
                    let cleaned: String = value
                        .trim()
                        .chars()
                        .filter(|char| !matches!(char, ',' | '$' | '%'))
                        .collect();
                    cleaned.parse().ok()
                })
                .collect();
            if numbers.is_empty() {
                return Some("\u{2014}".to_string());
            }
            numbers.sort_by(|left, right| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            });
            let result = match function {
                "SUM" => numbers.iter().sum(),
                "AVG" | "AVERAGE" => numbers.iter().sum::<f64>() / numbers.len() as f64,
                "MIN" => numbers[0],
                "MAX" => numbers[numbers.len() - 1],
                "MEDIAN" => {
                    let middle = numbers.len() / 2;
                    if numbers.len() % 2 == 0 {
                        (numbers[middle - 1] + numbers[middle]) / 2.0
                    } else {
                        numbers[middle]
                    }
                }
                _ => unreachable!(),
            };
            Some(format_number(result))
        }
        _ => Some(format!("#ERROR: unknown aggregate {function}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(page: &str, pairs: &[(&str, &str)]) -> ViewRecord {
        ViewRecord {
            page: page.to_string(),
            fields: pairs
                .iter()
                .map(|(key, value)| (key.to_ascii_lowercase(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn parses_view_blocks() {
        let source = "# Title\n\n```view\nfrom: Credit cards\ncolumns: Page, Status, Credit limit\nwhere: COL(\"Status\") = \"Active\"\nsort: Credit limit desc\n```\n\ntext\n";
        let blocks = parse_view_blocks(source);
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.from, "Credit cards");
        assert_eq!(
            block
                .columns
                .iter()
                .map(|column| column.field.as_str())
                .collect::<Vec<_>>(),
            vec!["Page", "Status", "Credit limit"]
        );
        assert_eq!(
            block.where_expression.as_deref(),
            Some("COL(\"Status\") = \"Active\"")
        );
        assert_eq!(block.sort, Some(("Credit limit".to_string(), true)));
        assert_eq!(&source[block.range.clone()], "```view\nfrom: Credit cards\ncolumns: Page, Status, Credit limit\nwhere: COL(\"Status\") = \"Active\"\nsort: Credit limit desc\n```\n");
    }

    #[test]
    fn ignores_view_blocks_inside_other_fences() {
        let source = "````markdown\n```view\nfrom: X\ncolumns: Page\n```\n````\n";
        assert_eq!(parse_view_blocks(source).len(), 0);
    }

    #[test]
    fn renders_filtered_sorted_table() {
        let block = ViewBlock {
            range: 0..0,
            from: "Cards".into(),
            columns: vec![
                parse_column("Page"),
                parse_column("Status"),
                parse_column("DOLLAR(Credit limit)"),
            ],
            where_expression: Some("COL(\"Status\") <> \"Closed\"".into()),
            sort: Some(("Credit limit".into(), true)),
            summarize: vec![
                ("COUNT".to_string(), None),
                ("SUM".to_string(), Some("Credit limit".to_string())),
            ],
        };
        let records = vec![
            record("Card A", &[("Status", "Active"), ("Credit limit", "8000")]),
            record("Card B", &[("Status", "Closed"), ("Credit limit", "14300")]),
            record("Card C", &[("Status", "Active"), ("Credit limit", "12000")]),
        ];
        let table = render_view_table(&block, records);
        assert_eq!(
            table,
            "<!-- table-footer: 1 -->\n\
             | Page | Status | Credit limit |\n\
             |---|---|---|\n\
             | [[Card C]] | Active | $12,000 |\n\
             | [[Card A]] | Active | $8,000 |\n\
             | **COUNT** 2 |  | **SUM** $20,000 |\n"
        );
    }

    #[test]
    fn column_formats() {
        assert_eq!(
            parse_column("DOLLAR(Balance, 2)"),
            ViewColumn {
                field: "Balance".into(),
                format: Some(ColumnFormat::Dollar(2)),
            }
        );
        assert_eq!(format_value("21.49", ColumnFormat::Percent), "21.49%");
        let age: f64 = format_value("01/01/2000", ColumnFormat::Age).parse().unwrap();
        assert!(age >= 26.0, "{age}");
        assert_eq!(format_value("", ColumnFormat::Age), "");
        assert_eq!(parse_column("AGE(Date of birth)").header(), "Age");
        assert_eq!(format_value("12000", ColumnFormat::Dollar(0)), "$12,000");
        assert_eq!(format_value("—", ColumnFormat::Dollar(0)), "—");
        assert_eq!(
            split_top_level_commas("Page, DOLLAR(Balance, 2), PERCENT(APR)"),
            vec!["Page", "DOLLAR(Balance, 2)", "PERCENT(APR)"]
        );
    }

    #[test]
    fn tenure_format() {
        assert_eq!(format_tenure(75.0), "6 years, 3 months");
        assert_eq!(format_tenure(12.0), "1 year");
        assert_eq!(format_tenure(13.0), "1 year, 1 month");
        assert_eq!(format_tenure(8.0), "8 months");
        assert_eq!(format_tenure(0.0), "0 months");
        assert_eq!(format_tenure(24.0), "2 years");
        assert_eq!(
            parse_column("AGE(Opened, ym)"),
            ViewColumn {
                field: "Opened".to_string(),
                format: Some(ColumnFormat::AgeYearsMonths),
            }
        );
        assert_eq!(parse_column("AGE(Opened, ym)").header(), "Age");
    }

    #[test]
    fn summary_over_values() {
        let values = vec!["8".to_string(), "".to_string(), "3".to_string(), "13".to_string()];
        assert_eq!(compute_summary_over("MEDIAN", &values).unwrap(), "8");
        assert_eq!(compute_summary_over("COUNT", &values).unwrap(), "3");
        assert_eq!(compute_summary_over("AVG", &values).unwrap(), "8");
    }

    #[test]
    fn median_aggregate() {
        let records = vec![
            record("A", &[("APR", "21.49")]),
            record("B", &[("APR", "26.49%")]),
            record("C", &[("APR", "25.24")]),
        ];
        assert_eq!(
            compute_summary("MEDIAN", Some("APR"), &records),
            Some("25.24".to_string())
        );
        assert_eq!(compute_summary("COUNT", None, &records), Some("3".to_string()));
    }
}
