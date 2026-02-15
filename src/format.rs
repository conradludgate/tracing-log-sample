use std::fmt::Write as _;

use tracing::Event;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};

/// Formats a [`tracing::Event`] into a byte buffer.
///
/// Implement this trait to customise how sampled events are serialised
/// before being written out.
pub trait FormatEvent: Send + Sync {
    /// Write a formatted representation of `event` into `buf`.
    fn format_event(&self, event: &Event<'_>, buf: &mut Vec<u8>) -> std::fmt::Result;
}

/// Plain-text formatter that writes `LEVEL target fields\n`.
pub struct TextFormat;

impl FormatEvent for TextFormat {
    fn format_event(&self, event: &Event<'_>, buf: &mut Vec<u8>) -> std::fmt::Result {
        let meta = event.metadata();
        let mut fmt_buf = BufWriter(buf);
        write!(fmt_buf, "{} {} ", meta.level(), meta.target())?;
        DefaultFields::new().format_fields(Writer::new(&mut fmt_buf), event)?;
        fmt_buf.0.push(b'\n');
        Ok(())
    }
}

pub(crate) struct BufWriter<'a>(pub(crate) &'a mut Vec<u8>);

impl std::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}
