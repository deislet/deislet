use serde::{Deserialize, Serialize};

/// The instance lifecycle, as a table rather than as prose.
///
/// This is the one definition of what an instance may do next. deis-proxy
/// drives its own lifecycle straight off it (`deis_proxy::control::state`)
/// rather than keeping a second copy, because a second copy is a copy that
/// drifts: the one that used to live there had grown weaker than this table
/// without anyone noticing, accepting a transition on any event at all.
///
/// The inputs are events that happen *to* an instance, and the destination is
/// this table's to decide. That is the whole point of naming them: a caller
/// says what happened, never where the instance should end up, so an event
/// and the transition it is recorded against can no longer disagree.
///
/// `_Heartbeat` is spelled with a leading underscore on purpose. yasm leaves
/// underscore-prefixed inputs out of generated diagrams and transition
/// tables, and a heartbeat is not a step in the lifecycle - it is what keeps
/// the instance where it already is. Drawing it would put two self-loops on
/// the picture and hide the four transitions that matter.
pub mod instance_fsm {
    use yasm::*;

    define_state_machine! {
        name: InstanceFSM,
        states: { Pending, Active, Sunsetting, Terminated },
        inputs: { Registered, _Heartbeat, Shutdown, Silenced, Startup },
        initial: Pending,
        transitions: {
            Pending + Registered => Active,
            // Re-registration after the control plane went away and came
            // back. The instance never stopped serving, so it is still
            // active; what changed is only that the control plane knows
            // about it again. Without this row a proxy that lost its control
            // connection could never re-attach.
            Active + Registered => Active,
            Active + _Heartbeat => Active,
            Active + Shutdown => Sunsetting,
            Sunsetting + _Heartbeat => Sunsetting,
            Sunsetting + Silenced => Terminated,
            Terminated + Startup => Pending,
        }
    }
}

// Re-export for easy access
pub use instance_fsm::State as InstanceStatus;

use crate::NodeInfo;

/// Instance detailed information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceDetails {
    /// Instance ID
    pub id: String,
    /// Node Info
    pub info: NodeInfo,
    /// Current status
    pub status: InstanceStatus,
    /// Last heartbeat time
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(test)]
mod tests {
    use super::instance_fsm::{Input, InstanceFSM, State};
    use yasm::{StateMachine, StateMachineInstance, StateMachineQuery, TransitionError};

    /// The serialised form is what an in-flight `InstanceDetails` and any
    /// stored instance record carry. It is the variant name and nothing
    /// clever, and it must stay that way whatever happens to the table.
    #[test]
    fn state_serialises_as_its_bare_name() {
        for (state, expected) in [
            (State::Pending, "\"Pending\""),
            (State::Active, "\"Active\""),
            (State::Sunsetting, "\"Sunsetting\""),
            (State::Terminated, "\"Terminated\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), expected);
            let back: State = serde_json::from_str(expected).unwrap();
            assert_eq!(back, state);
        }
    }

    /// The event decides the destination; the caller does not get to name
    /// one. Reaching `Active` from `Pending` takes a registration and
    /// nothing else.
    #[test]
    fn only_registration_activates_a_pending_instance() {
        let mut sm = StateMachineInstance::<InstanceFSM>::new();

        assert_eq!(
            sm.transition(Input::Shutdown),
            Err(TransitionError::InvalidInput {
                state: State::Pending,
                input: Input::Shutdown,
            })
        );
        assert_eq!(
            sm.transition(Input::_Heartbeat),
            Err(TransitionError::InvalidInput {
                state: State::Pending,
                input: Input::_Heartbeat,
            })
        );

        assert_eq!(sm.transition(Input::Registered).unwrap(), State::Active);
    }

    /// A proxy whose control connection dropped registers again while it is
    /// still serving. That must be accepted and must not move it.
    #[test]
    fn re_registration_keeps_an_active_instance_active() {
        let mut sm = StateMachineInstance::<InstanceFSM>::new();
        sm.transition(Input::Registered).unwrap();

        assert_eq!(sm.transition(Input::Registered).unwrap(), State::Active);
    }

    /// The old hand-written copy of this table in deis-proxy looked only at
    /// the state pair, so `Active -> Active` was legal on any event at all,
    /// including one that means "stop". Here `Shutdown` from `Active` has
    /// exactly one destination.
    #[test]
    fn shutdown_from_active_always_sunsets() {
        assert_eq!(
            InstanceFSM::next_state(&State::Active, &Input::Shutdown),
            Some(State::Sunsetting)
        );
        assert_eq!(
            InstanceFSM::next_state(&State::Active, &Input::Silenced),
            None
        );
        assert_eq!(
            InstanceFSM::next_state(&State::Sunsetting, &Input::Shutdown),
            None
        );
    }

    /// The restart edge is what makes the lifecycle a cycle rather than a
    /// funnel, so nothing here is a dead end.
    #[test]
    fn every_state_can_reach_every_other() {
        assert!(StateMachineQuery::<InstanceFSM>::is_strongly_connected());
        assert!(StateMachineQuery::<InstanceFSM>::terminal_states().is_empty());
    }

    /// The diagram in `README.md` is generated from this table, so it must
    /// still be the same diagram. A picture of a lifecycle that no longer
    /// matches the lifecycle is worse than no picture: it is read and
    /// believed.
    ///
    /// Compared as a set of lines rather than as text because
    /// `generate_mermaid` walks a `HashMap`, so the order of the arrows is
    /// not stable between runs. What must not change is which arrows there
    /// are.
    #[test]
    fn the_readme_diagram_still_matches_the_table() {
        let generated = yasm::StateMachineDoc::<InstanceFSM>::generate_mermaid();

        for (name, readme) in [
            ("README.md", include_str!("../../README.md")),
            ("README.zh.md", include_str!("../../README.zh.md")),
        ] {
            let block = readme
                .split("```mermaid\n")
                .nth(1)
                .and_then(|rest| rest.split("```").next())
                .unwrap_or_else(|| panic!("{name} must carry the generated state diagram"));

            assert_eq!(
                sorted_lines(block),
                sorted_lines(&generated),
                "{name} and InstanceFSM have parted; regenerate the diagram from\n{generated}"
            );
        }
    }

    fn sorted_lines(mermaid: &str) -> Vec<String> {
        let mut lines: Vec<String> = mermaid
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }
}
