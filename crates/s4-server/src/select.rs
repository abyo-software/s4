//! S3 Select — server-side SQL filter on object body (v0.6 #41).
//!
//! Implements the [`SelectObjectContent`][aws-doc] surface as a small,
//! self-contained module. The primary entry point is [`run_select_csv`] /
//! [`run_select_jsonlines`] which take a SQL string and the in-memory body
//! bytes (the caller is responsible for fetching + decompressing +
//! decrypting the object — at the handler level we delegate to S4's
//! existing GET path so SSE-C / SSE-S4 / SSE-KMS / S4 codec all work
//! transparently).
//!
//! ## Supported SQL subset
//!
//! - `SELECT col1, col2 FROM s3object` — projection by header name when
//!   the CSV has a header line.
//! - `SELECT _1, _3 FROM s3object` — positional projection (1-based, AWS
//!   convention; `_1` is the leftmost column).
//! - `SELECT * FROM s3object` — all columns in input order.
//! - `WHERE col = 'value'`, `WHERE col > 100`, `WHERE col LIKE 'foo%'`.
//! - `AND` / `OR` / `NOT` boolean composition.
//! - String / integer / float literals.
//! - Equality / inequality (`=`, `<>`, `<`, `>`, `<=`, `>=`) and `LIKE`.
//!
//! ## Explicitly unsupported (rejected with [`SelectError::UnsupportedFeature`])
//!
//! - Aggregates (`COUNT`, `SUM`, `AVG`, …) and `GROUP BY` / `HAVING`.
//! - `JOIN` / subqueries.
//! - `ORDER BY` / `LIMIT` (Select-on-S3 streams in input order; aggregating
//!   would defeat the streaming model and is outside this v0.6 scope).
//! - Parquet input (Parquet decode is intentionally out of scope; CSV /
//!   JSON Lines are the v0.6 deliverables).
//!
//! ## Output framing
//!
//! [`EventStreamWriter`] emits the AWS event-stream binary protocol —
//! one `Records` frame per non-empty payload, an optional `Stats` frame,
//! and a terminating `End` frame. Each frame is
//! `[total_len BE u32][headers_len BE u32][prelude CRC32][headers][payload][message CRC32]`
//! per the [AWS appendix][aws-events]. The handler in `service.rs` feeds
//! the produced events into `s3s::dto::SelectObjectContentEventStream`,
//! which performs equivalent framing on the wire — `EventStreamWriter`
//! exists primarily so the **frame format itself** can be unit-tested and
//! asserted-on by the integration test without spinning up a full client.
//!
//! [aws-doc]: https://docs.aws.amazon.com/AmazonS3/latest/API/API_SelectObjectContent.html
//! [aws-events]: https://docs.aws.amazon.com/AmazonS3/latest/API/RESTSelectObjectAppendix.html

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, ObjectName, Query, Select, SelectItem, SetExpr,
    Statement, TableFactor, UnaryOperator, Value,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

// =====================================================================
// Errors
// =====================================================================

#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    #[error("SQL parse error: {0}")]
    Parse(String),
    #[error("unsupported SQL feature: {0}")]
    UnsupportedFeature(String),
    #[error("input format error: {0}")]
    InputFormat(String),
    #[error("row evaluation error: {0}")]
    RowEval(String),
}

// =====================================================================
// Input / output formats
// =====================================================================

#[derive(Debug, Clone)]
pub enum SelectInputFormat {
    Csv { has_header: bool, delimiter: char },
    JsonLines,
}

#[derive(Debug, Clone)]
pub enum SelectOutputFormat {
    Csv,
    Json,
}

// =====================================================================
// Parsed query
// =====================================================================

#[derive(Debug, Clone)]
pub struct SelectQuery {
    /// Raw sqlparser SELECT items, validated against the supported
    /// subset at parse time (no aggregates / window funcs / subqueries).
    pub projection: Vec<SelectItem>,
    pub where_clause: Option<Expr>,
    /// Typically the literal `s3object` (case-insensitive). Captured for
    /// completeness; the runtime ignores it because there's only ever one
    /// virtual table in a Select query.
    pub from_alias: String,
}

/// Parse and validate a S3 Select SQL expression.
///
/// Reject features that have no row-streaming semantics on a single
/// object: aggregates, GROUP BY, HAVING, JOIN, ORDER BY, LIMIT, DISTINCT.
pub fn parse_select(sql: &str) -> Result<SelectQuery, SelectError> {
    let dialect = GenericDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)
        .map_err(|e| SelectError::Parse(e.to_string()))?;
    if statements.len() != 1 {
        return Err(SelectError::Parse(format!(
            "expected exactly one statement, got {}",
            statements.len()
        )));
    }
    let stmt = statements.pop().expect("len == 1");
    let query = match stmt {
        Statement::Query(q) => *q,
        other => {
            return Err(SelectError::UnsupportedFeature(format!(
                "only SELECT statements are supported, got: {other:?}"
            )));
        }
    };
    let Query {
        body, order_by, limit, offset, fetch, locks, with, ..
    } = query;
    if with.is_some() {
        return Err(SelectError::UnsupportedFeature("CTE / WITH".into()));
    }
    if order_by.is_some() {
        return Err(SelectError::UnsupportedFeature("ORDER BY".into()));
    }
    if limit.is_some() {
        return Err(SelectError::UnsupportedFeature("LIMIT".into()));
    }
    if offset.is_some() {
        return Err(SelectError::UnsupportedFeature("OFFSET".into()));
    }
    if fetch.is_some() {
        return Err(SelectError::UnsupportedFeature("FETCH".into()));
    }
    if !locks.is_empty() {
        return Err(SelectError::UnsupportedFeature("FOR UPDATE / lock clauses".into()));
    }

    let select = match *body {
        SetExpr::Select(s) => *s,
        SetExpr::Query(_) => {
            return Err(SelectError::UnsupportedFeature("nested query".into()));
        }
        SetExpr::SetOperation { .. } => {
            return Err(SelectError::UnsupportedFeature("set operation (UNION/INTERSECT/EXCEPT)".into()));
        }
        other => {
            return Err(SelectError::UnsupportedFeature(format!("unsupported SetExpr: {other:?}")));
        }
    };

    let Select {
        distinct,
        top,
        projection,
        from,
        selection,
        group_by,
        having,
        named_window,
        qualify,
        cluster_by,
        distribute_by,
        sort_by,
        prewhere,
        connect_by,
        ..
    } = select;
    if distinct.is_some() {
        return Err(SelectError::UnsupportedFeature("DISTINCT".into()));
    }
    if top.is_some() {
        return Err(SelectError::UnsupportedFeature("TOP".into()));
    }
    if having.is_some() {
        return Err(SelectError::UnsupportedFeature("HAVING".into()));
    }
    if !named_window.is_empty() {
        return Err(SelectError::UnsupportedFeature("WINDOW".into()));
    }
    if qualify.is_some() {
        return Err(SelectError::UnsupportedFeature("QUALIFY".into()));
    }
    if !cluster_by.is_empty() || !distribute_by.is_empty() || !sort_by.is_empty() {
        return Err(SelectError::UnsupportedFeature(
            "CLUSTER BY / DISTRIBUTE BY / SORT BY".into(),
        ));
    }
    if prewhere.is_some() {
        return Err(SelectError::UnsupportedFeature("PREWHERE".into()));
    }
    if connect_by.is_some() {
        return Err(SelectError::UnsupportedFeature("CONNECT BY".into()));
    }
    match group_by {
        GroupByExpr::Expressions(ref exprs, ref mods) if exprs.is_empty() && mods.is_empty() => {}
        _ => return Err(SelectError::UnsupportedFeature("GROUP BY".into())),
    }

    // Validate projection — reject anything that requires a non-row-local
    // computation (function calls, subqueries, aggregates).
    for item in &projection {
        validate_projection_item(item)?;
    }
    if let Some(ref where_expr) = selection {
        validate_where_expr(where_expr)?;
    }

    // FROM must be a single table reference, optionally aliased.
    let from_alias = match from.as_slice() {
        [twj] if twj.joins.is_empty() => match &twj.relation {
            TableFactor::Table { name, alias, .. } => alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| object_name_to_string(name)),
            _ => {
                return Err(SelectError::UnsupportedFeature(
                    "only `FROM s3object` (or aliased single table) is supported".into(),
                ));
            }
        },
        [] => "s3object".to_owned(),
        _ => return Err(SelectError::UnsupportedFeature("JOIN / multiple FROM tables".into())),
    };

    Ok(SelectQuery {
        projection,
        where_clause: selection,
        from_alias,
    })
}

fn object_name_to_string(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn validate_projection_item(item: &SelectItem) -> Result<(), SelectError> {
    match item {
        SelectItem::Wildcard(_) => Ok(()),
        SelectItem::QualifiedWildcard(_, _) => Ok(()),
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
            validate_simple_column_expr(e)
        }
    }
}

fn validate_simple_column_expr(expr: &Expr) -> Result<(), SelectError> {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Ok(()),
        Expr::Function(_) => Err(SelectError::UnsupportedFeature(
            "aggregate / scalar function in projection (only bare column references supported)".into(),
        )),
        Expr::Subquery(_) | Expr::Exists { .. } => {
            Err(SelectError::UnsupportedFeature("subquery in projection".into()))
        }
        _ => Err(SelectError::UnsupportedFeature(format!(
            "unsupported projection expression: {expr}"
        ))),
    }
}

fn validate_where_expr(expr: &Expr) -> Result<(), SelectError> {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => Ok(()),
        Expr::Nested(inner) => validate_where_expr(inner),
        Expr::UnaryOp { op, expr } => match op {
            UnaryOperator::Not | UnaryOperator::Minus | UnaryOperator::Plus => {
                validate_where_expr(expr)
            }
            other => Err(SelectError::UnsupportedFeature(format!(
                "unsupported unary operator in WHERE: {other:?}"
            ))),
        },
        Expr::BinaryOp { op, left, right } => match op {
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::And
            | BinaryOperator::Or => {
                validate_where_expr(left)?;
                validate_where_expr(right)
            }
            other => Err(SelectError::UnsupportedFeature(format!(
                "unsupported binary operator in WHERE: {other:?}"
            ))),
        },
        Expr::Like { expr, pattern, .. } => {
            validate_where_expr(expr)?;
            validate_where_expr(pattern)
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => validate_where_expr(e),
        Expr::Function(_) => Err(SelectError::UnsupportedFeature(
            "function call in WHERE".into(),
        )),
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(SelectError::UnsupportedFeature("subquery in WHERE".into()))
        }
        other => Err(SelectError::UnsupportedFeature(format!(
            "unsupported WHERE expression: {other}"
        ))),
    }
}

// =====================================================================
// Row representation + lookup
// =====================================================================

/// CSV input row. Columns indexed by 0-based position OR by header name
/// (when the InputFormat says `has_header = true`).
pub struct CsvRow<'a> {
    pub fields: Vec<&'a str>,
    pub headers: Option<&'a [String]>,
}

impl CsvRow<'_> {
    /// Look up a column. AWS Select supports both bare `column_name` (when
    /// the CSV has a header) and `_1`, `_2`, ... positional refs. Returns
    /// `None` if the identifier doesn't resolve.
    #[must_use]
    pub fn get(&self, ident: &str) -> Option<&str> {
        if let Some(stripped) = ident.strip_prefix('_')
            && let Ok(n) = stripped.parse::<usize>()
            && n >= 1
        {
            return self.fields.get(n - 1).copied();
        }
        // Header-name lookup. AWS S3 Select treats column names
        // case-insensitively when matched against headers in the file.
        if let Some(headers) = self.headers {
            for (i, h) in headers.iter().enumerate() {
                if h.eq_ignore_ascii_case(ident) {
                    return self.fields.get(i).copied();
                }
            }
        }
        None
    }
}

// =====================================================================
// Row evaluation
// =====================================================================

/// Logical value used by the WHERE evaluator. We keep it intentionally
/// small — only the literal kinds the supported subset can produce.
#[derive(Debug, Clone)]
enum Lit<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(std::borrow::Cow<'a, str>),
}

impl<'a> Lit<'a> {
    fn from_str_value(s: &'a str) -> Lit<'a> {
        Lit::Str(std::borrow::Cow::Borrowed(s))
    }

    fn truthy(&self) -> bool {
        matches!(self, Lit::Bool(true))
    }
}

/// Apply WHERE + projection to a single row. Returns `Ok(Some(values))`
/// for matched rows (one `String` per `SELECT` item, in declaration
/// order), `Ok(None)` if WHERE excluded the row, `Err(...)` only on
/// runtime evaluation problems (a projected column not in the row, etc).
pub fn evaluate_row(
    query: &SelectQuery,
    row: &CsvRow<'_>,
) -> Result<Option<Vec<String>>, SelectError> {
    if let Some(ref w) = query.where_clause {
        let v = eval_expr(w, row)?;
        if !v.truthy() {
            return Ok(None);
        }
    }
    let mut out = Vec::with_capacity(query.projection.len());
    for item in &query.projection {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                for f in &row.fields {
                    out.push((*f).to_owned());
                }
            }
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                let ident = expr_as_column(e)?;
                let v = row.get(&ident).ok_or_else(|| {
                    SelectError::RowEval(format!("column not found: {ident}"))
                })?;
                out.push(v.to_owned());
            }
        }
    }
    Ok(Some(out))
}

fn expr_as_column(expr: &Expr) -> Result<String, SelectError> {
    match expr {
        Expr::Identifier(i) => Ok(i.value.clone()),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| p.value.clone())
            .ok_or_else(|| SelectError::RowEval("empty compound identifier".into())),
        other => Err(SelectError::UnsupportedFeature(format!(
            "non-column projection: {other}"
        ))),
    }
}

fn eval_expr<'a>(expr: &Expr, row: &'a CsvRow<'a>) -> Result<Lit<'a>, SelectError> {
    match expr {
        Expr::Nested(inner) => eval_expr(inner, row),
        Expr::Identifier(i) => Ok(row
            .get(&i.value)
            .map_or(Lit::Null, Lit::from_str_value)),
        Expr::CompoundIdentifier(parts) => {
            let last = parts
                .last()
                .ok_or_else(|| SelectError::RowEval("empty compound identifier".into()))?;
            Ok(row
                .get(&last.value)
                .map_or(Lit::Null, Lit::from_str_value))
        }
        Expr::Value(v) => value_to_lit(v),
        Expr::UnaryOp { op, expr } => {
            let v = eval_expr(expr, row)?;
            match op {
                UnaryOperator::Not => Ok(Lit::Bool(!v.truthy())),
                UnaryOperator::Minus => match v {
                    Lit::Int(n) => Ok(Lit::Int(-n)),
                    Lit::Float(f) => Ok(Lit::Float(-f)),
                    other => Err(SelectError::RowEval(format!(
                        "cannot negate non-numeric value: {other:?}"
                    ))),
                },
                UnaryOperator::Plus => Ok(v),
                other => Err(SelectError::UnsupportedFeature(format!(
                    "unsupported unary op: {other:?}"
                ))),
            }
        }
        Expr::BinaryOp { op, left, right } => {
            let l = eval_expr(left, row)?;
            let r = eval_expr(right, row)?;
            eval_binary(op, &l, &r)
        }
        Expr::Like { negated, expr, pattern, escape_char } => {
            if escape_char.is_some() {
                return Err(SelectError::UnsupportedFeature(
                    "LIKE ESCAPE clause".into(),
                ));
            }
            let s_val = eval_expr(expr, row)?;
            let p_val = eval_expr(pattern, row)?;
            let s = lit_as_str(&s_val);
            let p = lit_as_str(&p_val);
            let m = like_match(s.as_ref(), p.as_ref());
            Ok(Lit::Bool(if *negated { !m } else { m }))
        }
        Expr::IsNull(e) => Ok(Lit::Bool(matches!(eval_expr(e, row)?, Lit::Null))),
        Expr::IsNotNull(e) => Ok(Lit::Bool(!matches!(eval_expr(e, row)?, Lit::Null))),
        other => Err(SelectError::UnsupportedFeature(format!(
            "unsupported expression in WHERE: {other}"
        ))),
    }
}

fn value_to_lit<'a>(v: &Value) -> Result<Lit<'a>, SelectError> {
    match v {
        Value::Number(s, _) => {
            if let Ok(n) = s.parse::<i64>() {
                Ok(Lit::Int(n))
            } else if let Ok(f) = s.parse::<f64>() {
                Ok(Lit::Float(f))
            } else {
                Err(SelectError::RowEval(format!("invalid number literal: {s}")))
            }
        }
        Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => {
            Ok(Lit::Str(std::borrow::Cow::Owned(s.clone())))
        }
        Value::Boolean(b) => Ok(Lit::Bool(*b)),
        Value::Null => Ok(Lit::Null),
        other => Err(SelectError::UnsupportedFeature(format!(
            "literal kind not supported: {other:?}"
        ))),
    }
}

fn lit_as_str<'a>(v: &Lit<'a>) -> std::borrow::Cow<'a, str> {
    match v {
        Lit::Null => std::borrow::Cow::Borrowed(""),
        Lit::Bool(b) => std::borrow::Cow::Owned(if *b { "true" } else { "false" }.into()),
        Lit::Int(n) => std::borrow::Cow::Owned(n.to_string()),
        Lit::Float(f) => std::borrow::Cow::Owned(f.to_string()),
        Lit::Str(s) => s.clone(),
    }
}

fn lit_as_f64(v: &Lit<'_>) -> Option<f64> {
    match v {
        Lit::Int(n) => Some(*n as f64),
        Lit::Float(f) => Some(*f),
        Lit::Str(s) => s.parse::<f64>().ok(),
        Lit::Bool(_) | Lit::Null => None,
    }
}

fn eval_binary<'a>(
    op: &BinaryOperator,
    l: &Lit<'a>,
    r: &Lit<'a>,
) -> Result<Lit<'a>, SelectError> {
    use BinaryOperator::*;
    match op {
        And => Ok(Lit::Bool(l.truthy() && r.truthy())),
        Or => Ok(Lit::Bool(l.truthy() || r.truthy())),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            // NULLs propagate to NULL → not-truthy. AWS S3 Select uses the
            // SQL NULL semantics; we collapse to a Bool(false) so they
            // simply don't match.
            if matches!(l, Lit::Null) || matches!(r, Lit::Null) {
                return Ok(Lit::Bool(false));
            }
            // Try numeric comparison first when both sides parse as
            // numbers — covers `col > 100` against CSV string fields.
            let cmp = if let (Some(a), Some(b)) = (lit_as_f64(l), lit_as_f64(r)) {
                a.partial_cmp(&b)
            } else {
                let a = lit_as_str(l);
                let b = lit_as_str(r);
                Some(a.as_ref().cmp(b.as_ref()))
            };
            let ord =
                cmp.ok_or_else(|| SelectError::RowEval("incomparable values (NaN?)".into()))?;
            let res = match op {
                Eq => ord == std::cmp::Ordering::Equal,
                NotEq => ord != std::cmp::Ordering::Equal,
                Lt => ord == std::cmp::Ordering::Less,
                LtEq => ord != std::cmp::Ordering::Greater,
                Gt => ord == std::cmp::Ordering::Greater,
                GtEq => ord != std::cmp::Ordering::Less,
                _ => unreachable!("guarded by outer match"),
            };
            Ok(Lit::Bool(res))
        }
        other => Err(SelectError::UnsupportedFeature(format!(
            "unsupported binary operator: {other:?}"
        ))),
    }
}

/// SQL `LIKE` matcher. Supports `%` (any sequence) and `_` (any single
/// char). Anchored at both ends — `'foo%'` matches `"foobar"` but not
/// `"xfoobar"`.
fn like_match(s: &str, pattern: &str) -> bool {
    let s_bytes: Vec<char> = s.chars().collect();
    let p_bytes: Vec<char> = pattern.chars().collect();
    let (mut si, mut pi) = (0usize, 0usize);
    let (mut star, mut match_si) = (None::<usize>, 0usize);
    while si < s_bytes.len() {
        if pi < p_bytes.len() && (p_bytes[pi] == '_' || p_bytes[pi] == s_bytes[si]) {
            si += 1;
            pi += 1;
        } else if pi < p_bytes.len() && p_bytes[pi] == '%' {
            star = Some(pi);
            match_si = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            match_si += 1;
            si = match_si;
        } else {
            return false;
        }
    }
    while pi < p_bytes.len() && p_bytes[pi] == '%' {
        pi += 1;
    }
    pi == p_bytes.len()
}

// =====================================================================
// CSV / JSON Lines runners
// =====================================================================

/// Run a Select against a CSV-bytes body in-memory. Returns the
/// concatenated output bytes in `output` format (CSV: rfc4180 single CRLF
/// rows / JSON: one JSON-object-per-line).
pub fn run_select_csv(
    sql: &str,
    body: &[u8],
    input: SelectInputFormat,
    output: SelectOutputFormat,
) -> Result<Vec<u8>, SelectError> {
    let (has_header, delim) = match input {
        SelectInputFormat::Csv { has_header, delimiter } => (has_header, delimiter),
        SelectInputFormat::JsonLines => {
            return Err(SelectError::InputFormat(
                "run_select_csv called with JsonLines input — use run_select_jsonlines".into(),
            ));
        }
    };
    let query = parse_select(sql)?;

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .delimiter(delim as u8)
        .flexible(true)
        .from_reader(body);

    let headers_owned: Option<Vec<String>> = if has_header {
        let h = rdr
            .headers()
            .map_err(|e| SelectError::InputFormat(format!("CSV headers: {e}")))?
            .iter()
            .map(|s| s.to_owned())
            .collect();
        Some(h)
    } else {
        None
    };
    let header_slice: Option<&[String]> = headers_owned.as_deref();

    let mut out = Vec::with_capacity(body.len() / 2);
    for record in rdr.records() {
        let record = record
            .map_err(|e| SelectError::InputFormat(format!("CSV record: {e}")))?;
        let fields: Vec<&str> = record.iter().collect();
        let row = CsvRow {
            fields,
            headers: header_slice,
        };
        if let Some(values) = evaluate_row(&query, &row)? {
            write_output_row(&query, &values, &output, &mut out)?;
        }
    }
    Ok(out)
}

/// Run a Select against a JSON-Lines body (`{...}\n{...}\n...`). One row
/// per top-level JSON object. Nested values are stringified for CSV
/// output; for JSON output, the projected fields are re-emitted with
/// their original JSON literal.
pub fn run_select_jsonlines(
    sql: &str,
    body: &[u8],
    output: SelectOutputFormat,
) -> Result<Vec<u8>, SelectError> {
    let query = parse_select(sql)?;
    let text = std::str::from_utf8(body)
        .map_err(|e| SelectError::InputFormat(format!("body is not valid UTF-8: {e}")))?;
    let mut out = Vec::with_capacity(body.len() / 2);
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            SelectError::InputFormat(format!("JSON parse on line {}: {e}", lineno + 1))
        })?;
        let obj = v.as_object().ok_or_else(|| {
            SelectError::InputFormat(format!(
                "JSON Lines requires top-level object, line {} was not an object",
                lineno + 1
            ))
        })?;
        // Reify the object as ordered (header_name, value_str) pairs so
        // the existing CsvRow evaluator works against it.
        let headers: Vec<String> = obj.keys().cloned().collect();
        let raw_strs: Vec<String> = obj
            .values()
            .map(|jv| match jv {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let fields: Vec<&str> = raw_strs.iter().map(|s| s.as_str()).collect();
        let row = CsvRow {
            fields,
            headers: Some(headers.as_slice()),
        };
        if let Some(values) = evaluate_row(&query, &row)? {
            write_jsonlines_row(&query, &headers, &values, &output, &mut out)?;
        }
    }
    Ok(out)
}

fn write_output_row(
    query: &SelectQuery,
    values: &[String],
    output: &SelectOutputFormat,
    out: &mut Vec<u8>,
) -> Result<(), SelectError> {
    match output {
        SelectOutputFormat::Csv => {
            let mut wtr = csv::WriterBuilder::new()
                .terminator(csv::Terminator::CRLF)
                .from_writer(Vec::new());
            wtr.write_record(values.iter().map(String::as_str))
                .map_err(|e| SelectError::InputFormat(format!("CSV write: {e}")))?;
            wtr.flush()
                .map_err(|e| SelectError::InputFormat(format!("CSV flush: {e}")))?;
            let inner = wtr
                .into_inner()
                .map_err(|e| SelectError::InputFormat(format!("CSV finish: {e}")))?;
            out.extend_from_slice(&inner);
        }
        SelectOutputFormat::Json => {
            let names = projection_names(query, values.len());
            let mut map = serde_json::Map::with_capacity(values.len());
            for (n, v) in names.iter().zip(values.iter()) {
                map.insert(n.clone(), serde_json::Value::String(v.clone()));
            }
            let line = serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| SelectError::InputFormat(format!("JSON serialize: {e}")))?;
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    Ok(())
}

fn write_jsonlines_row(
    query: &SelectQuery,
    headers: &[String],
    values: &[String],
    output: &SelectOutputFormat,
    out: &mut Vec<u8>,
) -> Result<(), SelectError> {
    match output {
        SelectOutputFormat::Csv => write_output_row(query, values, output, out)?,
        SelectOutputFormat::Json => {
            let names = projection_names_with_headers(query, headers, values.len());
            let mut map = serde_json::Map::with_capacity(values.len());
            for (n, v) in names.iter().zip(values.iter()) {
                map.insert(n.clone(), serde_json::Value::String(v.clone()));
            }
            let line = serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| SelectError::InputFormat(format!("JSON serialize: {e}")))?;
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    Ok(())
}

fn projection_names(query: &SelectQuery, fallback_len: usize) -> Vec<String> {
    let mut names = Vec::with_capacity(fallback_len);
    for (i, item) in query.projection.iter().enumerate() {
        match item {
            SelectItem::ExprWithAlias { alias, .. } => names.push(alias.value.clone()),
            SelectItem::UnnamedExpr(e) => match expr_as_column(e) {
                Ok(s) => names.push(s),
                Err(_) => names.push(format!("_{}", i + 1)),
            },
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                for j in names.len()..fallback_len {
                    names.push(format!("_{}", j + 1));
                }
                return names;
            }
        }
    }
    while names.len() < fallback_len {
        let n = names.len();
        names.push(format!("_{}", n + 1));
    }
    names
}

fn projection_names_with_headers(
    query: &SelectQuery,
    headers: &[String],
    fallback_len: usize,
) -> Vec<String> {
    let mut names = Vec::with_capacity(fallback_len);
    for (i, item) in query.projection.iter().enumerate() {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                for h in headers {
                    names.push(h.clone());
                }
                while names.len() < fallback_len {
                    let n = names.len();
                    names.push(format!("_{}", n + 1));
                }
                return names;
            }
            SelectItem::ExprWithAlias { alias, .. } => names.push(alias.value.clone()),
            SelectItem::UnnamedExpr(e) => match expr_as_column(e) {
                Ok(s) => names.push(s),
                Err(_) => names.push(format!("_{}", i + 1)),
            },
        }
    }
    while names.len() < fallback_len {
        let n = names.len();
        names.push(format!("_{}", n + 1));
    }
    names
}

// =====================================================================
// AWS event-stream framing
// =====================================================================

/// Emits AWS event-stream binary frames for a Select response. Each frame
/// is `[total_len BE u32][headers_len BE u32][prelude CRC32][headers...][payload][message CRC32]`.
///
/// Header value type is fixed at `7` (UTF-8 string). Headers always
/// emitted: `:event-type`, `:message-type`, plus `:content-type` for
/// payload-bearing frames (Records / Stats).
#[derive(Debug, Default)]
pub struct EventStreamWriter {}

impl EventStreamWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Build a `Records` frame. `payload` is the (optionally empty) body
    /// chunk — typically a CSV / JSON-Lines slab of one or more output
    /// rows. AWS allows splitting a logical record across frames.
    pub fn records(&mut self, payload: &[u8]) -> Vec<u8> {
        build_frame(
            &[
                (":event-type", "Records"),
                (":content-type", "application/octet-stream"),
                (":message-type", "event"),
            ],
            Some(payload),
        )
    }

    /// Build a `Stats` frame containing the standard
    /// `BytesScanned` / `BytesProcessed` / `BytesReturned` XML payload.
    pub fn stats(&mut self, scanned: u64, processed: u64, returned: u64) -> Vec<u8> {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<Stats xmlns=\"\">\
<BytesScanned>{scanned}</BytesScanned>\
<BytesProcessed>{processed}</BytesProcessed>\
<BytesReturned>{returned}</BytesReturned>\
</Stats>"
        );
        build_frame(
            &[
                (":event-type", "Stats"),
                (":content-type", "text/xml"),
                (":message-type", "event"),
            ],
            Some(xml.as_bytes()),
        )
    }

    /// Build the terminating `End` frame. Clients must wait for this
    /// before assuming the response stream is complete.
    pub fn end(&mut self) -> Vec<u8> {
        build_frame(
            &[
                (":event-type", "End"),
                (":message-type", "event"),
            ],
            None,
        )
    }
}

fn build_frame(headers: &[(&str, &str)], payload: Option<&[u8]>) -> Vec<u8> {
    let mut header_buf: Vec<u8> = Vec::new();
    for (name, value) in headers {
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();
        debug_assert!(name_bytes.len() <= u8::MAX as usize, "header name too long");
        debug_assert!(value_bytes.len() <= u16::MAX as usize, "header value too long");
        header_buf.push(name_bytes.len() as u8);
        header_buf.extend_from_slice(name_bytes);
        header_buf.push(7); // value type 7 == UTF-8 string
        header_buf.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
        header_buf.extend_from_slice(value_bytes);
    }
    let payload_bytes = payload.unwrap_or(&[]);
    let headers_len: u32 = header_buf.len() as u32;
    let total_len: u32 = 12 + headers_len + payload_bytes.len() as u32 + 4;

    let mut buf: Vec<u8> = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = crc32fast::hash(&buf[..8]);
    buf.extend_from_slice(&prelude_crc.to_be_bytes());
    buf.extend_from_slice(&header_buf);
    buf.extend_from_slice(payload_bytes);
    let message_crc = crc32fast::hash(&buf[..buf.len()]);
    buf.extend_from_slice(&message_crc.to_be_bytes());
    buf
}

// =====================================================================
// GPU stub (v0.7+ scope marker)
// =====================================================================

/// GPU acceleration stub — always returns `None` today. The integration
/// test verifies it's wired but inactive; v0.7 will swap in an actual
/// CUDA WHERE-evaluator.
#[must_use]
pub fn select_gpu(
    _sql: &str,
    _body: &[u8],
    _input: &SelectInputFormat,
) -> Option<Vec<u8>> {
    None
}

// =====================================================================
// Unit tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn csv_input() -> SelectInputFormat {
        SelectInputFormat::Csv {
            has_header: true,
            delimiter: ',',
        }
    }

    #[test]
    fn parse_select_happy_path() {
        let q = parse_select("SELECT name, age FROM s3object WHERE age > 30").unwrap();
        assert_eq!(q.projection.len(), 2);
        assert!(q.where_clause.is_some());
        assert_eq!(q.from_alias.to_lowercase(), "s3object");
    }

    #[test]
    fn parse_select_rejects_group_by() {
        let err =
            parse_select("SELECT name, COUNT(*) FROM s3object GROUP BY name").unwrap_err();
        match err {
            SelectError::UnsupportedFeature(_) => {}
            other => panic!("expected UnsupportedFeature, got {other:?}"),
        }
    }

    #[test]
    fn parse_select_rejects_join() {
        let err = parse_select("SELECT a.x FROM s3object a JOIN other b ON a.id = b.id")
            .unwrap_err();
        assert!(matches!(err, SelectError::UnsupportedFeature(_)));
    }

    #[test]
    fn parse_select_rejects_order_by() {
        let err = parse_select("SELECT name FROM s3object ORDER BY name").unwrap_err();
        assert!(matches!(err, SelectError::UnsupportedFeature(_)));
    }

    #[test]
    fn evaluate_row_eq_match() {
        let q = parse_select("SELECT name FROM s3object WHERE name = 'alice'").unwrap();
        let headers = vec!["name".to_owned(), "age".to_owned()];
        let row = CsvRow {
            fields: vec!["alice", "30"],
            headers: Some(&headers),
        };
        let r = evaluate_row(&q, &row).unwrap();
        assert_eq!(r, Some(vec!["alice".to_owned()]));

        let row2 = CsvRow {
            fields: vec!["bob", "30"],
            headers: Some(&headers),
        };
        assert_eq!(evaluate_row(&q, &row2).unwrap(), None);
    }

    #[test]
    fn evaluate_row_int_compare() {
        let q = parse_select("SELECT age FROM s3object WHERE age > 100").unwrap();
        let headers = vec!["name".to_owned(), "age".to_owned()];
        let big = CsvRow {
            fields: vec!["x", "200"],
            headers: Some(&headers),
        };
        let small = CsvRow {
            fields: vec!["x", "50"],
            headers: Some(&headers),
        };
        assert!(evaluate_row(&q, &big).unwrap().is_some());
        assert!(evaluate_row(&q, &small).unwrap().is_none());
    }

    #[test]
    fn evaluate_row_like_pattern() {
        let q = parse_select("SELECT name FROM s3object WHERE name LIKE 'foo%'").unwrap();
        let headers = vec!["name".to_owned()];
        let yes = CsvRow {
            fields: vec!["foobar"],
            headers: Some(&headers),
        };
        let no = CsvRow {
            fields: vec!["xfoobar"],
            headers: Some(&headers),
        };
        assert!(evaluate_row(&q, &yes).unwrap().is_some());
        assert!(evaluate_row(&q, &no).unwrap().is_none());
    }

    #[test]
    fn run_select_csv_end_to_end_filters_rows() {
        let body = b"name,age\nalice,30\nbob,40\ncarol,50\n";
        let out = run_select_csv(
            "SELECT name FROM s3object WHERE age > 35",
            body,
            csv_input(),
            SelectOutputFormat::Csv,
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        let lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["bob", "carol"]);
    }

    #[test]
    fn run_select_jsonlines_filter() {
        let body = b"{\"name\":\"alice\",\"age\":\"30\"}\n\
                     {\"name\":\"bob\",\"age\":\"40\"}\n\
                     {\"name\":\"carol\",\"age\":\"50\"}\n";
        let out = run_select_jsonlines(
            "SELECT name FROM s3object WHERE age > 35",
            body,
            SelectOutputFormat::Json,
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        let lines: Vec<&str> = s.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("bob"));
        assert!(lines[1].contains("carol"));
    }

    #[test]
    fn positional_column_ref() {
        let body = b"alice,30\nbob,40\n";
        let out = run_select_csv(
            "SELECT _1 FROM s3object WHERE _2 > 35",
            body,
            SelectInputFormat::Csv {
                has_header: false,
                delimiter: ',',
            },
            SelectOutputFormat::Csv,
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        let lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["bob"]);
    }

    #[test]
    fn and_or_combination() {
        let body = b"name,age,city\n\
                     alice,30,nyc\n\
                     bob,40,nyc\n\
                     carol,50,sf\n\
                     dan,25,sf\n";
        let out = run_select_csv(
            "SELECT name FROM s3object WHERE (city = 'nyc' AND age > 35) OR name = 'dan'",
            body,
            csv_input(),
            SelectOutputFormat::Csv,
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        let mut lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["bob", "dan"]);
    }

    #[test]
    fn event_stream_records_frame_format() {
        let mut w = EventStreamWriter::new();
        let frame = w.records(b"hello,world\r\n");
        let total =
            u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(total, frame.len());
        let headers_len =
            u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
        let prelude_crc =
            u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
        assert_eq!(prelude_crc, crc32fast::hash(&frame[..8]));
        let msg_crc = u32::from_be_bytes([
            frame[total - 4],
            frame[total - 3],
            frame[total - 2],
            frame[total - 1],
        ]);
        assert_eq!(msg_crc, crc32fast::hash(&frame[..total - 4]));
        let hdr_region = &frame[12..12 + headers_len];
        let s = String::from_utf8_lossy(hdr_region);
        assert!(s.contains(":event-type"));
        assert!(s.contains("Records"));
        let payload = &frame[12 + headers_len..total - 4];
        assert_eq!(payload, b"hello,world\r\n");
    }

    #[test]
    fn event_stream_end_frame_no_payload() {
        let mut w = EventStreamWriter::new();
        let frame = w.end();
        let total =
            u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        let headers_len =
            u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
        assert_eq!(total - 4 - 12 - headers_len, 0);
        let s = String::from_utf8_lossy(&frame[12..12 + headers_len]);
        assert!(s.contains("End"));
    }

    #[test]
    fn event_stream_stats_xml_payload() {
        let mut w = EventStreamWriter::new();
        let frame = w.stats(1024, 800, 64);
        let total =
            u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        let headers_len =
            u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
        let payload = &frame[12 + headers_len..total - 4];
        let xml = std::str::from_utf8(payload).unwrap();
        assert!(xml.contains("<BytesScanned>1024</BytesScanned>"));
        assert!(xml.contains("<BytesProcessed>800</BytesProcessed>"));
        assert!(xml.contains("<BytesReturned>64</BytesReturned>"));
    }

    #[test]
    fn gpu_stub_returns_none() {
        let v = select_gpu(
            "SELECT * FROM s3object",
            b"name,age\nalice,30\n",
            &csv_input(),
        );
        assert!(v.is_none(), "GPU stub must always return None for v0.6");
    }

    #[test]
    fn like_match_basics() {
        assert!(like_match("foobar", "foo%"));
        assert!(!like_match("xfoobar", "foo%"));
        assert!(like_match("abc", "_b_"));
        assert!(like_match("anything", "%"));
        assert!(like_match("", ""));
        assert!(!like_match("a", ""));
    }
}
