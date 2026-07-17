//! Listener management
//!
//! Manages TCP listeners, supporting rebuilding listeners from file descriptors.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Listener manager
pub struct ListenerManager {
    /// Active listeners
    listeners: HashMap<SocketAddr, TcpListener>,
}

impl ListenerManager {
    /// Create a new listener manager
    pub fn new() -> Self {
        Self {
            listeners: HashMap::new(),
        }
    }

    /// Add a listener
    pub fn add_listener(&mut self, addr: SocketAddr, listener: TcpListener) {
        debug!("Added listener for {}", addr);
        self.listeners.insert(addr, listener);
    }

    /// Get all listener addresses
    pub fn get_addresses(&self) -> Vec<SocketAddr> {
        self.listeners.keys().copied().collect()
    }

    /// Get number of listeners
    pub fn count(&self) -> usize {
        self.listeners.len()
    }

    /// Extract file descriptors of all listeners
    ///
    /// Returns (FD list, address list)
    pub fn extract_fds(&self) -> (Vec<RawFd>, Vec<SocketAddr>) {
        let mut fds = Vec::new();
        let mut addrs = Vec::new();

        for (addr, listener) in &self.listeners {
            // tokio::net::TcpListener implements AsRawFd
            let fd = listener.as_raw_fd();

            fds.push(fd);
            addrs.push(*addr);

            debug!("Extracted FD {} for {}", fd, addr);
        }

        info!("Extracted {} FDs", fds.len());

        (fds, addrs)
    }

    /// Rebuild listeners from file descriptors
    ///
    /// # Safety
    /// - FD must be a valid TCP listener file descriptor
    /// - Caller must ensure the FD is not closed elsewhere
    pub async fn rebuild_from_fds(
        &mut self,
        addr_to_fd: HashMap<SocketAddr, RawFd>,
    ) -> Result<()> {
        info!("Rebuilding {} listeners from FDs", addr_to_fd.len());

        for (addr, fd) in addr_to_fd {
            let listener = unsafe {
                // Create std::net::TcpListener from raw FD
                let std_listener = std::net::TcpListener::from_raw_fd(fd);

                // Set to non-blocking mode
                std_listener
                    .set_nonblocking(true)
                    .with_context(|| format!("Failed to set non-blocking for {}", addr))?;

                // Convert to tokio::net::TcpListener
                TcpListener::from_std(std_listener)
                    .with_context(|| format!("Failed to create Tokio listener for {}", addr))?
            };

            info!("Rebuilt listener for {} from FD {}", addr, fd);
            self.add_listener(addr, listener);
        }

        Ok(())
    }

    /// Take all listeners (for transferring ownership)
    pub fn take_all(&mut self) -> Vec<(TcpListener, SocketAddr)> {
        let listeners = std::mem::take(&mut self.listeners);
        listeners
            .into_iter()
            .map(|(addr, listener)| (listener, addr))
            .collect()
    }

    /// Close all listeners
    pub fn close_all(&mut self) {
        let count = self.listeners.len();
        self.listeners.clear();
        info!("Closed {} listeners", count);
    }
}

impl Default for ListenerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_listener_manager_basic() {
        let mut manager = ListenerManager::new();

        let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr1 = listener1.local_addr().unwrap();

        manager.add_listener(addr1, listener1);

        assert_eq!(manager.count(), 1);
        assert_eq!(manager.get_addresses(), vec![addr1]);
    }

    // Note: This test cannot be correctly implemented in unit tests because FD transfer and rebuilding
    // need to happen in different processes. In the same process, even with std::mem::forget,
    // tokio's internal state management will cause conflicts.
    // Real FD transfer and rebuilding functionality will be verified in integration tests.
    //
    // #[tokio::test]
    // async fn test_extract_and_rebuild_fds() {
    //     // This test needs to be verified in integration tests using real IPC
    // }
}
