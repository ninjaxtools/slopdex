//! Human-readable inspection and lossless CSV/JSON exports.
use super::*;
use crate::cli::args::Detail;

pub(in crate::cli) fn print_indexes(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    if format == Format::Csv {
        return csv(
            out,
            rows,
            &[
                "name",
                "entryCount",
                "vectorCount",
                "pendingCount",
                "exists",
                "state",
            ],
        );
    }
    writeln!(
        out,
        "{:<16} {:>9} {:>9} {:>9}  State",
        "Index", "Entries", "Vectors", "Pending"
    )?;
    for row in rows {
        writeln!(
            out,
            "{:<16} {:>9} {:>9} {:>9}  {}",
            text(row, "name"),
            row["entryCount"],
            row["vectorCount"],
            row["pendingCount"],
            text(row, "state")
        )?;
    }
    Ok(())
}

pub(in crate::cli) fn print_index_entries(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    detail: Detail,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    if format == Format::Csv {
        return csv(
            out,
            rows,
            &[
                "rank",
                "order",
                "index",
                "id",
                "kind",
                "path",
                "name",
                "similarity",
                "text",
                "data",
                "embeddings",
            ],
        );
    }
    if rows.is_empty() {
        writeln!(out, "No entries.")?;
    }
    for (position, row) in rows.iter().enumerate() {
        if detail == Detail::Expanded && position > 0 {
            writeln!(out)?;
        }
        let score = row["similarity"]
            .as_f64()
            .map(|score| format!(" score={score:.6}"))
            .unwrap_or_default();
        let location = row["path"]
            .as_str()
            .map(|path| format!(" ({path})"))
            .unwrap_or_default();
        let heading = format!(
            "{}. {}{}{} [{} #{}]",
            row["rank"],
            text(row, "name"),
            location,
            score,
            text(row, "index"),
            text(row, "id")
        );
        if detail != Detail::Expanded {
            let state = if row["data"]["valid"] == false {
                " [stale]"
            } else if array(&row["embeddings"])
                .iter()
                .any(|embedding| embedding["present"] == false)
            {
                " [pending]"
            } else {
                ""
            };
            writeln!(
                out,
                "{}{} — {}",
                concise(&heading, 120),
                state,
                concise(
                    row["data"]["answer"]
                        .as_str()
                        .unwrap_or_else(|| text(row, "text")),
                    160
                )
            )?;
            continue;
        }
        writeln!(
            out,
            "{heading}\n  order={} kind={}{}",
            row["order"],
            text(row, "kind"),
            row["path"]
                .as_str()
                .map(|path| format!(" path={path}"))
                .unwrap_or_default()
        )?;
        for embedding in array(&row["embeddings"]) {
            writeln!(
                out,
                "  {}: {} ({} dimensions; present={}; compatible={})",
                text(embedding, "role"),
                text(embedding, "key"),
                embedding["dimensions"],
                embedding["present"],
                embedding["compatible"]
            )?;
        }
        writeln!(
            out,
            "\n{}\n\nMetadata\n{}",
            text(row, "text"),
            serde_json::to_string_pretty(&row["data"])?
        )?;
    }
    Ok(())
}

fn concise(input: &str, limit: usize) -> String {
    let flat = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit - 1).collect::<String>())
    }
}

fn csv(out: &mut impl Write, rows: &[Value], columns: &[&str]) -> Result<()> {
    csv_row(out, columns.iter().copied())?;
    for row in rows {
        let fields: Vec<_> = columns
            .iter()
            .map(|key| match &row[*key] {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                value => value.to_string(),
            })
            .collect();
        csv_row(out, fields.iter().map(String::as_str))?;
    }
    Ok(())
}

fn csv_row<'a>(out: &mut impl Write, fields: impl Iterator<Item = &'a str>) -> Result<()> {
    for (index, field) in fields.enumerate() {
        if index > 0 {
            write!(out, ",")?;
        }
        if field.contains([',', '"', '\r', '\n']) {
            write!(out, "\"{}\"", field.replace('"', "\"\""))?;
        } else {
            write!(out, "{field}")?;
        }
    }
    write!(out, "\r\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_multiline_unicode_json_and_empty_fields_without_truncation() -> Result<()> {
        let mut out = Vec::new();
        let long = format!("unicode 📖, \"quoted\"\r\n{}", "x".repeat(500));
        csv(
            &mut out,
            &[serde_json::json!({"name":long,"data":{"text":"a,b"},"empty":null})],
            &["name", "data", "empty"],
        )?;
        let text = String::from_utf8(out)?;
        assert!(text.starts_with("name,data,empty\r\n\"unicode 📖, \"\"quoted\"\"\r\n"));
        assert!(text.contains(&"x".repeat(500)));
        assert!(text.ends_with(",\"{\"\"text\"\":\"\"a,b\"\"}\",\r\n"));
        assert_eq!(concise("📖".repeat(200).as_str(), 10).chars().count(), 10);
        Ok(())
    }
}
