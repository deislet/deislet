//! The state diagrams in `ARCHITECTURE.md` are generated, so they must still
//! match what generated them.
//!
//! A diagram of a lifecycle is read and believed. One that has quietly parted
//! from the table it describes is worse than no diagram, and a table gains a
//! row far more easily than a document does - which is the whole reason these
//! pictures come out of `yasm` rather than out of somebody's head.
//!
//! The comparison is on the *set* of arrows rather than on the text, because
//! `generate_mermaid` walks a `HashMap` and the order it emits them in is not
//! stable between runs. Which arrows exist is what must not change.

use deis_control::NodeFSM;
use deis_control::database::CommandFSM;
use yasm::StateMachineDoc;

const ARCHITECTURE: &str = include_str!("../ARCHITECTURE.md");

/// Every fenced `mermaid` block in the document, as sorted arrow lists.
fn diagrams_in_the_document() -> Vec<Vec<String>> {
    ARCHITECTURE
        .split("```mermaid\n")
        .skip(1)
        .filter_map(|rest| rest.split("```").next())
        .map(sorted_lines)
        .collect()
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

#[test]
fn the_node_session_diagram_still_matches_the_table() {
    let generated = StateMachineDoc::<NodeFSM>::generate_mermaid();

    assert!(
        diagrams_in_the_document().contains(&sorted_lines(&generated)),
        "ARCHITECTURE.md has no diagram matching NodeFSM any more; the current one is\n\n```mermaid\n{generated}```"
    );
}

#[test]
fn the_command_lifecycle_diagram_still_matches_the_table() {
    let generated = StateMachineDoc::<CommandFSM>::generate_mermaid();

    assert!(
        diagrams_in_the_document().contains(&sorted_lines(&generated)),
        "ARCHITECTURE.md has no diagram matching CommandFSM any more; the current one is\n\n```mermaid\n{generated}```"
    );
}
