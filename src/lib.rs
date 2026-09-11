#![forbid(unsafe_code)]

//! CSV: records of fields, one part per record. The shape walks the content
//! as RFC 4180 records through the Foundation's [`record`] walk — a field
//! quoted when it carries the separator, a line break or a quote — and
//! hands back one part per record, named by its number in the content,
//! with the record's bytes as written. The first record is named `header`
//! when the media type says `header=present`, or when no media type says
//! and the record reads as a header: every field non-empty, none a number,
//! no two the same. No type is announced, because a CSV says nothing about
//! itself (ADR-0047).
//!
//! The separator is the one the shape was built with, else the one the
//! first line uses most among comma, semicolon, tab and pipe. Nothing is
//! decoded and no field count is enforced: a shape sections and names, a
//! contract validates. What cannot be sectioned — a quote never closed,
//! content after a closing quote, bytes that are not text — is refused with
//! the byte where the walk stopped.

use message::record::{self, Delimited, Record};
use message::{Part, Shape, ShapeError, Shaped};
use stream::Stream;

/// The CSV shape.
#[derive(Clone, Copy, Debug, Default)]
pub struct Csv {
    separator: Option<u8>,
}

impl Csv {
    /// A shape whose separator is known rather than sniffed.
    #[must_use]
    pub const fn separated_by(separator: u8) -> Self {
        Self {
            separator: Some(separator),
        }
    }

    fn delimited(self, bytes: &[u8]) -> Delimited {
        self.separator
            .map(Delimited::separated_by)
            .or_else(|| Delimited::sniff(bytes))
            .unwrap_or_default()
    }
}

/// The `name` parameter of a media type, unquoted.
fn parameter<'a>(media: &'a str, name: &str) -> Option<&'a str> {
    media.split(';').skip(1).find_map(|parameter| {
        let (key, value) = parameter.split_once('=')?;
        let value = value.trim();
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim_matches('"'))
    })
}

/// Whether the field's text is a number: an optional sign, digits, at most
/// one point among them.
fn is_number(text: &[u8]) -> bool {
    let text = text.trim_ascii();
    let digits = text
        .strip_prefix(b"-")
        .or_else(|| text.strip_prefix(b"+"))
        .unwrap_or(text);
    let points = digits.split(|b| *b == b'.').count() - 1;
    !digits.is_empty()
        && points <= 1
        && digits.iter().any(u8::is_ascii_digit)
        && digits.iter().all(|b| b.is_ascii_digit() || *b == b'.')
}

/// Whether the first record reads as a header: every field non-empty, none
/// a number, no two the same.
fn reads_as_header(delimited: Delimited, bytes: &[u8], first: &Record) -> bool {
    let fields: Vec<Vec<u8>> = first
        .fields
        .iter()
        .map(|range| delimited.unquote(&bytes[range.clone()]))
        .collect();
    let sound = fields
        .iter()
        .all(|field| !field.trim_ascii().is_empty() && !is_number(field));
    let distinct = fields
        .iter()
        .enumerate()
        .all(|(i, field)| !fields[..i].contains(field));
    sound && distinct
}

impl Shape for Csv {
    fn technology(&self) -> &'static str {
        "csv"
    }

    fn media_types(&self) -> &'static [&'static str] {
        &["text/csv", "text/tab-separated-values", "application/csv"]
    }

    fn recognises(&self, bytes: &[u8]) -> bool {
        if record::text(bytes).is_err() {
            return false;
        }
        let Some(delimited) = Delimited::sniff(bytes) else {
            return false;
        };
        let mut records = delimited.records(bytes).take(2);
        let Some(Ok(first)) = records.next() else {
            return false;
        };
        let second = records.next();
        first.fields.len() >= 2
            && match second {
                Some(Ok(second)) => second.fields.len() == first.fields.len(),
                Some(Err(_)) => false,
                None => true,
            }
    }

    fn shape(&self, stream: &Stream) -> Result<Shaped, ShapeError> {
        let bytes = stream.bytes();
        let refused = |(reason, at): (&str, usize)| ShapeError::new("csv", reason).at(at);
        record::text(bytes).map_err(refused)?;
        if bytes.is_empty() {
            return Err(refused(("no records", 0)));
        }
        let delimited = self.delimited(bytes);
        let records: Vec<Record> = delimited
            .records(bytes)
            .collect::<Result<_, _>>()
            .map_err(refused)?;
        let media = stream
            .media_type()
            .map_or_else(|| "text/csv".to_string(), str::to_string);
        let header = match stream.media_type().and_then(|m| parameter(m, "header")) {
            Some(said) => said.eq_ignore_ascii_case("present"),
            None => reads_as_header(delimited, bytes, &records[0]),
        };
        let parts = records
            .iter()
            .map(|record| {
                let name = if header && record.number == 1 {
                    "header".to_string()
                } else {
                    record.number.to_string()
                };
                Part::new(
                    Some(name),
                    &bytes[record.range.clone()],
                    Some(media.clone()),
                )
            })
            .collect();
        Ok(Shaped {
            parts,
            message_type: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcore::StreamId;

    fn stream(bytes: &[u8], media: Option<&str>) -> Stream {
        Stream::new(StreamId::new(1), bytes.to_vec(), media.map(str::to_string))
    }

    #[test]
    fn records_are_numbered_parts_and_a_first_row_of_names_is_the_header() {
        let text = b"id,name,note\r\n1,\"Anna, B\",\"says \"\"hi\"\"\r\nthere\"\r\n2,Bo,\r\n";
        let shaped = Csv::default().shape(&stream(text, None)).expect("cuts");
        assert_eq!(shaped.parts.len(), 3);
        assert_eq!(shaped.parts[0].name.as_deref(), Some("header"));
        assert_eq!(shaped.parts[0].bytes, b"id,name,note");
        assert_eq!(shaped.parts[0].media_type.as_deref(), Some("text/csv"));
        assert_eq!(shaped.parts[1].name.as_deref(), Some("2"));
        assert_eq!(
            shaped.parts[1].bytes,
            b"1,\"Anna, B\",\"says \"\"hi\"\"\r\nthere\""
        );
        assert_eq!(shaped.parts[2].name.as_deref(), Some("3"));
        assert_eq!(shaped.parts[2].bytes, b"2,Bo,");
        assert_eq!(shaped.message_type, None);

        let numbers = Csv::default()
            .shape(&stream(b"1,2\n3,4", Some("text/csv; charset=utf-8")))
            .expect("cuts");
        assert_eq!(numbers.parts[0].name.as_deref(), Some("1"));
        assert_eq!(
            numbers.parts[0].media_type.as_deref(),
            Some("text/csv; charset=utf-8")
        );
    }

    #[test]
    fn the_media_type_says_whether_there_is_a_header_and_the_separator_is_sniffed_or_given() {
        let said_present = Csv::default()
            .shape(&stream(b"1;2\n3;4", Some("text/csv; header=present")))
            .expect("cuts");
        assert_eq!(said_present.parts[0].name.as_deref(), Some("header"));
        let said_absent = Csv::default()
            .shape(&stream(b"a;b\n1;2", Some("text/csv; header=\"absent\"")))
            .expect("cuts");
        assert_eq!(said_absent.parts[0].name.as_deref(), Some("1"));

        let sniffed = Csv::default()
            .shape(&stream(b"a\tb,c\n1\t2,3", None))
            .expect("cuts");
        assert_eq!(sniffed.parts.len(), 2);
        let given = Csv::separated_by(b'|')
            .shape(&stream(b"a|b,c\n\"1|2\"|3", None))
            .expect("cuts");
        assert_eq!(given.parts[1].bytes, b"\"1|2\"|3");
        assert!(!reads_as_header(
            Delimited::default(),
            b"a,a",
            &given_first(b"a,a")
        ));
        assert!(!reads_as_header(
            Delimited::default(),
            b"a,",
            &given_first(b"a,")
        ));
        assert!(is_number(b" -1.5 ") && !is_number(b"1.2.3") && !is_number(b"x1"));
    }

    fn given_first(bytes: &[u8]) -> Record {
        Delimited::default()
            .records(bytes)
            .next()
            .expect("one")
            .expect("cuts")
    }

    #[test]
    fn an_open_quote_content_after_a_quote_and_bytes_that_are_not_text_are_refused() {
        let open = Csv::default()
            .shape(&stream(b"a,b\n1,\"two\n", None))
            .expect_err("never closed");
        assert_eq!(open.offset, Some(6));
        assert_eq!(open.to_string(), "csv: unterminated quoted field at byte 6");

        let after = Csv::default()
            .shape(&stream(b"\"a\"x,b", None))
            .expect_err("content after the quote");
        assert_eq!(after.offset, Some(3));

        let binary = Csv::default()
            .shape(&stream(b"a,b\n\x00", None))
            .expect_err("not text");
        assert_eq!(binary.reason, "a NUL byte");
        assert_eq!(binary.offset, Some(4));

        let empty = Csv::default()
            .shape(&stream(b"", None))
            .expect_err("nothing");
        assert_eq!(empty.reason, "no records");
    }

    #[test]
    fn the_shape_claims_csv_and_recognises_two_records_of_the_same_width() {
        assert_eq!(Csv::default().technology(), "csv");
        assert!(Csv::default().media_types().contains(&"text/csv"));
        assert!(Csv::default().recognises(b"a,b,c\n1,2,3\n"));
        assert!(Csv::default().recognises(b"a;b"));
        assert!(Csv::default().recognises(b"\"a,b\",c\n1,2"));
        assert!(!Csv::default().recognises(b"a,b\n1,2,3"));
        assert!(!Csv::default().recognises(b"plain text"));
        assert!(!Csv::default().recognises(b"a,\"b"));
        assert!(!Csv::default().recognises(b"a,b\n\xff"));
        assert!(!Csv::default().recognises(b""));

        let shapes: [&dyn Shape; 1] = [&Csv::default()];
        let by_media = message::choose(&shapes, &stream(b"x", Some("Text/CSV; header=absent")));
        assert_eq!(by_media.map(Shape::technology), Some("csv"));
        let by_look = message::choose(&shapes, &stream(b"a,b\n1,2", None));
        assert_eq!(by_look.map(Shape::technology), Some("csv"));
        assert!(message::choose(&shapes, &stream(b"{}", None)).is_none());
    }
}
