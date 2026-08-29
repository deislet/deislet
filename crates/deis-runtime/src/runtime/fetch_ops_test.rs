#[cfg(test)]
mod tests {
    use crate::runtime::fetch_ops::*;
    use std::time::Duration;

    /// The margin is what buys the handler its `catch` block. Too small and
    /// the abort and the isolate's own termination race; too large and an
    /// application with a short timeout loses most of it to a reserve it
    /// never asked for.
    #[test]
    fn a_call_gets_its_budget_less_a_margin_to_answer_in() {
        // The platform default. A tenth would be three seconds, which is far
        // more than a `catch` and a `return` need, so the cap applies.
        assert_eq!(
            allowance(Duration::from_secs(30)),
            Duration::from_millis(29_500)
        );

        // In the middle: a tenth, because it is between the floor and the cap.
        assert_eq!(
            allowance(Duration::from_secs(2)),
            Duration::from_millis(1_800)
        );

        // Short enough that a tenth would be 30ms, which is not long enough to
        // be worth calling a margin, so the floor applies.
        assert_eq!(
            allowance(Duration::from_millis(300)),
            Duration::from_millis(250)
        );
    }

    /// A budget already spent must not wrap around into an enormous one.
    #[test]
    fn a_budget_smaller_than_the_margin_leaves_nothing() {
        assert_eq!(allowance(Duration::from_millis(40)), Duration::ZERO);
        assert_eq!(allowance(Duration::ZERO), Duration::ZERO);
    }

    /// The margin never grows without bound: whatever the operator sets, the
    /// reserve stays a slice and the call keeps the rest.
    #[test]
    fn the_margin_stays_a_slice_of_even_a_very_long_budget() {
        let remaining = Duration::from_secs(600);
        assert_eq!(remaining - allowance(remaining), MAX_REPLY_MARGIN);
    }
}
