//! `deis queue dead-letters|replay` - the messages a queue gave up on.
//!
//! A message that fails more times than the queue's ceiling allows is not
//! deleted: deis-queue moves it to a dead letter table, where it waits for a
//! person. Until these two commands existed nothing outside deis-queue's own
//! tests ever read that table, which made it the place work went to
//! disappear - the platform stopped retrying, said so in one log line, and
//! there was no way to find out afterwards what was in there short of opening
//! `queue.sqlite` with the sqlite3 shell on whichever host it lives on.
//!
//! The two halves of what an operator has to do are the two subcommands:
//!
//! * `dead-letters` answers "what is stuck, and why" - which queue, which
//!   message, how many deliveries it took before the queue gave up, when it
//!   arrived and when it died, which of the two ways it died, and what is in
//!   the body.
//! * `replay` puts named messages back, for after the cause has been fixed.
//!
//! Ids are named on `replay` one by one, never `--all`. deis-queue itself
//! refuses a replay that names none, for the reason this command offers no
//! shortcut either: putting a whole queue of poison messages back cannot be
//! undone, and the messages come back with a fresh set of delivery attempts,
//! so whatever made them fail is about to be tried again.

use anyhow::Result;
use colored::Colorize;

use crate::commands::Session;
use crate::platform::queue::DeadLetter;
use crate::render::{BytesForDisplay, Output, age, base64_encode, bytes_for_display, timestamp};
use crate::settings::Overrides;

/// How much of a body one line of the report shows.
///
/// Enough to recognise which job it was - the key, the customer, the order
/// number are all near the front of anything an application enqueues - and
/// not so much that one message pushes the rest off the screen. `--json`
/// carries every byte, so nothing is lost by cutting here.
const BODY_PREVIEW_CHARS: usize = 80;

pub struct DeadLettersArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    /// One queue, or `None` for every queue this tenant has. `None` is the
    /// view to start from: the question is usually "what is stuck?", not
    /// "what is stuck in this one queue".
    pub queue: Option<String>,
    /// Messages per page asked of deis-queue. It clamps this to its own
    /// ceiling, so a large value is a request rather than a promise.
    pub limit: Option<i32>,
    /// Resume from the cursor a previous page answered with.
    pub cursor: Option<String>,
    /// Keep asking for pages until the table ends, instead of stopping after
    /// one.
    pub all: bool,
}

pub async fn dead_letters(args: DeadLettersArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: None,
    })?;
    let tenant = session.tenant()?;
    let queue = session.queue().await?;

    let mut messages: Vec<DeadLetter> = Vec::new();
    let mut cursor = args.cursor;
    let mut pages = 0usize;
    loop {
        let page = queue
            .dead_letters(args.queue.as_deref(), args.limit, cursor)
            .await?;
        pages += 1;
        messages.extend(page.messages);
        cursor = page.next_cursor;
        // The cursor is the end signal, not a short page: deis-queue also
        // cuts a page short when the bodies would make the response too large
        // to send, so a page can be short and still have more behind it.
        if cursor.is_none() || !args.all {
            break;
        }
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "queue": args.queue,
            "pages": pages,
            "messages": messages
                .iter()
                .map(|message| serde_json::json!({
                    "queue": message.queue,
                    "id": message.id,
                    "attempts": message.attempts,
                    "created_at": message.created_at,
                    "died_at": message.died_at,
                    "reason": message.reason,
                    "bytes": message.body.len(),
                    // The whole body, base64: this is the document a person
                    // pipes into jq to work out what the poison actually was,
                    // and the human report only ever shows the first line of
                    // it.
                    "body_base64": base64_encode(&message.body),
                }))
                .collect::<Vec<_>>(),
            "cursor": cursor,
        }));
    }

    println!(
        "\n{} {}",
        "Dead letters in".bold(),
        match &args.queue {
            Some(queue) => format!("{tenant}, queue {queue}"),
            None => tenant.clone(),
        }
        .cyan()
    );

    if messages.is_empty() {
        println!("\n  {}", "none - nothing has been given up on".dimmed());
        return Ok(());
    }

    // One block per message rather than one row: the reason is a sentence and
    // the body is whatever the producer sent, and neither fits a column that
    // every other message has to share.
    for message in &messages {
        println!("\n  {}  {}", message.queue.cyan(), message.id.bold());
        println!(
            "    {}",
            format!(
                "{} attempt(s), enqueued {}, died {} ({})",
                message.attempts,
                timestamp(message.created_at),
                timestamp(message.died_at),
                age(message.died_at)
            )
            .dimmed()
        );
        // The reason, on its own line and undimmed: it is the one field that
        // says which half of the system to go and look at, and an operator
        // scanning this report is looking for exactly it.
        println!("    {}", why(message));
        println!(
            "    {}",
            format!("body  {}", body_preview(&message.body)).dimmed()
        );
    }

    println!();
    match &cursor {
        // Named rather than merely reported: this is the value a person
        // passes to --cursor, and without it a truncated table looks like a
        // complete one - which for this table means believing the queue has
        // given up on less work than it has.
        Some(cursor) => println!(
            "  {}",
            format!(
                "{} shown, more to come. Continue with --cursor {cursor}, or ask for --all.",
                messages.len()
            )
            .dimmed()
        ),
        None => println!(
            "  {}",
            format!("{} shown, end of the table.", messages.len()).dimmed()
        ),
    }
    // The next command, spelled out with the queue that is actually on
    // screen. Nothing here is retried on its own, so a report that stopped at
    // "this is stuck" would leave the operator to guess how to unstick it.
    println!(
        "  {}",
        format!(
            "Put chosen messages back with `deis queue replay --queue {} <id>...` once the cause \
             is fixed.",
            one_queue(&messages).unwrap_or("<queue>")
        )
        .dimmed()
    );
    Ok(())
}

/// The queue every message on screen belongs to, or `None` when they came
/// from more than one.
///
/// A replay names one queue, so the hint under the table may only fill that
/// in when there is nothing to choose between. Naming the first message's
/// queue while the table also holds another one would hand the operator a
/// command that silently does nothing for half of what they just read.
fn one_queue(messages: &[DeadLetter]) -> Option<&str> {
    let first = messages.first()?.queue.as_str();
    messages
        .iter()
        .all(|message| message.queue == first)
        .then_some(first)
}

pub struct ReplayArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub queue: String,
    /// Ids from `deis queue dead-letters`. Named one by one, deliberately:
    /// see the module docs.
    pub ids: Vec<String>,
    /// Hold the messages back this many seconds before a consumer may have
    /// them, for putting work back while the fix is still rolling out.
    pub delay: Option<i32>,
}

pub async fn replay(args: ReplayArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: None,
    })?;
    let tenant = session.tenant()?;
    let queue = session.queue().await?;

    let named = args.ids.len();
    let report = queue.replay(&args.queue, args.ids, args.delay).await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "queue": args.queue,
            "named": named,
            "replayed": report.replayed,
            "not_found": report.not_found,
            "delay_seconds": args.delay,
        }));
    }

    // Nothing moved is not a success with a zero in it: an operator replaying
    // ids read off an older listing has to be told plainly that the queue is
    // unchanged, rather than shown a tick and left to notice the number.
    if report.replayed == 0 {
        println!(
            "\n  {}",
            format!(
                "none of the {named} id(s) named are in the dead letter table of queue {} in \
                 {tenant}; nothing was put back. They were replayed already, deleted by the \
                 retention sweep, or never there.",
                args.queue
            )
            .yellow()
        );
        return Ok(());
    }

    println!(
        "\n  {} {} of {} message(s) back on queue {} in {}",
        "✓".green(),
        report.replayed,
        named,
        args.queue.cyan(),
        tenant.cyan()
    );
    // Said every time, not only when something looks wrong: a replayed
    // message starts its attempts again, so the failure that put it here is
    // about to be tried again as many times as the first one was.
    println!(
        "  {}",
        match args.delay {
            Some(delay) if delay > 0 => format!(
                "They become visible to a consumer in {delay}s, each with a fresh set of delivery \
                 attempts."
            ),
            _ => "They are visible to a consumer now, each with a fresh set of delivery attempts."
                .to_string(),
        }
        .dimmed()
    );
    if report.not_found > 0 {
        println!(
            "  {}",
            format!(
                "The other {} id(s) were not in the dead letter table: already replayed, already \
                 deleted by the retention sweep, or never there. Those messages are not back on \
                 the queue.",
                report.not_found
            )
            .yellow()
        );
    }
    Ok(())
}

/// The reason a message was given up on, as a sentence.
///
/// A queue too old to have recorded one is said to be exactly that, rather
/// than left blank: a blank cell reads as "no reason", which would suggest
/// the message died for no reason at all.
fn why(message: &DeadLetter) -> String {
    match &message.reason {
        Some(reason) => reason.clone(),
        None => "no reason was recorded; this message was given up on by a queue that did not \
                 keep one"
            .to_string(),
    }
}

/// One line of a message body, safe to put on a terminal.
///
/// A dead letter body is whatever the producer sent, and the bodies that end
/// up here are disproportionately the malformed ones. Two things therefore
/// never reach the terminal raw: bytes that are not UTF-8, which is what
/// `bytes_for_display` already decides for `deis kv` and `deis blob`, and the
/// control characters inside a body that *is* text - a newline would break
/// the report into pieces that no longer line up with the message they belong
/// to, and an escape sequence would repaint the operator's screen.
fn body_preview(data: &[u8]) -> String {
    match bytes_for_display(data) {
        BytesForDisplay::Binary { len, preview_hex } => {
            format!("{len} bytes, not valid UTF-8 (starts {preview_hex}...)")
        }
        BytesForDisplay::Text(text) => {
            let mut preview = String::new();
            // Two counts, because escaping one character can produce six:
            // `shown` is what the terminal will see and is what the ceiling
            // is on, `taken` is how far into the body that got. Counting only
            // the source characters would let a body of eighty control bytes
            // print as a line of nearly five hundred.
            let mut shown = 0usize;
            let mut taken = 0usize;
            for character in text.chars() {
                if shown >= BODY_PREVIEW_CHARS {
                    break;
                }
                if character.is_control() {
                    for escaped in character.escape_debug() {
                        preview.push(escaped);
                        shown += 1;
                    }
                } else {
                    preview.push(character);
                    shown += 1;
                }
                taken += 1;
            }
            let cut = text.chars().count() > taken;
            format!(
                "{preview}{} ({} bytes)",
                if cut { "..." } else { "" },
                data.len()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead(reason: Option<&str>) -> DeadLetter {
        DeadLetter {
            queue: "jobs".to_string(),
            id: "m1".to_string(),
            body: b"{}".to_vec(),
            attempts: 5,
            created_at: 1_700_000_000,
            died_at: 1_700_000_100,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn the_recorded_reason_is_shown_as_the_queue_worded_it() {
        assert_eq!(
            why(&dead(Some("the consumer reported it failed"))),
            "the consumer reported it failed"
        );
    }

    #[test]
    fn a_message_from_a_queue_that_recorded_no_reason_says_so() {
        // Not a blank line: blank reads as "died for no reason", which is
        // the one thing that cannot have happened.
        let rendered = why(&dead(None));
        assert!(rendered.contains("no reason was recorded"), "{rendered}");
    }

    #[test]
    fn the_replay_hint_names_a_queue_only_when_there_is_one_to_name() {
        let mut jobs = dead(Some("the consumer reported it failed"));
        jobs.queue = "jobs".to_string();
        let mut emails = jobs.clone();
        emails.queue = "emails".to_string();

        assert_eq!(one_queue(&[jobs.clone()]), Some("jobs"));
        assert_eq!(
            one_queue(&[jobs.clone(), jobs.clone()]),
            Some("jobs"),
            "several messages of one queue still name it"
        );
        assert_eq!(
            one_queue(&[jobs, emails]),
            None,
            "a hint naming one of two queues would do nothing for half the table"
        );
        assert_eq!(one_queue(&[]), None);
    }

    #[test]
    fn a_body_that_is_text_is_shown_with_its_size() {
        assert_eq!(
            body_preview(b"{\"key\":\"a\"}"),
            "{\"key\":\"a\"} (11 bytes)"
        );
    }

    #[test]
    fn a_newline_in_a_body_cannot_break_the_report_into_pieces() {
        // The failure this prevents: one message's body printing as three
        // lines, two of which look like they belong to the message below it.
        let rendered = body_preview(b"first\nsecond");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(rendered.starts_with("first\\nsecond"), "{rendered}");
    }

    #[test]
    fn an_escape_sequence_in_a_body_cannot_repaint_the_screen() {
        let rendered = body_preview(b"\x1b[31mred");
        assert!(!rendered.contains('\x1b'), "{rendered}");
        assert!(rendered.contains("\\u{1b}[31mred"), "{rendered}");
    }

    #[test]
    fn a_long_body_is_cut_but_still_reports_its_whole_size() {
        let body = "x".repeat(500);
        let rendered = body_preview(body.as_bytes());
        assert!(
            rendered.starts_with(&"x".repeat(BODY_PREVIEW_CHARS)),
            "{rendered}"
        );
        assert!(rendered.contains("... (500 bytes)"), "{rendered}");
    }

    #[test]
    fn a_body_of_control_characters_cannot_grow_past_the_ceiling() {
        // Every one of these escapes to two characters, so a preview that
        // counted the body's own characters would print twice what it
        // promised - and six times for the ones that escape to \u{..}.
        let rendered = body_preview(&b"\n".repeat(200));
        let body = rendered.split(" (200 bytes)").next().unwrap();
        assert!(
            body.chars().count() <= BODY_PREVIEW_CHARS + "...".len(),
            "{rendered}"
        );
        assert!(rendered.ends_with("... (200 bytes)"), "{rendered}");
    }

    #[test]
    fn a_body_that_is_not_text_is_described_rather_than_dumped() {
        // Exactly the kind of message that ends up dead lettered: 0xff can
        // never start a valid UTF-8 sequence.
        let rendered = body_preview(&[0xff, 0x00, 0x01]);
        assert!(rendered.contains("not valid UTF-8"), "{rendered}");
        assert!(rendered.contains("ff 00 01"), "{rendered}");
    }
}
