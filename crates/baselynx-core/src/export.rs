//! CSV and XLSX export in constant memory.

use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

use rust_xlsxwriter::{Format, Workbook};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::exec::{self, ColumnMeta, ExecEvent, Row};
use crate::model::ConnectionConfig;
use crate::runtime;
use crate::session;

pub const XLSX_MAX_DATA_ROWS: u64 = 1_048_575;
pub const XLSX_MAX_CELL_CHARS: usize = 32_767;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Xlsx,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Xlsx => "xlsx",
        }
    }
}

pub enum ExportSource {
    /// What the grid holds.
    Loaded { columns: Vec<ColumnMeta>, rows: Vec<Row> },
    /// Streamed on a new session.
    Query {
        connection: Box<ConnectionConfig>,
        password: Option<String>,
        sql: String,
        columns: Vec<ColumnMeta>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub rows: u64,
    pub path: PathBuf,
    /// XLSX cells cut to the Excel limit.
    pub truncated_cells: u64,
}

/// Exports to `path` through `<path>.part`, renamed on success and removed on failure.
pub fn export(
    source: ExportSource,
    format: ExportFormat,
    path: PathBuf,
    sheet_name: String,
    progress: watch::Sender<u64>,
    cancel: CancellationToken,
) -> impl Future<Output = Result<ExportReport>> + Send + 'static {
    runtime::spawn(async move {
        let (tx, rx) = mpsc::channel::<Msg>(4);
        let producer = match source {
            ExportSource::Loaded { columns, rows } => tokio::spawn(async move {
                if tx.send(Msg::Columns(columns)).await.is_err() {
                    return Ok(());
                }
                for chunk in rows.chunks(exec::BATCH_ROWS) {
                    if tx.send(Msg::Rows(chunk.to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok::<(), CoreError>(())
            }),
            ExportSource::Query {
                connection,
                password,
                sql,
                columns,
            } => {
                let session = session::connect(&connection, password).await?;
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let mut events = exec::execute(&session, sql, None);
                    loop {
                        let ev = tokio::select! {
                            _ = cancel.cancelled() => { let _ = session.cancel().await; return Err(CoreError::Cancelled) }
                            ev = events.recv() => ev,
                        };
                        match ev {
                            None => return Ok(()),
                            Some(Err(e)) => return Err(e),
                            Some(Ok(ev)) => {
                                let msg = match ev {
                                    ExecEvent::Rows(rows) => Msg::Rows(rows),
                                    ExecEvent::Columns(found) => Msg::Columns(merge_columns(found, &columns)),
                                    ExecEvent::Done { .. } => continue,
                                };
                                if tx.send(msg).await.is_err() {
                                    let _ = session.cancel().await;
                                    return Ok(());
                                }
                            }
                        }
                    }
                })
            }
        };

        let part = part_path(&path);
        let writer_cancel = cancel.clone();
        let writer_part = part.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut rx = rx;
            // The first message names the columns; nothing at all means the producer failed.
            let columns = match rx.blocking_recv() {
                Some(Msg::Columns(c)) => c,
                Some(Msg::Rows(_)) => return Err(CoreError::Internal("rows before columns".into())),
                None => Vec::new(),
            };
            let rows = RowStream { rx };
            match format {
                ExportFormat::Csv => write_csv(&writer_part, &columns, rows, &progress, &writer_cancel),
                ExportFormat::Xlsx => write_xlsx(&writer_part, &columns, &sheet_name, rows, &progress, &writer_cancel),
            }
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))?;

        let produced = producer.await.map_err(|e| CoreError::Internal(e.to_string()))?;
        let result = match (written, produced) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(_), Ok(())) if cancel.is_cancelled() => Err(CoreError::Cancelled),
            (Ok((rows, truncated_cells)), Ok(())) => Ok((rows, truncated_cells)),
        };
        match result {
            Ok((rows, truncated_cells)) => {
                fs::rename(&part, &path).map_err(|e| {
                    let _ = fs::remove_file(&part);
                    CoreError::Export(format!("cannot write {}: {e}", path.display()))
                })?;
                Ok(ExportReport {
                    rows,
                    path,
                    truncated_cells,
                })
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e)
            }
        }
    })
}

enum Msg {
    Columns(Vec<ColumnMeta>),
    Rows(Vec<Row>),
}

/// Rows after the column message.
struct RowStream {
    rx: mpsc::Receiver<Msg>,
}

impl RowStream {
    fn next_batch(&mut self) -> Option<Vec<Row>> {
        loop {
            match self.rx.blocking_recv()? {
                Msg::Rows(r) => return Some(r),
                Msg::Columns(_) => continue,
            }
        }
    }
}

/// Names come from the result; types from the caller's list when it describes the same
/// columns, otherwise from the prepared statement.
fn merge_columns(found: Vec<ColumnMeta>, given: &[ColumnMeta]) -> Vec<ColumnMeta> {
    found
        .into_iter()
        .enumerate()
        .map(|(i, mut c)| {
            if let Some(g) = given.get(i).filter(|g| g.name == c.name && g.type_oid.is_some()) {
                c.type_oid = g.type_oid;
                c.type_name = g.type_name.clone();
            }
            c
        })
        .collect()
}

fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn write_csv(
    part: &Path,
    columns: &[ColumnMeta],
    mut rows_in: RowStream,
    progress: &watch::Sender<u64>,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let file =
        fs::File::create(part).map_err(|e| CoreError::Export(format!("cannot create {}: {e}", part.display())))?;
    let mut buf = std::io::BufWriter::with_capacity(1 << 16, file);
    // A byte-order mark makes Excel read the file as UTF-8.
    buf.write_all(b"\xEF\xBB\xBF")?;
    let mut w = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .quote_style(csv::QuoteStyle::Necessary)
        .from_writer(buf);
    let csv_err = |e: csv::Error| CoreError::Export(e.to_string());
    w.write_record(columns.iter().map(|c| c.name.as_str()))
        .map_err(csv_err)?;
    let mut rows = 0u64;
    while let Some(batch) = rows_in.next_batch() {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        for row in batch {
            w.write_record(row.iter().map(|v| v.as_deref().unwrap_or("")))
                .map_err(csv_err)?;
            rows += 1;
        }
        progress.send_replace(rows);
    }
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    w.flush()?;
    Ok((rows, 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Number,
    BigNumber,
    Bool,
    Text,
}

fn cell_kind(oid: Option<u32>) -> CellKind {
    match oid {
        Some(21 | 23 | 700 | 701) => CellKind::Number,
        Some(20 | 1700) => CellKind::BigNumber,
        Some(16) => CellKind::Bool,
        _ => CellKind::Text,
    }
}

/// Significant digits of a plain decimal string, or `None` if it is not one.
fn significant_digits(s: &str) -> Option<usize> {
    let s = s.strip_prefix('-').or_else(|| s.strip_prefix('+')).unwrap_or(s);
    if s.is_empty()
        || !s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || s.bytes().filter(|&b| b == b'.').count() > 1
    {
        return None;
    }
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let digits = if int.is_empty() {
        frac.trim_start_matches('0').len()
    } else {
        int.len() + frac.len()
    };
    Some(digits.max(1))
}

fn sanitize_sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('\'').trim();
    let mut out: String = cleaned.chars().take(31).collect();
    if out.is_empty() {
        out = "Result".into();
    }
    out
}

fn write_xlsx(
    part: &Path,
    columns: &[ColumnMeta],
    sheet_name: &str,
    mut rows_in: RowStream,
    progress: &watch::Sender<u64>,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let xerr = |e: rust_xlsxwriter::XlsxError| CoreError::Export(e.to_string());
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet_with_constant_memory().map_err(xerr)?;
    sheet.set_name(sanitize_sheet_name(sheet_name)).map_err(xerr)?;
    let bold = Format::new().set_bold();
    for (c, col) in columns.iter().enumerate() {
        sheet
            .write_string_with_format(0, c as u16, &col.name, &bold)
            .map_err(xerr)?;
    }
    sheet.set_freeze_panes(1, 0).map_err(xerr)?;
    let kinds: Vec<CellKind> = columns.iter().map(|c| cell_kind(c.type_oid)).collect();
    let mut rows = 0u64;
    let mut truncated_cells = 0u64;
    while let Some(batch) = rows_in.next_batch() {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        for row in batch {
            if rows >= XLSX_MAX_DATA_ROWS {
                return Err(CoreError::Export(format!(
                    "The data has more than {XLSX_MAX_DATA_ROWS} rows, the Excel limit. Export to CSV instead."
                )));
            }
            let r = (rows + 1) as u32;
            for (c, value) in row.iter().enumerate() {
                let Some(v) = value else { continue };
                let col = c as u16;
                let kind = kinds.get(c).copied().unwrap_or(CellKind::Text);
                let as_number = match kind {
                    CellKind::Number => v.parse::<f64>().ok().filter(|n| n.is_finite()),
                    CellKind::BigNumber => significant_digits(v)
                        .filter(|&d| d <= 15)
                        .and_then(|_| v.parse::<f64>().ok()),
                    _ => None,
                };
                if let Some(n) = as_number {
                    sheet.write_number(r, col, n).map_err(xerr)?;
                } else if kind == CellKind::Bool && (v == "t" || v == "f") {
                    sheet.write_boolean(r, col, v == "t").map_err(xerr)?;
                } else if v.chars().count() > XLSX_MAX_CELL_CHARS {
                    let cut: String = v.chars().take(XLSX_MAX_CELL_CHARS).collect();
                    sheet.write_string(r, col, &cut).map_err(xerr)?;
                    truncated_cells += 1;
                } else {
                    sheet.write_string(r, col, v).map_err(xerr)?;
                }
            }
            rows += 1;
        }
        progress.send_replace(rows);
    }
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    workbook.save(part).map_err(xerr)?;
    Ok((rows, truncated_cells))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits() {
        assert_eq!(significant_digits("123"), Some(3));
        assert_eq!(significant_digits("-0.00120"), Some(2));
        assert_eq!(significant_digits("12345678901234.5"), Some(15));
        assert_eq!(significant_digits("1234567890123456"), Some(16));
        assert_eq!(significant_digits("0"), Some(1));
        assert_eq!(significant_digits("NaN"), None);
        assert_eq!(significant_digits("1e5"), None);
    }

    #[test]
    fn sheet_names() {
        assert_eq!(sanitize_sheet_name("a/b[c]"), "a_b_c_");
        assert_eq!(sanitize_sheet_name(""), "Result");
        assert_eq!(sanitize_sheet_name(&"x".repeat(40)).len(), 31);
    }
}
