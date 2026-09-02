//! The last place a log line can still be made safe: a `MakeWriter` wrapper
//! that escapes every control character on its way out of the subscriber.
//!
//! # Why this is at the sink and not at the producers
//!
//! A log line is a record with a shape an operator reads by eye and a script
//! reads by `grep`: one line, timestamp first, fields after the message. Any
//! text that reaches a `tracing` event carrying a bare newline ends that line
//! early and starts a second one the writer of the text composed in full -
//! timestamp, level, target and all. Nothing downstream can tell the forged
//! line from a real one, because by the time it is bytes on a pipe there is no
//! longer a boundary to appeal to.
//!
//! The producers cannot be the fix. There are roughly ninety `tracing` call
//! sites in this workspace that interpolate something a tenant chose - an
//! application's name, a queue name, a URL, a header value, an error built
//! from one - and the set grows every time someone adds a log line. Escaping
//! at each of them is a rule that has to be remembered ninety times and then
//! once more per new line. Escaping here is a rule enforced once, below every
//! producer, for every binary that installs it.
//!
//! "Every producer" means every producer of a `tracing` event. One writer of
//! tenant text is not one: the isolate's native console in deis-runtime,
//! which `console.*` still reaches beside `op_log` and which writes to the
//! process's stdout without a subscriber in between. That copy is escaped
//! where it is made (`js/runtime.js`, `escapeControl`), by the same rule as
//! [`escape_control`] here. deis-build does not install this wrapper: it is
//! the author's compiler running over the author's own tree, and the only
//! text in its log is theirs.
//!
//! # What the pinned formatter already does, and what it does not
//!
//! Measured against `tracing-subscriber` 0.3.23, which `Cargo.lock` pins:
//!
//! * The `message` field is written through `EscapeGuard`
//!   (`src/fmt/format/escape.rs`), which escapes ESC, BEL, BS, FF, DEL and the
//!   C1 block - and **not** LF, CR, NUL, VT, or the rest of C0. A newline in a
//!   message therefore reaches the writer intact. That is the hole.
//! * Other fields are written with `{:?}` (`src/fmt/format/mod.rs`, the
//!   `record_debug` arms after the `"message"` arm). A `&str` field picks up
//!   Rust's own `Debug` escaping and is safe by accident; a field recorded
//!   through `%value` is formatted by its `Display` impl inside a `format_args`
//!   whose `Debug` is that same `Display`, so it reaches the writer raw -
//!   including ESC.
//!
//! So ESC is escaped on one tenant-reachable path and not on another. There is
//! no way to tell the formatter's own colour codes from a tenant's ESC once
//! both are bytes in the same buffer, and inventing a heuristic that tried
//! would be a hole with extra steps. This wrapper therefore escapes **all**
//! control characters, ESC included, and every install site pairs it with
//! `with_ansi(false)` so the formatter stops emitting colour it would only
//! watch get escaped.
//!
//! Colour is a real thing to give up and it is worth naming what it costs:
//! nothing on the services, which write to files and journals where the codes
//! were never wanted (`tracing-subscriber` enables ANSI by default and does
//! not check for a terminal, so those files have been collecting escape codes
//! all along), and very little on the `deis` CLI, whose human-facing colour
//! comes from `colored` on its own `eprintln!` paths rather than from
//! `tracing`.
//!
//! # The escaping rule, and why it is that one
//!
//! Identical to `escape_control` in `crates/deis-runtime/src/runtime/ops.rs`
//! and `crates/deis/src/commands/mod.rs`: `char::is_control` - the Unicode
//! `Cc` category, meaning C0, DEL, and the C1 block - spelled with
//! `char::escape_debug`, so a newline reads `\n` and U+009B reads `\u{9b}`.
//! Three copies of one rule is two too many to leave undescribed, so: those
//! two escape a *value* that is about to be quoted as evidence, and they run
//! whether or not this sink is installed (the runtime's app log also travels
//! to deis-telemetry through a `Layer` that never passes through `fmt`). This
//! one escapes a *line* and is the floor under all of them.
//!
//! C1 is in the rule for the same reason it is in theirs: U+009B is a
//! single-character CSI on a good many terminals, so a string that contains no
//! ESC at all can still repaint the operator's screen.
//!
//! # What this cannot do
//!
//! It keeps one event to one line. It cannot make the line *true*. A producer
//! that writes `info!("request from {}", tenant_supplied)` where the tenant
//! supplied `app_id=someone-else` still emits a line whose tail reads like a
//! structured field, and an operator skimming it can still be fooled. Only the
//! producer knows which of its bytes are its own and which are quoted, and
//! only the producer can quote them. This wrapper guarantees the record
//! boundary, nothing above it.

use std::io::{self, Write};

use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

/// Wraps a `MakeWriter` so that no event it forwards can end its line early.
///
/// Install it with `with_writer`, and pair it with `with_ansi(false)`: this
/// escapes ESC along with everything else, so colour codes would survive only
/// as literal `\u{1b}[2m` text.
///
/// ```no_run
/// # use deis_wire::logging::LineSafe;
/// tracing_subscriber::fmt()
///     .with_ansi(false)
///     .with_writer(LineSafe::new(std::io::stdout))
///     .init();
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LineSafe<W> {
    inner: W,
}

impl<W> LineSafe<W> {
    /// Wrap `inner`. `inner` is the `MakeWriter` that would otherwise have
    /// been handed to `with_writer` - `std::io::stdout`, `std::io::stderr`, a
    /// `Mutex<File>`, anything else that implements the trait.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// The wrapped `MakeWriter`, for a caller that needs to reach past this.
    pub fn inner(&self) -> &W {
        &self.inner
    }
}

impl<'a, W> MakeWriter<'a> for LineSafe<W>
where
    W: MakeWriter<'a>,
{
    type Writer = LineSafeWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        LineSafeWriter::new(self.inner.make_writer())
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        LineSafeWriter::new(self.inner.make_writer_for(meta))
    }
}

/// One event's worth of buffer. Escapes and forwards when it is dropped.
///
/// Buffering per writer instance is the right unit because the `fmt` layer
/// asks for a writer once per event and drops it before the next: see
/// `tracing-subscriber` 0.3.23, `src/fmt/fmt_layer.rs`, in
/// `<Layer as Layer<S>>::on_event` - `self.make_writer.make_writer_for(...)`
/// at line 1049 (the formatted event) and line 1059 (the "could not format"
/// fallback), each inside the same `if` arm as the `write_all` that follows
/// it. That is load-bearing rather than incidental: a writer handed two
/// events would escape the LF between them and emit one line holding both.
/// No forgery gets through either way - the second event's timestamp would be
/// visibly escaped rather than starting a line - but a record boundary would
/// be lost, so the assumption is pinned by
/// `two_events_through_one_writer_are_welded_into_one_line` below. If that
/// test ever describes real behaviour rather than a hypothetical, the fix is
/// to make `write` emit at the terminator instead of buffering to `Drop`.
pub struct LineSafeWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> LineSafeWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            // Enough for an ordinary line without a second allocation; an
            // event that needs more is not the common case.
            buf: Vec::with_capacity(256),
        }
    }
}

impl<W: Write> Write for LineSafeWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    /// Deliberately does nothing.
    ///
    /// A flush in the middle of an event would forward half a line, which is
    /// the shape this type exists to prevent - and it could not know that the
    /// half it is holding ends anywhere in particular. The buffer is written
    /// and the inner writer flushed on `Drop`, which the `fmt` layer reaches
    /// immediately after its single `write_all`.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for LineSafeWriter<W> {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(self.buf.len() + 16);
        escape_event(&self.buf, &mut out);
        // Nothing useful to do with a failure here: this is a destructor, so
        // it must not panic, and reporting the error would itself be a log
        // line through the writer that just failed. `tracing-subscriber` makes
        // the same trade for the same reason (its `log_internal_errors` is off
        // by default).
        let _ = self.inner.write_all(&out);
        let _ = self.inner.flush();
    }
}

/// Escape one event's bytes into `out`, keeping the terminator.
///
/// `event` is what the formatter wrote, which ends in the single LF the
/// formatter appends. That LF is the record separator and is passed through;
/// every other control character in the event - the tenant's included - is
/// spelled out.
fn escape_event(event: &[u8], out: &mut Vec<u8>) {
    let (body, terminator) = match event.split_last() {
        Some((b'\n', body)) => (body, &event[event.len() - 1..]),
        _ => (event, &[][..]),
    };

    // The `fmt` layer formats into a `String` and hands us its bytes, so the
    // valid-UTF-8 arm is the only one that runs in this workspace. The lossy
    // arm covers a caller outside `fmt` writing arbitrary bytes: replacing
    // them beats forwarding them, because a lone 0x9B in some other encoding
    // is a CSI on the operator's terminal all the same.
    out.extend_from_slice(escape_control(&String::from_utf8_lossy(body)).as_bytes());
    out.extend_from_slice(terminator);
}

/// Spell every control character in `text` so it cannot act on a terminal or
/// end a line: the Unicode `Cc` category (C0, DEL, the C1 block) written the
/// way `char::escape_debug` writes it, so a newline reads `\n` and U+009B
/// reads `\u{9b}`. Text without one is returned as it was, unallocated.
///
/// This is the rule [`LineSafe`] applies to a whole event, made available on
/// its own for a line that is written with `writeln!` rather than through a
/// subscriber - deis-proxy's access log, whose fields come from request
/// headers, is one.
pub fn escape_control(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if character.is_control() {
            out.extend(character.escape_debug());
        } else {
            out.push(character);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn escape_control_spells_every_control_character_and_leaves_plain_text_alone() {
        let plain = "GET /index.html \"Mozilla/5.0\" 200";
        assert!(matches!(
            escape_control(plain),
            std::borrow::Cow::Borrowed(_)
        ));

        let forged = "ok\n2026-09-03T00:00:00Z INFO forged\u{9b}31m\r\u{0}";
        let escaped = escape_control(forged);
        assert_eq!(
            escaped.as_ref(),
            "ok\\n2026-09-03T00:00:00Z INFO forged\\u{9b}31m\\r\\0"
        );
        assert!(!escaped.chars().any(char::is_control), "{escaped:?}");
    }

    /// Collects everything written, so a test can count lines.
    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<u8>>>);

    impl Collector {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        /// Lines as an operator's `grep` would see them: the trailing LF ends
        /// the last line rather than starting an empty one.
        fn lines(&self) -> Vec<String> {
            let text = self.text();
            text.strip_suffix('\n')
                .unwrap_or(&text)
                .split('\n')
                .map(str::to_owned)
                .collect()
        }
    }

    impl Write for Collector {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Collector {
        type Writer = Collector;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn through_wrapper(pieces: &[&str]) -> String {
        let sink = Collector::default();
        let make = LineSafe::new(sink.clone());
        {
            let mut writer = make.make_writer();
            for piece in pieces {
                writer.write_all(piece.as_bytes()).unwrap();
            }
        }
        sink.text()
    }

    #[test]
    fn newline_inside_an_event_cannot_start_a_second_line() {
        let out = through_wrapper(&["INFO ok\n2026-09-03T00:00:00Z  INFO forged\n"]);
        assert_eq!(out, "INFO ok\\n2026-09-03T00:00:00Z  INFO forged\n");
        assert_eq!(out.matches('\n').count(), 1);
    }

    #[test]
    fn carriage_return_cannot_rewrite_the_line_in_place() {
        // CR alone does not end a line, but it does put the cursor back at
        // column zero, so the next characters overwrite what an operator
        // already read.
        let out = through_wrapper(&["INFO real\rINFO fake\n"]);
        assert_eq!(out, "INFO real\\rINFO fake\n");
    }

    #[test]
    fn nul_and_del_are_spelled_out() {
        let out = through_wrapper(&["a\0b\u{7f}c\n"]);
        assert_eq!(out, "a\\0b\\u{7f}c\n");
    }

    #[test]
    fn c1_csi_is_spelled_out_even_though_it_has_no_esc() {
        // U+009B is a one-byte CSI on many terminals: without this it would
        // repaint the screen with no ESC anywhere in the string.
        let out = through_wrapper(&["a\u{9b}31mb\n"]);
        assert_eq!(out, "a\\u{9b}31mb\n");
    }

    #[test]
    fn esc_is_spelled_out() {
        let out = through_wrapper(&["a\u{1b}[31mb\n"]);
        assert_eq!(out, "a\\u{1b}[31mb\n");
    }

    #[test]
    fn the_trailing_newline_is_kept_exactly_once() {
        let out = through_wrapper(&["plain\n"]);
        assert_eq!(out, "plain\n");
    }

    #[test]
    fn an_event_written_in_several_pieces_is_still_one_line() {
        // A formatter is free to write the timestamp, the level, the message
        // and the terminator as separate calls; the buffer is what makes the
        // trailing-LF rule apply to the event rather than to each piece.
        let out = through_wrapper(&["2026-09-03  ", "INFO ", "ok\nforged", "\n"]);
        assert_eq!(out, "2026-09-03  INFO ok\\nforged\n");
    }

    #[test]
    fn two_events_through_one_writer_are_welded_into_one_line() {
        // Not how the pinned `fmt` layer behaves: it takes a writer per event
        // and drops it. This records what would happen if that changed, so the
        // assumption is visible instead of implied. Nothing is forged - the
        // second record's own newline is escaped in plain sight - but the two
        // records arrive as one line.
        let sink = Collector::default();
        let make = LineSafe::new(sink.clone());
        {
            let mut writer = make.make_writer();
            writer.write_all(b"first\n").unwrap();
            writer.write_all(b"second\n").unwrap();
        }
        assert_eq!(sink.lines(), vec!["first\\nsecond".to_owned()]);
    }

    #[test]
    fn ordinary_text_is_left_alone_in_every_language() {
        let out = through_wrapper(&["naive resume, \u{4f60}\u{597d}, \u{1f600}\n"]);
        assert_eq!(out, "naive resume, \u{4f60}\u{597d}, \u{1f600}\n");
    }

    #[test]
    fn an_event_with_no_terminator_does_not_gain_one() {
        let out = through_wrapper(&["no newline here"]);
        assert_eq!(out, "no newline here");
    }

    // The rest build a real `fmt` subscriber the way the binaries do, because
    // the unit tests above prove the wrapper and not the wiring.

    fn subscriber_lines(with_wrapper: bool, message: &str) -> Vec<String> {
        use tracing_subscriber::prelude::*;

        let sink = Collector::default();
        let layer = tracing_subscriber::fmt::layer().with_ansi(false);
        let dispatch = if with_wrapper {
            tracing::Dispatch::new(
                tracing_subscriber::registry().with(layer.with_writer(LineSafe::new(sink.clone()))),
            )
        } else {
            tracing::Dispatch::new(
                tracing_subscriber::registry().with(layer.with_writer(sink.clone())),
            )
        };
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(app_id = "genuine", "{}", message);
        });
        sink.lines()
    }

    #[test]
    fn a_forged_line_in_a_message_reaches_the_writer_as_one_line() {
        // The witness for the whole lane: this is exactly the shape an
        // application can put into a `console.log` or a queue name.
        let forgery = "ok\n2026-09-03T00:00:00Z  INFO forged app_id=other";
        let lines = subscriber_lines(true, forgery);
        assert_eq!(lines.len(), 1, "got {lines:#?}");
        assert!(
            lines[0].contains("ok\\n2026-09-03T00:00:00Z"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("app_id=\"genuine\""), "{}", lines[0]);
    }

    #[test]
    fn the_same_forgery_without_the_wrapper_is_two_lines() {
        // Pins the reason the wrapper exists. If a future `tracing-subscriber`
        // starts escaping LF in messages by itself this goes red, and whoever
        // sees it can decide whether to keep the wrapper as belt and braces.
        let forgery = "ok\n2026-09-03T00:00:00Z  INFO forged app_id=other";
        let lines = subscriber_lines(false, forgery);
        assert_eq!(lines.len(), 2, "got {lines:#?}");
    }

    /// What the pinned formatter escapes on its own, recorded as a fact rather
    /// than as a belief.
    ///
    /// If this goes red, `tracing-subscriber` changed what it sanitizes. The
    /// wrapper does not depend on the answer - it escapes everything either
    /// way - but the module documentation above cites this behaviour, and the
    /// documentation should not be allowed to drift into being wrong.
    #[test]
    fn the_formatter_escapes_esc_in_a_message_but_not_lf() {
        use tracing_subscriber::prelude::*;

        let sink = Collector::default();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(sink.clone()),
            ),
        );
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(bell = "a\u{7}b", "colour \u{1b}[31m and a newline \n here");
        });
        let text = sink.text();
        assert!(!text.contains('\u{1b}'), "ESC survived: {text:?}");
        assert!(!text.contains('\u{7}'), "BEL survived: {text:?}");
        assert!(
            text.matches('\n').count() == 2,
            "the formatter escaped LF after all: {text:?}"
        );
    }

    /// The other half of the evidence: the formatter's ESC handling is not
    /// uniform, which is why the wrapper escapes ESC rather than trusting it.
    ///
    /// A field recorded through `%value` is formatted by its `Display` impl,
    /// and that path does not go through `EscapeGuard`. If this ever goes red
    /// because a `tracing-subscriber` release sanitizes every field, the
    /// wrapper is still right to escape ESC - it just stops being the only
    /// thing standing between a `%`-recorded URL and the operator's terminal.
    #[test]
    fn the_formatter_does_not_escape_esc_in_a_display_recorded_field() {
        use tracing_subscriber::prelude::*;

        let sink = Collector::default();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(sink.clone()),
            ),
        );
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(url = %"http://x/\u{1b}[31m", "fetching");
        });
        let text = sink.text();
        assert!(
            text.contains('\u{1b}'),
            "the formatter now escapes Display fields too: {text:?}"
        );
    }

    /// `deis-control` runs with `LOG_FORMAT=json`, and there the protection is
    /// `serde_json`, not this wrapper: JSON string escaping turns a control
    /// character into a `\u00XX` *sequence of ASCII characters*, so nothing
    /// control-shaped ever reaches the writer. The wrapper is still installed
    /// on that branch, where it finds nothing to do; this test says so.
    #[test]
    fn json_output_carries_no_control_characters_of_its_own() {
        use tracing_subscriber::prelude::*;

        let sink = Collector::default();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(sink.clone()),
            ),
        );
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(app_id = "genuine", "ok\nforged \u{1b}[31m");
        });
        let text = sink.text();
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("\\n"), "{text:?}");
    }
}
