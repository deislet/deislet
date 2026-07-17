//! Load balancing utilities
//!
//! This module provides helper utilities for load balancing operations.
//! The actual load balancing is handled by Pingora's built-in load balancer.

use tracing::debug;

/// Helper functions for load balancing
pub struct BalancerHelper;

impl BalancerHelper {
    /// Create a new balancer helper
    pub fn new() -> Self {
        Self
    }

    /// Generate a session ID for sticky sessions
    pub fn generate_session_id() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                abcdefghijklmnopqrstuvwxyz\
                                0123456789";
        const SESSION_ID_LEN: usize = 32;

        let mut rng = rand::rng();
        let session_id: String = (0..SESSION_ID_LEN)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect();

        debug!("Generated session ID: {}", session_id);
        session_id
    }
}

impl Default for BalancerHelper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_generation() {
        let _helper = BalancerHelper::new();
        let session_id = BalancerHelper::generate_session_id();

        // Session ID should be 32 characters long
        assert_eq!(session_id.len(), 32);

        // Should contain only alphanumeric characters
        assert!(session_id.chars().all(|c| c.is_alphanumeric()));

        // Generate another one to ensure they're different
        let session_id2 = BalancerHelper::generate_session_id();
        assert_ne!(session_id, session_id2);
    }

    #[test]
    fn test_helper_creation() {
        let _helper = BalancerHelper::new();
        let _helper2 = BalancerHelper;

        // Both should be created successfully
        // (No specific assertions needed as creation itself is the test)
    }
}
