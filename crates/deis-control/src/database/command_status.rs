//! What a command's status may be, and what may follow what.
//!
//! A command is created `Pending`, goes down a node's gRPC stream, and ends
//! in exactly one of three terminal statuses. That was true before this
//! module existed; what was missing is that nothing enforced it. Every call
//! site named the status it wanted and [`crate::database::Database`] wrote
//! it, so the last writer won - and two of the writers race by design.
//!
//! The two races are real, not theoretical. `CommandExecutor::execute_command`
//! writes the outcome of its own `tokio::time::timeout`, while
//! `CommandExecutor::handle_response` writes the outcome the node reported,
//! and both run when a node answers at about the moment the deadline passes.
//! Separately, the `Sent` write happens *after* the message is already on the
//! stream, so a node that answers quickly can have its `Completed` overwritten
//! by the `Sent` that was still on its way to SQLite - which also blanks
//! `completed_at`, because `Sent` is not a terminal status.
//!
//! So the statuses are a yasm table now, and the events drive it. A status is
//! never named by a caller: a caller says what happened - the command went
//! out, the node answered, the deadline passed - and the table says where
//! that leaves it. A terminal status has no outgoing row, which is the whole
//! rule in one line: **the first ending wins, and a late write for an earlier
//! step is dropped.**
//!
//! The state names are the strings in the `commands.status` column of an
//! existing SQLite file. They are the Rust variant names spelled exactly, and
//! [`CommandStatus::as_str`] is pinned to them by a test, because renaming
//! one would make every command already in a database unreadable.

use yasm::StateMachine;

use super::DatabaseError;

pub mod command_fsm {
    use yasm::*;

    define_state_machine! {
        name: CommandFSM,
        states: { Pending, Sent, Acknowledged, Completed, Failed, Timeout },
        inputs: { Delivered, NodeAcknowledged, Succeeded, Failed, Expired },
        initial: Pending,
        transitions: {
            Pending + Delivered => Sent,
            // A node can answer before the `Sent` write reaches SQLite, so
            // every ending is reachable straight from `Pending`. Without
            // these rows a command that completed in under a database round
            // trip would be stuck at `Pending` for ever.
            Pending + Succeeded => Completed,
            Pending + Failed => Failed,
            Pending + Expired => Timeout,

            Sent + NodeAcknowledged => Acknowledged,
            Sent + Succeeded => Completed,
            Sent + Failed => Failed,
            Sent + Expired => Timeout,

            // Nothing sends `NodeAcknowledged` today - the wire has no
            // separate acknowledgement message, only `CommandResponse`. The
            // status is here because it is a value an existing database may
            // hold and `from_str` has always accepted it, and these rows are
            // here so that such a row can still reach an ending. Leaving it
            // with no way out would be a step backwards from the blind write
            // this table replaces.
            Acknowledged + Succeeded => Completed,
            Acknowledged + Failed => Failed,
            Acknowledged + Expired => Timeout,
        }
    }
}

/// A command's lifecycle status, as persisted in `commands.status`.
pub use command_fsm::State as CommandStatus;

/// What happened to a command. The status it leads to is the table's answer.
pub use command_fsm::Input as CommandEvent;

pub use command_fsm::CommandFSM;

impl CommandStatus {
    /// The exact string this status is stored and reported as.
    ///
    /// Written out rather than derived from `Display` because it is borrowed
    /// static data on a hot-ish path, and pinned to `Display` by
    /// `as_str_matches_the_state_name` below so the two can never part.
    pub fn as_str(&self) -> &'static str {
        match self {
            CommandStatus::Pending => "Pending",
            CommandStatus::Sent => "Sent",
            CommandStatus::Acknowledged => "Acknowledged",
            CommandStatus::Completed => "Completed",
            CommandStatus::Failed => "Failed",
            CommandStatus::Timeout => "Timeout",
        }
    }

    /// Parse a status back out of the database.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, DatabaseError> {
        s.parse().map_err(|_| DatabaseError::ParseError {
            message: format!("Invalid command status: {}", s),
        })
    }

    /// Where `event` leaves a command in this status, or `None` if this
    /// status has no row for it.
    pub fn apply(&self, event: &CommandEvent) -> Option<Self> {
        CommandFSM::next_state(self, event)
    }

    /// Whether this status is an ending: nothing may follow it.
    ///
    /// Also what decides whether `completed_at` is stamped, which used to be
    /// a second hand-written list of the three terminal statuses beside the
    /// UPDATE that wrote them.
    pub fn is_terminal(&self) -> bool {
        CommandFSM::valid_inputs(self).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yasm::StateMachineQuery;

    /// The stored spelling. A database written by any earlier build holds
    /// these exact strings, and an in-flight command reports them over the
    /// HTTP API.
    #[test]
    fn as_str_matches_the_state_name() {
        for state in CommandFSM::states() {
            assert_eq!(state.as_str(), state.to_string());
            assert_eq!(CommandStatus::from_str(state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn the_stored_spellings_are_exactly_the_six_that_were_there_before() {
        let names: Vec<String> = CommandFSM::states()
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "Pending",
                "Sent",
                "Acknowledged",
                "Completed",
                "Failed",
                "Timeout"
            ]
        );
    }

    #[test]
    fn an_unknown_string_is_a_parse_error() {
        let error = CommandStatus::from_str("Cancelled").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Parse error: Invalid command status: Cancelled"
        );
    }

    /// The rule the blind `UPDATE` did not have: an ending is an ending.
    #[test]
    fn nothing_follows_a_terminal_status() {
        for terminal in [
            CommandStatus::Completed,
            CommandStatus::Failed,
            CommandStatus::Timeout,
        ] {
            assert!(terminal.is_terminal(), "{terminal:?}");
            for event in CommandFSM::inputs() {
                assert_eq!(
                    terminal.apply(&event),
                    None,
                    "{terminal:?} must not accept {event:?}"
                );
            }
        }

        assert_eq!(
            StateMachineQuery::<CommandFSM>::terminal_states().len(),
            3,
            "Pending, Sent and Acknowledged must all still have a way out"
        );
    }

    /// The `Sent` write is issued after the message is already on the wire,
    /// so it can land after the response did. It must not undo the ending.
    #[test]
    fn a_late_delivery_write_cannot_undo_a_completed_command() {
        assert_eq!(
            CommandStatus::Pending.apply(&CommandEvent::Succeeded),
            Some(CommandStatus::Completed)
        );
        assert_eq!(
            CommandStatus::Completed.apply(&CommandEvent::Delivered),
            None
        );
    }

    /// Both directions of the completion/timeout race end with the record
    /// agreeing with whatever the caller was told first.
    #[test]
    fn whichever_ending_arrives_first_is_the_one_that_stands() {
        assert_eq!(
            CommandStatus::Sent.apply(&CommandEvent::Succeeded),
            Some(CommandStatus::Completed)
        );
        assert_eq!(CommandStatus::Completed.apply(&CommandEvent::Expired), None);

        assert_eq!(
            CommandStatus::Sent.apply(&CommandEvent::Expired),
            Some(CommandStatus::Timeout)
        );
        assert_eq!(CommandStatus::Timeout.apply(&CommandEvent::Succeeded), None);
    }

    /// Every status must be reachable from where a command starts, or it is
    /// a value the database can hold and the code can never produce.
    #[test]
    fn every_status_is_reachable_from_pending() {
        let reachable = StateMachineQuery::<CommandFSM>::reachable_states(&CommandStatus::Pending);
        for state in CommandFSM::states() {
            assert!(reachable.contains(&state), "{state:?} is unreachable");
        }
    }
}
