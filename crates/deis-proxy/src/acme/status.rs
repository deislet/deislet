//! The order and authorization statuses of RFC 8555, and what may follow
//! what.
//!
//! Section 7.1.6 of the RFC draws both of these as state machines, and this
//! client used to walk them with bare string comparisons in three separate
//! places. They had already drifted apart:
//!
//! * the order poll treated `ready` as "keep waiting" and an unrecognised
//!   status as a protocol error;
//! * the authorization poll treated `ready` as a hard failure - correctly,
//!   an authorization has no `ready` - and an unrecognised status as a
//!   challenge failure;
//! * a third comparison in `solve_authorization` looked only for `valid`.
//!
//! Two of those readings are right and the split was an accident, which is
//! exactly the shape of thing a table fixes. The legal statuses now live in
//! one place per resource, and so does the answer to "may the authority move
//! from this one to that one".
//!
//! What is deliberately *not* claimed here is authority over the status. The
//! authority owns it; this client only observes it. So the input to each
//! table is "the authority reported X", and the table's job is to say whether
//! X may follow what was last seen. An authority that moves an order
//! backwards - `processing` and then `pending` again - used to make this
//! client poll until its whole order timeout expired and then report a stall;
//! it is now named the moment it happens.
//!
//! The wire spelling is lower case, which is not what yasm's generated
//! `Display` produces, so [`OrderStatus::as_str`] and
//! [`AuthorizationStatus::as_str`] are written out and pinned to the state
//! list by tests. Nothing here is serialised by serde: these values are read
//! out of the authority's JSON and never written back.

use yasm::StateMachine;

/// The status of an order, RFC 8555 section 7.1.6 figure 3.
pub mod order_fsm {
    use yasm::*;

    define_state_machine! {
        name: OrderFSM,
        states: { Pending, Ready, Processing, Valid, Invalid },
        inputs: {
            ReportedPending,
            ReportedReady,
            ReportedProcessing,
            ReportedValid,
            ReportedInvalid
        },
        initial: Pending,
        transitions: {
            // Repeating the same status is the ordinary case: it is what
            // every poll that has not changed anything looks like.
            Pending + ReportedPending => Pending,
            Pending + ReportedReady => Ready,
            Pending + ReportedInvalid => Invalid,

            Ready + ReportedReady => Ready,
            Ready + ReportedProcessing => Processing,
            Ready + ReportedInvalid => Invalid,

            Processing + ReportedProcessing => Processing,
            Processing + ReportedValid => Valid,
            Processing + ReportedInvalid => Invalid,
        }
    }
}

/// The status of an authorization, RFC 8555 section 7.1.6 figure 2.
pub mod authorization_fsm {
    use yasm::*;

    define_state_machine! {
        name: AuthorizationFSM,
        states: { Pending, Processing, Valid, Invalid, Deactivated, Expired, Revoked },
        inputs: {
            ReportedPending,
            ReportedProcessing,
            ReportedValid,
            ReportedInvalid,
            ReportedDeactivated,
            ReportedExpired,
            ReportedRevoked
        },
        initial: Pending,
        transitions: {
            Pending + ReportedPending => Pending,
            // `processing` is not an authorization status in RFC 8555 - it
            // belongs to a challenge. It is accepted here because this client
            // accepted it before, and narrowing that would refuse an
            // authority it works with today over a distinction that costs
            // nothing to tolerate.
            Pending + ReportedProcessing => Processing,
            Pending + ReportedValid => Valid,
            Pending + ReportedInvalid => Invalid,
            Pending + ReportedDeactivated => Deactivated,
            Pending + ReportedExpired => Expired,

            Processing + ReportedProcessing => Processing,
            Processing + ReportedValid => Valid,
            Processing + ReportedInvalid => Invalid,

            // An authorization the authority is reusing can lapse or be
            // withdrawn under it. This client stops polling at `valid` so it
            // never watches that happen, but the rows are the RFC's and
            // leaving them out would make a legal answer look like a
            // protocol violation.
            Valid + ReportedValid => Valid,
            Valid + ReportedDeactivated => Deactivated,
            Valid + ReportedExpired => Expired,
            Valid + ReportedRevoked => Revoked,
        }
    }
}

pub use authorization_fsm::{AuthorizationFSM, State as AuthorizationStatus};
pub use order_fsm::{OrderFSM, State as OrderStatus};

/// An authority that moved a resource somewhere the RFC does not allow.
///
/// Carried rather than turned into an [`super::error::AcmeError`] here so
/// that each caller can raise the error its own resource deserves: a bad step
/// on an order is a protocol fault, and one on an authorization is a
/// challenge that will not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalStep {
    pub from: &'static str,
    pub to: &'static str,
}

impl std::fmt::Display for IllegalStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "it went from {} to {}, which RFC 8555 section 7.1.6 does not allow",
            self.from, self.to
        )
    }
}

impl OrderStatus {
    /// Parse the `status` member of an order.
    pub fn parse(status: &str) -> Option<Self> {
        Some(match status {
            "pending" => OrderStatus::Pending,
            "ready" => OrderStatus::Ready,
            "processing" => OrderStatus::Processing,
            "valid" => OrderStatus::Valid,
            "invalid" => OrderStatus::Invalid,
            _ => return None,
        })
    }

    /// The wire spelling, which is lower case.
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Ready => "ready",
            OrderStatus::Processing => "processing",
            OrderStatus::Valid => "valid",
            OrderStatus::Invalid => "invalid",
        }
    }

    fn reported(&self) -> order_fsm::Input {
        match self {
            OrderStatus::Pending => order_fsm::Input::ReportedPending,
            OrderStatus::Ready => order_fsm::Input::ReportedReady,
            OrderStatus::Processing => order_fsm::Input::ReportedProcessing,
            OrderStatus::Valid => order_fsm::Input::ReportedValid,
            OrderStatus::Invalid => order_fsm::Input::ReportedInvalid,
        }
    }

    /// Accept `reported` as this order's next status, or refuse the step.
    pub fn advance(&self, reported: OrderStatus) -> Result<OrderStatus, IllegalStep> {
        OrderFSM::next_state(self, &reported.reported()).ok_or(IllegalStep {
            from: self.as_str(),
            to: reported.as_str(),
        })
    }

    /// Whether the order has finished moving. `valid` and `invalid` are the
    /// two endings; everything else means poll again.
    pub fn is_settled(&self) -> bool {
        OrderFSM::valid_inputs(self).is_empty()
    }
}

impl AuthorizationStatus {
    /// Parse the `status` member of an authorization.
    pub fn parse(status: &str) -> Option<Self> {
        Some(match status {
            "pending" => AuthorizationStatus::Pending,
            "processing" => AuthorizationStatus::Processing,
            "valid" => AuthorizationStatus::Valid,
            "invalid" => AuthorizationStatus::Invalid,
            "deactivated" => AuthorizationStatus::Deactivated,
            "expired" => AuthorizationStatus::Expired,
            "revoked" => AuthorizationStatus::Revoked,
            _ => return None,
        })
    }

    /// The wire spelling, which is lower case.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthorizationStatus::Pending => "pending",
            AuthorizationStatus::Processing => "processing",
            AuthorizationStatus::Valid => "valid",
            AuthorizationStatus::Invalid => "invalid",
            AuthorizationStatus::Deactivated => "deactivated",
            AuthorizationStatus::Expired => "expired",
            AuthorizationStatus::Revoked => "revoked",
        }
    }

    fn reported(&self) -> authorization_fsm::Input {
        use authorization_fsm::Input;
        match self {
            AuthorizationStatus::Pending => Input::ReportedPending,
            AuthorizationStatus::Processing => Input::ReportedProcessing,
            AuthorizationStatus::Valid => Input::ReportedValid,
            AuthorizationStatus::Invalid => Input::ReportedInvalid,
            AuthorizationStatus::Deactivated => Input::ReportedDeactivated,
            AuthorizationStatus::Expired => Input::ReportedExpired,
            AuthorizationStatus::Revoked => Input::ReportedRevoked,
        }
    }

    /// Accept `reported` as this authorization's next status, or refuse the
    /// step.
    pub fn advance(&self, reported: AuthorizationStatus) -> Result<Self, IllegalStep> {
        AuthorizationFSM::next_state(self, &reported.reported()).ok_or(IllegalStep {
            from: self.as_str(),
            to: reported.as_str(),
        })
    }

    /// Whether the authority has decided. Only `pending` and `processing`
    /// mean poll again; every other status is an answer, right or wrong.
    pub fn is_decided(&self) -> bool {
        !matches!(
            self,
            AuthorizationStatus::Pending | AuthorizationStatus::Processing
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yasm::StateMachineQuery;

    /// The wire spellings, which are the authority's and not ours to change.
    #[test]
    fn every_status_round_trips_through_its_wire_spelling() {
        for state in OrderFSM::states() {
            assert_eq!(OrderStatus::parse(state.as_str()), Some(state.clone()));
            assert_eq!(state.as_str(), state.to_string().to_lowercase());
        }
        for state in AuthorizationFSM::states() {
            assert_eq!(
                AuthorizationStatus::parse(state.as_str()),
                Some(state.clone())
            );
            assert_eq!(state.as_str(), state.to_string().to_lowercase());
        }
    }

    #[test]
    fn an_unknown_status_does_not_parse() {
        assert_eq!(OrderStatus::parse("ready-ish"), None);
        assert_eq!(OrderStatus::parse(""), None);
        assert_eq!(AuthorizationStatus::parse("processing "), None);
    }

    /// Each input means "the authority reported this status", so a step that
    /// is allowed must land on exactly the status that was reported. This
    /// catches a mis-wired row long before a certificate does.
    #[test]
    fn an_accepted_step_always_lands_on_the_reported_status() {
        for from in OrderFSM::states() {
            for reported in OrderFSM::states() {
                if let Ok(next) = from.advance(reported.clone()) {
                    assert_eq!(next, reported, "from {from:?}");
                }
            }
        }
        for from in AuthorizationFSM::states() {
            for reported in AuthorizationFSM::states() {
                if let Ok(next) = from.advance(reported.clone()) {
                    assert_eq!(next, reported, "from {from:?}");
                }
            }
        }
    }

    /// The rule that did not exist before: an order moves forward. An
    /// authority that reports `pending` again after `processing` used to make
    /// this client poll until the whole order timeout ran out.
    #[test]
    fn an_order_cannot_go_backwards() {
        assert_eq!(
            OrderStatus::Processing.advance(OrderStatus::Pending),
            Err(IllegalStep {
                from: "processing",
                to: "pending"
            })
        );
        assert_eq!(
            OrderStatus::Processing.advance(OrderStatus::Ready),
            Err(IllegalStep {
                from: "processing",
                to: "ready"
            })
        );
        assert_eq!(
            OrderStatus::Ready.advance(OrderStatus::Pending),
            Err(IllegalStep {
                from: "ready",
                to: "pending"
            })
        );
    }

    /// The ordinary run: the finalize response says `processing`, the poll
    /// says `processing` for a while, and then the certificate is there.
    #[test]
    fn the_ordinary_order_run() {
        let mut status = OrderStatus::Processing;
        assert!(!status.is_settled());
        status = status.advance(OrderStatus::Processing).unwrap();
        status = status.advance(OrderStatus::Valid).unwrap();
        assert_eq!(status, OrderStatus::Valid);
        assert!(status.is_settled());
    }

    /// An order that ends has ended: nothing the authority says afterwards
    /// moves it, which is what lets the poll loop stop on `is_settled`.
    #[test]
    fn both_order_endings_are_final() {
        assert_eq!(
            StateMachineQuery::<OrderFSM>::terminal_states().len(),
            2,
            "valid and invalid, and nothing else, end an order"
        );
        for reported in OrderFSM::states() {
            assert!(OrderStatus::Valid.advance(reported.clone()).is_err());
            assert!(OrderStatus::Invalid.advance(reported).is_err());
        }
    }

    /// An order has a `ready` and an authorization does not. That difference
    /// used to be two unrelated string lists that happened to be right.
    #[test]
    fn ready_belongs_to_orders_and_not_to_authorizations() {
        assert!(OrderStatus::parse("ready").is_some());
        assert!(AuthorizationStatus::parse("ready").is_none());
    }

    /// The order diagram in `README.md` is generated from this table, so it
    /// must still be the same diagram. It is what an operator reads after
    /// finding "RFC 8555 section 7.1.6 does not allow" in a log, so a picture
    /// that has parted from the table would send them looking in the wrong
    /// place.
    ///
    /// Compared as a set of arrows rather than as text: `generate_mermaid`
    /// walks a `HashMap`, so the order it emits them in is not stable between
    /// runs.
    #[test]
    fn the_readme_order_diagram_still_matches_the_table() {
        let generated = yasm::StateMachineDoc::<OrderFSM>::generate_mermaid();
        let readme = include_str!("../../README.md");

        let found = readme
            .split("```mermaid\n")
            .skip(1)
            .filter_map(|rest| rest.split("```").next())
            .any(|block| sorted_lines(block) == sorted_lines(&generated));

        assert!(
            found,
            "README.md has no diagram matching OrderFSM any more; the current one is\n\n```mermaid\n{generated}```"
        );
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

    /// Only `pending` and `processing` mean the authority has not answered.
    #[test]
    fn an_authorization_is_decided_by_anything_but_pending_and_processing() {
        assert!(!AuthorizationStatus::Pending.is_decided());
        assert!(!AuthorizationStatus::Processing.is_decided());
        for decided in [
            AuthorizationStatus::Valid,
            AuthorizationStatus::Invalid,
            AuthorizationStatus::Deactivated,
            AuthorizationStatus::Expired,
            AuthorizationStatus::Revoked,
        ] {
            assert!(decided.is_decided(), "{decided:?}");
        }
    }
}
