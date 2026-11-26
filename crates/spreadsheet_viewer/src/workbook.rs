use std::io::Cursor;

use anyhow::{Context as _, Result};
use calamine::{Data, ExcelDateTime, Range, Reader as _, SheetType};
use tabular_data_preview::types::{LineNumber, TableCell, TableLikeContent};
use ui::{SharedString, table_row::TableRow};

pub struct Workbook {
    pub sheets: Vec<Sheet>,
}

pub struct Sheet {
    pub name: SharedString,
    pub content: TableLikeContent,
}

impl Workbook {
    pub fn sheet_index(&self, name: &str) -> Option<usize> {
        self.sheets.iter().position(|sheet| sheet.name == name)
    }
}

/// Parses the bytes of an xlsx, xlsm, xlsb, xls or ods file. The format is detected from the
/// content rather than the file extension, so a misnamed file still opens.
pub fn parse_workbook(bytes: &[u8]) -> Result<Workbook> {
    let mut reader = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
        .context("Failed to read spreadsheet")?;

    // Chart sheets, dialog sheets and VBA modules have no cell grid to show.
    let worksheet_names = reader
        .sheets_metadata()
        .iter()
        .filter(|sheet| sheet.typ == SheetType::WorkSheet)
        .map(|sheet| sheet.name.clone())
        .collect::<Vec<_>>();

    let sheets = worksheet_names
        .into_iter()
        .map(|name| {
            let range = reader
                .worksheet_range(&name)
                .with_context(|| format!("Failed to read sheet {name:?}"))?;
            Ok(Sheet {
                name: name.into(),
                content: range_to_content(&range),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Workbook { sheets })
}

/// Converts the used range of a sheet into table content whose headers are spreadsheet column
/// letters and whose line numbers are the spreadsheet row numbers. A range that starts at `C3`
/// is shown from column `C` and row `3`, so positions match what a spreadsheet app shows.
fn range_to_content(range: &Range<Data>) -> TableLikeContent {
    let Some((first_row, first_column)) = range.start() else {
        return TableLikeContent::default();
    };
    let number_of_cols = range.width();

    let headers = (0..number_of_cols)
        .map(|offset| TableCell::Generated(column_name(first_column as usize + offset).into()))
        .collect();

    let rows = range
        .rows()
        .map(|row| {
            let cells = row
                .iter()
                .map(|value| TableCell::Generated(format_value(value)))
                .collect();
            TableRow::from_vec(cells, number_of_cols)
        })
        .collect::<Vec<_>>();

    let line_numbers = (0..rows.len())
        .map(|offset| LineNumber::Line(first_row as usize + offset + 1))
        .collect();

    TableLikeContent {
        number_of_cols,
        headers: TableRow::from_vec(headers, number_of_cols),
        rows,
        line_numbers,
    }
}

/// Returns the spreadsheet name of a zero-based column index: 0 is `A`, 25 is `Z`, 26 is `AA`.
pub fn column_name(index: usize) -> String {
    let mut name = Vec::new();
    let mut remaining = index + 1;
    while remaining > 0 {
        let letter_index = (remaining - 1) % 26;
        name.push(b'A' + letter_index as u8);
        remaining = (remaining - 1) / 26;
    }
    name.reverse();
    String::from_utf8_lossy(&name).into_owned()
}

fn format_value(value: &Data) -> SharedString {
    match value {
        Data::Empty => SharedString::default(),
        Data::String(text) => text.clone().into(),
        Data::Int(number) => number.to_string().into(),
        Data::Float(number) => number.to_string().into(),
        Data::Bool(true) => "TRUE".into(),
        Data::Bool(false) => "FALSE".into(),
        Data::Error(error) => error.to_string().into(),
        Data::DateTime(date_time) => format_date_time(date_time).into(),
        Data::DateTimeIso(text) | Data::DurationIso(text) => text.clone().into(),
    }
}

fn format_date_time(date_time: &ExcelDateTime) -> String {
    let serial = date_time.as_f64();
    if date_time.is_duration() {
        let total_seconds = (serial * 24.0 * 60.0 * 60.0).round() as i64;
        let sign = if total_seconds < 0 { "-" } else { "" };
        let total_seconds = total_seconds.abs();
        return format!(
            "{sign}{}:{:02}:{:02}",
            total_seconds / 3600,
            total_seconds / 60 % 60,
            total_seconds % 60
        );
    }

    let (year, month, day, hour, minute, second, _) = date_time.to_ymd_hms_milli();
    let date = format!("{year:04}-{month:02}-{day:02}");
    let time = format!("{hour:02}:{minute:02}:{second:02}");
    if serial.fract() == 0.0 {
        date
    } else if (0.0..1.0).contains(&serial) {
        time
    } else {
        format!("{date} {time}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XLSX: &[u8] = include_bytes!("../test_data/sample.xlsx");

    fn display_values(row: &TableRow<TableCell>) -> Vec<String> {
        row.as_slice()
            .iter()
            .map(|cell| {
                cell.display_value()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            })
            .collect()
    }

    fn line_numbers(content: &TableLikeContent) -> Vec<usize> {
        content
            .line_numbers
            .iter()
            .map(|line_number| match line_number {
                LineNumber::Line(line) => *line,
                LineNumber::LineRange(start, _) => *start,
            })
            .collect()
    }

    #[test]
    fn test_column_name() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }

    #[test]
    fn test_parse_sample_workbook() -> Result<()> {
        let workbook = parse_workbook(SAMPLE_XLSX)?;

        let names = workbook
            .sheets
            .iter()
            .map(|sheet| sheet.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Data", "Offset", "Empty"]);

        let data = &workbook.sheets[0].content;
        assert_eq!(data.number_of_cols, 5);
        assert_eq!(display_values(&data.headers), ["A", "B", "C", "D", "E"]);
        assert_eq!(
            data.rows.iter().map(display_values).collect::<Vec<_>>(),
            [
                ["Name", "Amount", "Joined", "Active", "Ratio"],
                ["Ada", "42", "2023-03-15", "TRUE", "#DIV/0!"],
                ["Grace", "3.5", "2023-03-15 12:00:00", "FALSE", ""],
                ["Overtime", "36:00:00", "", "", ""],
            ]
        );
        assert_eq!(line_numbers(data), [1, 2, 3, 4]);

        let offset = &workbook.sheets[1].content;
        assert_eq!(display_values(&offset.headers), ["C", "D"]);
        assert_eq!(
            offset.rows.iter().map(display_values).collect::<Vec<_>>(),
            [["x", ""], ["", "7"]]
        );
        assert_eq!(line_numbers(offset), [3, 4]);

        let empty = &workbook.sheets[2].content;
        assert_eq!(empty.number_of_cols, 0);
        assert!(empty.rows.is_empty());
        assert!(empty.line_numbers.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_invalid_bytes_fails() {
        assert!(parse_workbook(b"not a spreadsheet").is_err());
    }
}
