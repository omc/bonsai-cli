//! Human and JSON output.
//!
//! Data goes to stdout; progress, hints, warnings and errors to stderr. API text in
//! human output goes through [`sanitize`]. Colour is off when `NO_COLOR` is set,
//! `TERM=dumb`, or the stream isn't a TTY (anstream decides the last per stream, and
//! honours `CLICOLOR_FORCE`). Writes ignore errors, so a closed pipe
//! (`bonsai clusters | head`) doesn't panic.

use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::time::Duration;

use anstream::{AutoStream, ColorChoice};
use anyhow::Result;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};
use jiff::Timestamp;
use owo_colors::OwoColorize;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::sanitize::sanitize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human,
    Json,
}

pub struct Printer {
    pub mode: Mode,
    pub quiet: bool,
    color: ColorChoice,
}

impl Printer {
    pub fn new(json: bool, quiet: bool) -> Self {
        let off = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        let color = if off {
            ColorChoice::Never
        } else {
            ColorChoice::Auto
        };
        if off {
            inquire::set_global_render_config(inquire::ui::RenderConfig::empty());
        }
        Self {
            mode: if json { Mode::Json } else { Mode::Human },
            quiet,
            color,
        }
    }

    pub fn is_json(&self) -> bool {
        self.mode == Mode::Json
    }

    fn stdout(&self) -> AutoStream<std::io::Stdout> {
        AutoStream::new(std::io::stdout(), self.color)
    }

    fn stderr(&self) -> AutoStream<std::io::Stderr> {
        AutoStream::new(std::io::stderr(), self.color)
    }

    /// Pretty-printed JSON, the server's value, to stdout, with every control,
    /// bidi and format character escaped (see [`write_json`]). Written to the raw
    /// stream, never through anstream, whose stripper would change the text.
    pub fn json(&self, v: &Value) -> Result<()> {
        finish(write_json(&mut std::io::stdout().lock(), v))
    }

    /// One line of data to stdout, byte for byte (a URL, a password): the caller
    /// sanitizes any API text in it. Never styled.
    pub fn data_line(&self, s: &str) -> Result<()> {
        finish(writeln!(std::io::stdout().lock(), "{s}"))
    }

    /// Bytes to stdout as they are (a completion script).
    pub fn data_bytes(&self, b: &[u8]) -> Result<()> {
        let mut out = std::io::stdout().lock();
        finish(out.write_all(b).and_then(|()| out.flush()))
    }

    /// One line of human output to stdout that may carry colour, which is
    /// stripped when stdout isn't a TTY or colour is off.
    pub fn styled_line(&self, s: &str) -> Result<()> {
        finish(writeln!(self.stdout(), "{s}"))
    }

    /// Aligned `key  value` rows to stdout; values are sanitized.
    pub fn kv(&self, rows: &[(&str, String)]) -> Result<()> {
        let width = rows
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        let mut out = self.stdout().lock();
        for (k, v) in rows {
            let pad = " ".repeat(width - k.chars().count());
            finish(writeln!(out, "{}{pad}  {}", k.bold(), sanitize(v, false)))?;
        }
        Ok(())
    }

    /// A table to stdout, fitted to [`table_width`] (see [`layout`]). Columns marked
    /// [`Column::wide`] show only when `wide`. Cells are sanitized (tabs kept), then
    /// tabs and newlines become spaces so a row stays on one line.
    pub fn table(&self, columns: &[Column], rows: Vec<Vec<String>>, wide: bool) -> Result<()> {
        let keep: Vec<usize> = (0..columns.len())
            .filter(|&i| wide || !columns[i].wide)
            .collect();
        let cols: Vec<Column> = keep.iter().map(|&i| columns[i]).collect();
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                keep.iter()
                    .map(|&i| r.get(i).map(|c| cell(c)).unwrap_or_default())
                    .collect()
            })
            .collect();
        let mut out = self.stdout().lock();
        for line in layout(&cols, &rows, table_width()) {
            finish(writeln!(out, "{line}"))?;
        }
        Ok(())
    }

    /// Progress and notes to stderr; `--quiet` drops them.
    pub fn info(&self, s: &str) {
        if !self.quiet {
            let _ = writeln!(self.stderr(), "{}", sanitize(s, false));
        }
    }

    /// `✓ s` to stderr; `--quiet` drops it.
    pub fn success(&self, s: &str) {
        if !self.quiet {
            let _ = writeln!(self.stderr(), "{} {}", "✓".green(), sanitize(s, false));
        }
    }

    /// `s` to stderr, even under `--quiet`: for what the user must see to go on,
    /// such as a login code.
    pub fn notice(&self, s: &str) {
        let _ = writeln!(self.stderr(), "{}", sanitize(s, false));
    }

    /// `! s` to stderr, even under `--quiet`.
    pub fn warn(&self, s: &str) {
        let _ = writeln!(self.stderr(), "{} {}", "!".yellow(), sanitize(s, false));
    }

    /// `✗ s` to stderr, even under `--quiet`.
    pub fn error(&self, s: &str) {
        let _ = writeln!(self.stderr(), "{} {}", "✗".red(), sanitize(s, false));
    }

    /// A spinner on stderr, only when stderr is a TTY and the output is human and
    /// not `--quiet`. Wrap prompts in `ProgressBar::suspend`.
    pub fn spinner(&self, msg: &str) -> Option<ProgressBar> {
        if self.quiet || self.is_json() || !std::io::stderr().is_terminal() {
            return None;
        }
        let target =
            ProgressDrawTarget::term_like_with_hz(Box::new(OneShort(console::Term::stderr())), 20);
        let pb = ProgressBar::with_draw_target(None, target);
        pb.set_style(ProgressStyle::with_template("{spinner} {msg}").expect("the template parses"));
        pb.set_message(sanitize(msg, false).into_owned());
        pb.enable_steady_tick(Duration::from_millis(100));
        Some(pb)
    }
}

/// A terminal one column narrower than `T`, for the spinner. indicatif pads the
/// spinner's line to the width it's given; a line that reaches the last column
/// leaves the cursor in the terminal's pending-wrap state, and terminals disagree
/// on what the next `\r` or text then does. One column short, the line never
/// wraps.
#[derive(Debug)]
pub struct OneShort<T>(pub T);

impl<T: TermLike> TermLike for OneShort<T> {
    fn width(&self) -> u16 {
        self.0.width().saturating_sub(1).max(1)
    }
    fn height(&self) -> u16 {
        self.0.height()
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_up(n)
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_down(n)
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_right(n)
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_left(n)
    }
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        self.0.write_line(s)
    }
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        self.0.write_str(s)
    }
    fn clear_line(&self) -> std::io::Result<()> {
        self.0.clear_line()
    }
    fn flush(&self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// What a table column holds, which decides how [`layout`] may fit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An ID or a name: shown whole, never truncated or dropped.
    Key,
    /// A short value (a state, a price, a count, a time): never truncated.
    Fixed,
    /// Free text: truncated with `…`, down to `min` columns.
    Text { min: usize },
}

/// A table column: its header, what it holds, when it's dropped and whether only
/// `--wide` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub header: &'static str,
    pub kind: Kind,
    /// Lower is dropped first when the table doesn't fit; `None` is never dropped.
    pub priority: Option<u8>,
    /// Shown only under `--wide`.
    pub wide: bool,
}

/// The narrowest a [`Kind::Text`] column is truncated to by default.
pub const TEXT_MIN: usize = 12;

impl Column {
    pub const fn key(header: &'static str) -> Self {
        Self {
            header,
            kind: Kind::Key,
            priority: None,
            wide: false,
        }
    }
    pub const fn fixed(header: &'static str, priority: u8) -> Self {
        Self {
            header,
            kind: Kind::Fixed,
            priority: Some(priority),
            wide: false,
        }
    }
    pub const fn text(header: &'static str, priority: u8) -> Self {
        Self {
            header,
            kind: Kind::Text { min: TEXT_MIN },
            priority: Some(priority),
            wide: false,
        }
    }
    /// Free text that is cut to no fewer than `min` columns but never dropped:
    /// what the table is about, such as a search term.
    pub const fn text_kept(header: &'static str, min: usize) -> Self {
        Self {
            header,
            kind: Kind::Text { min },
            priority: None,
            wide: false,
        }
    }
    /// Never dropped.
    pub const fn kept(self) -> Self {
        Self {
            priority: None,
            ..self
        }
    }
    /// Shown only under `--wide`.
    pub const fn wide(self) -> Self {
        Self { wide: true, ..self }
    }
}

/// The width tables fit: `COLUMNS` when it's a positive integer, else the
/// terminal's when stdout is one, else unlimited (`None`), so piped output is
/// whole.
pub fn table_width() -> Option<usize> {
    if let Some(n) = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return Some(n);
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    terminal_size::terminal_size().map(|(w, _)| usize::from(w.0))
}

/// API text as one table cell: [`sanitize`]d (tabs kept), then tabs and newlines
/// turned into spaces, so a row stays on one line.
pub fn cell(s: &str) -> String {
    sanitize(s, true).replace(['\t', '\n'], " ")
}

/// Columns between two cells.
const GAP: usize = 2;

/// The lines of a table of `columns` and `rows` fitted to `width` (unlimited when
/// `None`). Cells must already be single-line and sanitized.
///
/// Every column starts at its content's width. While the table is too wide, the
/// widest [`Kind::Text`] column loses a column, down to its minimum; if that isn't
/// enough, the lowest-priority droppable column goes (the rightmost of equals) and
/// the Text columns get their room back. When only Key and never-dropped columns
/// are left, the table is printed anyway and the terminal wraps the line. No cell
/// is ever broken: it's whole, or cut short with `…`.
pub fn layout(columns: &[Column], rows: &[Vec<String>], width: Option<usize>) -> Vec<String> {
    let natural: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| c.width()))
                .fold(c.header.width(), usize::max)
        })
        .collect();
    let mut shown: Vec<usize> = (0..columns.len()).collect();
    let widths = loop {
        let Some(max) = width else {
            break natural.clone();
        };
        let (widths, fits) = shrink(columns, &natural, &shown, max);
        if fits {
            break widths;
        }
        let drop = shown
            .iter()
            .enumerate()
            .filter_map(|(at, &i)| columns[i].priority.map(|p| (p, std::cmp::Reverse(at))))
            .min()
            .map(|(_, std::cmp::Reverse(at))| at);
        match drop {
            Some(at) => {
                shown.remove(at);
            }
            None => break widths,
        }
    };
    let line = |cell: &dyn Fn(usize) -> String| {
        let mut out = String::new();
        for (n, &i) in shown.iter().enumerate() {
            let text = cell(i);
            let text = truncate(&text, widths[i]);
            if n > 0 {
                out.push_str(&" ".repeat(GAP));
            }
            out.push_str(&text);
            // A backstop: `truncate` keeps every cell within its width.
            out.push_str(&" ".repeat(widths[i].saturating_sub(text.width())));
        }
        out.truncate(out.trim_end().len());
        out
    };
    let mut lines = vec![line(&|i| columns[i].header.to_owned())];
    lines.extend(
        rows.iter()
            .map(|r| line(&|i| r.get(i).cloned().unwrap_or_default())),
    );
    lines
}

/// The widths of the `shown` columns with the Text ones shrunk, widest first, until
/// the line fits `max` or they're all at their minimum; and whether it fits.
fn shrink(
    columns: &[Column],
    natural: &[usize],
    shown: &[usize],
    max: usize,
) -> (Vec<usize>, bool) {
    let mut widths = natural.to_vec();
    let total = |w: &[usize]| {
        shown.iter().map(|&i| w[i]).sum::<usize>() + GAP * shown.len().saturating_sub(1)
    };
    let floor = |i: usize| match columns[i].kind {
        Kind::Text { min } => columns[i].header.width().max(min).min(natural[i]),
        Kind::Key | Kind::Fixed => natural[i],
    };
    let mut over = total(&widths).saturating_sub(max);
    while over > 0 {
        let Some(i) = shown
            .iter()
            .copied()
            .filter(|&i| widths[i] > floor(i))
            .max_by_key(|&i| (widths[i], std::cmp::Reverse(i)))
        else {
            break;
        };
        widths[i] -= 1;
        over -= 1;
    }
    (widths, over == 0)
}

/// `s` cut to `max` display columns, ending in `…` when anything was cut. It cuts
/// between grapheme clusters, never inside one, and measures with `str::width`, as
/// [`layout`] does: a char-by-char sum disagrees with it on sequences such as `❤️`
/// (VS16) or a keycap.
pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let room = max - 1; // for the `…`
    // Whole graphemes while their summed widths fit; a grapheme's own width is
    // context-free, but neighbours can still combine, so check the prefix as a
    // string and give back graphemes until it fits.
    let mut ends = Vec::new();
    let mut used = 0;
    for (at, g) in s.grapheme_indices(true) {
        let w = g.width();
        if used + w > room {
            break;
        }
        used += w;
        ends.push(at + g.len());
    }
    while let Some(&end) = ends.last() {
        if s[..end].width() <= room {
            break;
        }
        ends.pop();
    }
    let end = ends.last().copied().unwrap_or(0);
    Cow::Owned(format!("{}…", &s[..end]))
}

/// `v` pretty-printed, with a newline. Every character [`sanitize`] would replace
/// (C0 and C1 controls, DEL, bidi controls, `Cf` format characters) is written as
/// a `\uXXXX` escape (a UTF-16 surrogate pair above the BMP), in keys and values
/// alike, so the document parses to exactly `v` but none of them reaches a
/// terminal raw.
pub fn write_json(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let mut ser = serde_json::Serializer::with_formatter(
        &mut *w,
        EscapingFormatter(serde_json::ser::PrettyFormatter::new()),
    );
    // `From` gives back the writer's own error (a closed pipe stays `BrokenPipe`).
    serde::Serialize::serialize(v, &mut ser).map_err(std::io::Error::from)?;
    writeln!(w)?;
    w.flush()
}

/// [`write_json`] into a `String`, for the `--json` error report on stderr.
pub fn json_string(v: &Value) -> String {
    let mut out = Vec::new();
    write_json(&mut out, v).expect("writing to a Vec doesn't fail");
    let mut s = String::from_utf8(out).expect("serde_json writes UTF-8");
    s.truncate(s.trim_end_matches('\n').len());
    s
}

/// serde_json's pretty formatter, but string fragments it would pass through
/// unescaped get `\uXXXX` for each unsafe character. serde_json already escapes
/// `"`, `\\` and C0 controls itself, through `write_char_escape`.
struct EscapingFormatter(serde_json::ser::PrettyFormatter<'static>);

impl serde_json::ser::Formatter for EscapingFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if !crate::sanitize::is_unsafe(c) {
                continue;
            }
            w.write_all(&fragment.as_bytes()[start..i])?;
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                write!(w, "\\u{unit:04x}")?;
            }
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
    }

    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_array(w)
    }
    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_array(w)
    }
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_array_value(w)
    }
    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_object(w)
    }
    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_object(w)
    }
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_object_value(w)
    }
}

/// A stdout write's outcome for the command. A closed pipe (`bonsai api … | head`)
/// is the reader's choice, so it isn't an error; any other failure (a full disk
/// under `bonsai url > .env`) fails the command.
fn finish(res: std::io::Result<()>) -> Result<()> {
    match res {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(anyhow::Error::new(e).context("couldn't write to stdout"))
        }
        _ => Ok(()),
    }
}

/// Log output to stderr through [`sanitize`] (newlines kept): log lines can carry
/// server text (a Problem's title in an error), and tracing-subscriber escapes
/// neither `\r` nor bidi controls.
pub struct SanitizedStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SanitizedStderr {
    type Writer = Sanitizing<std::io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        Sanitizing(std::io::stderr())
    }
}

/// Writes each chunk sanitized. tracing-subscriber hands over a whole formatted
/// event per `write_all`, and this consumes every chunk whole, so a character is
/// never split between writes.
pub struct Sanitizing<W>(pub W);

impl<W: Write> Write for Sanitizing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0.write_all(sanitize(&text, false).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[allow(dead_code, reason = "the cluster and observability commands use it")]
/// `n` bytes in binary units with one decimal: `512 B`, `1.5 KiB`, `5.0 GiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// `t` relative to `now` in its largest whole unit (`3h ago`, `in 2m`), or the
/// UTC date when it's more than 30 days away.
pub fn relative_time(t: Timestamp, now: Timestamp) -> String {
    let secs = now.as_second() - t.as_second();
    let abs = secs.unsigned_abs();
    if abs > 30 * 86_400 {
        return t.strftime("%Y-%m-%d").to_string();
    }
    let amount = match abs {
        0 => return "just now".into(),
        1..60 => format!("{abs}s"),
        60..3600 => format!("{}m", abs / 60),
        3600..86_400 => format!("{}h", abs / 3600),
        _ => format!("{}d", abs / 86_400),
    };
    if secs >= 0 {
        format!("{amount} ago")
    } else {
        format!("in {amount}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn log_lines_are_sanitized() {
        let mut w = Sanitizing(Vec::new());
        w.write_all("WARN error=Bad\rTitle \u{202e}evil\x1b[2J\n".as_bytes())
            .unwrap();
        let out = String::from_utf8(w.0).unwrap();
        assert_eq!(out, "WARN error=Bad\u{FFFD}Title \u{FFFD}evil\u{FFFD}[2J\n");
    }

    #[test]
    fn json_escapes_every_unsafe_char_and_keeps_the_value() {
        let v = serde_json::json!({
            "name": "del\u{7f}name\u{1b}[31m",
            "c1": "csi\u{9b}2J",
            "bidi": "abc\u{202e}cba\u{2066}",
            "fmt": "zw\u{200b}j\u{feff}",
            "tag": "astral\u{E0041}",
            "nl": "a\nb\tc\r",
            "plain": "ünïcödé ✓ 😀",
            "list": ["\u{61c}", {"\u{202e}key": 1}],
        });
        let mut out = Vec::new();
        write_json(&mut out, &v).unwrap();
        let text = String::from_utf8(out).unwrap();
        for c in text.chars() {
            assert!(
                c == '\n' || !(c.is_control() || crate::sanitize::is_unsafe(c)),
                "raw {:X} in {text}",
                c as u32
            );
        }
        for escaped in [
            "\\u007f",
            "\\u001b",
            "\\u009b",
            "\\u202e",
            "\\u2066",
            "\\u200b",
            "\\ufeff",
            "\\u061c",
            "\\udb40\\udc41",
        ] {
            assert!(text.contains(escaped), "{escaped} in {text}");
        }
        assert!(text.contains("ünïcödé ✓ 😀"), "{text}");
        assert!(text.ends_with("}\n"));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    }

    #[test]
    fn json_error_text_is_escaped_too() {
        let v = serde_json::json!({"error": "bad\u{202e}\u{7f}"});
        let text = json_string(&v);
        assert!(
            !text.contains('\u{202e}') && !text.contains('\u{7f}'),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn relative_time_rounds_down_to_one_unit() {
        let now: Timestamp = "2026-10-20T12:00:00Z".parse().unwrap();
        let ago = |s: i64| relative_time(now - jiff::SignedDuration::from_secs(s), now);
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(30), "30s ago");
        assert_eq!(ago(90), "1m ago");
        assert_eq!(ago(3 * 3600 + 5), "3h ago");
        assert_eq!(ago(2 * 86_400), "2d ago");
        assert_eq!(ago(40 * 86_400), "2026-09-10");
        assert_eq!(
            relative_time(now + jiff::SignedDuration::from_secs(120), now),
            "in 2m"
        );
    }

    // ------------------------------------------------------------ tables

    fn plan_cols() -> Vec<Column> {
        vec![
            Column::key("ID"),
            Column::text("NAME", 4),
            Column::fixed("PRICE", 5),
            Column::fixed("TENANCY", 3),
            Column::fixed("PRIVATE NETWORK", 2),
            Column::text("REGIONS", 1),
            Column::text("RELEASES", 1),
        ]
    }

    fn plan_rows() -> Vec<Vec<String>> {
        let row = |id: &str, name: &str, price: &str| {
            [
                id,
                name,
                price,
                "shared",
                "no",
                "aws-eu-west-1, aws-us-east-1, aws-us-west-2",
                "opensearch-3.8.0, opensearch-2.19.6, elasticsearch-7.10.2",
            ]
            .map(String::from)
            .to_vec()
        };
        vec![
            row("sandbox", "Sandbox", "Free"),
            row("standard-fir", "Standard Fir", "$20/mo"),
            row(
                "business-performance-lg",
                "Business Performance Large",
                "$600/mo",
            ),
        ]
    }

    /// The table's cells, cut at the header's column starts. Only for ASCII data
    /// (`…` is the one wide-char exception, and it's one column).
    fn cells(lines: &[String]) -> Vec<Vec<String>> {
        let header: Vec<char> = lines[0].chars().collect();
        let mut starts = vec![0];
        for i in 2..header.len() {
            if header[i] != ' ' && header[i - 1] == ' ' && header[i - 2] == ' ' {
                starts.push(i);
            }
        }
        lines
            .iter()
            .map(|l| {
                let chars: Vec<char> = l.chars().collect();
                starts
                    .iter()
                    .enumerate()
                    .map(|(j, &s)| {
                        let e = starts
                            .get(j + 1)
                            .copied()
                            .unwrap_or(chars.len())
                            .min(chars.len());
                        chars[s.min(chars.len())..e]
                            .iter()
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect()
            })
            .collect()
    }

    fn headers(lines: &[String]) -> Vec<String> {
        lines[0]
            .split("  ")
            .filter(|h| !h.is_empty())
            .map(|h| h.trim().to_owned())
            .collect()
    }

    /// Every shown cell is its whole value, or a prefix of it ending in `…`.
    fn assert_whole_or_ellipsis(cols: &[Column], rows: &[Vec<String>], lines: &[String]) {
        let shown = headers(lines);
        let idx: Vec<usize> = shown
            .iter()
            .map(|h| cols.iter().position(|c| c.header == h).unwrap())
            .collect();
        for (r, line) in cells(lines).iter().skip(1).enumerate() {
            for (j, cell) in line.iter().enumerate() {
                let full = &rows[r][idx[j]];
                let ok = cell == full
                    || cell
                        .strip_suffix('…')
                        .is_some_and(|p| full.starts_with(p) && p.len() < full.len());
                assert!(
                    ok,
                    "{cell:?} isn't {full:?} or a prefix + …\n{}",
                    lines.join("\n")
                );
                if cols[idx[j]].kind == Kind::Key {
                    assert_eq!(cell, full, "a key was cut");
                }
            }
        }
    }

    fn widest(lines: &[String]) -> usize {
        lines.iter().map(|l| l.chars().count()).max().unwrap()
    }

    #[test]
    fn unlimited_width_shows_everything_whole() {
        let (cols, rows) = (plan_cols(), plan_rows());
        let lines = layout(&cols, &rows, None);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            headers(&lines),
            cols.iter().map(|c| c.header).collect::<Vec<_>>()
        );
        for (line, row) in cells(&lines).iter().skip(1).zip(&rows) {
            assert_eq!(line, row);
        }
        assert!(lines.iter().all(|l| l == l.trim_end()));
    }

    #[test]
    fn fits_every_width_without_breaking_a_word() {
        let (cols, rows) = (plan_cols(), plan_rows());
        for width in [200, 120, 80, 60, 40] {
            let lines = layout(&cols, &rows, Some(width));
            assert_eq!(lines.len(), 4, "one line per row at {width}");
            assert!(widest(&lines) <= width, "{width}:\n{}", lines.join("\n"));
            assert_whole_or_ellipsis(&cols, &rows, &lines);
        }
    }

    #[test]
    fn drops_the_lowest_priority_first() {
        let (cols, rows) = (plan_cols(), plan_rows());
        let at = |w| headers(&layout(&cols, &rows, Some(w)));
        assert_eq!(at(200).len(), 7);
        // 120: the lists are cut short, nothing is dropped.
        assert_eq!(at(120).len(), 7);
        // 90: even at their minimums it doesn't fit; RELEASES, the rightmost of the
        // two lowest, goes first.
        assert_eq!(
            at(90),
            [
                "ID",
                "NAME",
                "PRICE",
                "TENANCY",
                "PRIVATE NETWORK",
                "REGIONS"
            ]
        );
        assert_eq!(
            at(80),
            ["ID", "NAME", "PRICE", "TENANCY", "PRIVATE NETWORK"]
        );
        assert_eq!(at(60), ["ID", "NAME", "PRICE", "TENANCY"]);
        assert_eq!(at(40), ["ID", "PRICE"]);
    }

    #[test]
    fn text_shrinks_before_anything_is_dropped() {
        let cols = [
            Column::key("ID"),
            Column::text("NAME", 1),
            Column::fixed("STATE", 2),
        ];
        let rows = vec![
            ["c1", "a rather long cluster description", "ready"]
                .map(String::from)
                .to_vec(),
        ];
        let lines = layout(&cols, &rows, Some(30));
        assert_eq!(lines[1], "c1  a rather long clus…  ready");
        assert!(widest(&lines) <= 30);
    }

    #[test]
    fn only_keys_left_prints_them_whole() {
        let cols = [
            Column::key("NAME"),
            Column::key("ID"),
            Column::fixed("STATE", 1),
        ];
        let rows = vec![
            ["logs-production-eu", "logs-production-eu-90p05v", "ready"]
                .map(String::from)
                .to_vec(),
        ];
        let lines = layout(&cols, &rows, Some(20));
        assert_eq!(
            lines,
            [
                "NAME                ID",
                "logs-production-eu  logs-production-eu-90p05v"
            ]
        );
    }

    #[test]
    fn kept_text_is_truncated_but_never_dropped() {
        let cols = [
            Column::text_kept("QUERY", 20),
            Column::fixed("VOLUME", 4),
            Column::fixed("AVG TIME", 3),
        ];
        let q = "an extremely long search term somebody typed into the box";
        let rows = vec![vec![q.to_owned(), "1,234".into(), "13.2 ms".into()]];
        let lines = layout(&cols, &rows, Some(30));
        assert_eq!(headers(&lines), ["QUERY", "VOLUME"]);
        // AVG TIME goes, and the term gets the room back.
        assert_eq!(lines[1], "an extremely long sea…  1,234");
        let lines = layout(&cols, &rows, Some(10));
        assert_eq!(lines[1], "an extremely long s…");
    }

    #[test]
    fn ties_drop_the_rightmost_first() {
        let cols = [
            Column::key("ID"),
            Column::fixed("A", 1),
            Column::fixed("B", 1),
        ];
        let rows = vec![["x", "aaaa", "bbbb"].map(String::from).to_vec()];
        assert_eq!(layout(&cols, &rows, Some(9)), ["ID  A", "x   aaaa"]);
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("standard-birch", 20), "standard-birch");
        assert_eq!(truncate("standard-birch", 14), "standard-birch");
        assert_eq!(truncate("standard-birch", 9), "standard…");
        assert_eq!(truncate("日本語の検索", 7), "日本語…");
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
    }

    /// Values whose `str::width` differs from the sum of their chars' widths, or
    /// that are several chars per grapheme.
    const HARD: [&str; 5] = [
        "❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}❤\u{FE0F}",
        "#\u{FE0F}\u{20E3}#\u{FE0F}\u{20E3}#\u{FE0F}\u{20E3}#\u{FE0F}\u{20E3}#\u{FE0F}\u{20E3}#\u{FE0F}\u{20E3}",
        "👍🏽👍🏽👍🏽👍🏽👍🏽👍🏽👍🏽👍🏽 thumbs",
        "🇯🇵🇫🇷🇩🇪🇧🇷🇲🇽🇯🇵🇫🇷",
        "日本語の検索プラン名前がとても長い",
    ];

    /// `cut` is `full`, or whole graphemes of `full` then `…`.
    fn assert_grapheme_prefix(cut: &str, full: &str) {
        if cut == full {
            return;
        }
        let prefix = cut
            .strip_suffix('…')
            .unwrap_or_else(|| panic!("{cut:?} of {full:?}"));
        let mut acc = String::new();
        for g in full.graphemes(true) {
            if acc == prefix {
                return;
            }
            acc.push_str(g);
        }
        assert_eq!(acc, prefix, "{cut:?} splits a grapheme of {full:?}");
    }

    #[test]
    fn truncate_cuts_whole_graphemes_by_str_width() {
        for s in HARD {
            for max in 0..=s.width() + 1 {
                let cut = truncate(s, max);
                assert!(cut.width() <= max, "{cut:?} is wider than {max}");
                if max > 0 {
                    assert_grapheme_prefix(&cut, s);
                }
            }
        }
    }

    #[test]
    fn wide_characters_fit_and_never_panic() {
        let cols = [
            Column::key("ID"),
            Column::text("NAME", 3),
            Column::fixed("PRICE", 2),
        ];
        let rows: Vec<Vec<String>> = HARD
            .iter()
            .enumerate()
            .map(|(i, n)| vec![format!("p{i}"), (*n).to_owned(), "$50/mo".into()])
            .collect();
        for width in [80, 60, 40, 30, 20, 16] {
            let lines = layout(&cols, &rows, Some(width));
            for line in &lines {
                assert!(
                    line.width() <= width,
                    "{} > {width}: {line:?}",
                    line.width()
                );
            }
            for (line, row) in lines.iter().skip(1).zip(&rows) {
                let cells: Vec<&str> = line
                    .split("  ")
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .collect();
                assert_eq!(cells[0], row[0]);
                assert_grapheme_prefix(cells[1], &row[1]);
            }
        }
    }

    /// Records every string drawn, as a terminal 60 columns wide.
    #[derive(Clone, Debug, Default)]
    struct Drawn(std::sync::Arc<std::sync::Mutex<String>>);

    impl indicatif::TermLike for Drawn {
        fn width(&self) -> u16 {
            60
        }
        fn move_cursor_up(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> std::io::Result<()> {
            self.write_str(&format!("{s}\n"))
        }
        fn write_str(&self, s: &str) -> std::io::Result<()> {
            self.0.lock().unwrap().push_str(s);
            Ok(())
        }
        fn clear_line(&self) -> std::io::Result<()> {
            self.write_str("\r")
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_spinner_line_stops_short_of_the_last_column() {
        let drawn = Drawn::default();
        let bar = ProgressBar::with_draw_target(
            None,
            indicatif::ProgressDrawTarget::term_like(Box::new(OneShort(drawn.clone()))),
        );
        bar.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
        bar.set_message("Provisioning t1");
        bar.tick();
        bar.finish_and_clear();
        let text = drawn.0.lock().unwrap().clone();
        assert!(text.contains("Provisioning t1"), "{text:?}");
        for line in text.split('\r').filter(|l| !l.is_empty()) {
            assert_eq!(line.width(), 59, "{line:?}");
        }
    }
}
