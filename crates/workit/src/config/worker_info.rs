use serde::{Deserialize, Serialize};

/// Worker Type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerType {
    Denix(String), // Javascript Runtime Worker, The inner string is the version of the worker
    Deflux(String), // LB / HttpProxy Worker, The inner string is the version of the worker
}

impl WorkerType {
    /// Get the version string from the worker type
    pub fn version(&self) -> &str {
        match self {
            WorkerType::Denix(v) => v,
            WorkerType::Deflux(v) => v,
        }
    }
}

/// Worker instance information
///
/// Contains metadata about the worker instance including version,
/// listening address, and port configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    /// Worker Type
    pub worker_type: WorkerType,
    /// Listen host, e.g., "0.0.0.0" or "127.0.0.1"
    pub listen_host: String,
    /// Port numbers that the worker listens on, e.g., [80, 443]
    pub listen_port: Vec<u16>,
}

impl WorkerInfo {
    /// Create new worker information with multiple ports
    ///
    /// # Arguments
    ///
    /// * `worker_type` - Type of the worker, including its version
    /// * `listen_host` - Host address to listen on
    /// * `listen_port` - Vector of ports to listen on
    ///
    /// # Examples
    ///
    /// ```rust
    /// use workit::{WorkerInfo, WorkerType};
    ///
    /// let worker_info = WorkerInfo::new(
    ///     WorkerType::Denix("1.0.0".to_string()),
    ///     "0.0.0.0".to_string(),
    ///     vec![80, 443],
    /// );
    /// ```
    pub fn new(worker_type: WorkerType, listen_host: String, listen_port: Vec<u16>) -> Self {
        Self {
            worker_type,
            listen_host,
            listen_port,
        }
    }

    /// Create new worker information with a single port (convenience method)
    ///
    /// # Arguments
    ///
    /// * `worker_type` - Type of the worker, including its version
    /// * `listen_host` - Host address to listen on
    /// * `port` - Single port to listen on
    ///
    /// # Examples
    ///
    /// ```rust
    /// use workit::{WorkerInfo, WorkerType};
    ///
    /// let worker_info = WorkerInfo::new_single_port(
    ///     WorkerType::Denix("1.0.0".to_string()),
    ///     "127.0.0.1".to_string(),
    ///     3000,
    /// );
    /// ```
    pub fn new_single_port(worker_type: WorkerType, listen_host: String, port: u16) -> Self {
        Self {
            worker_type,
            listen_host,
            listen_port: vec![port],
        }
    }

    /// Get the version of the worker
    pub fn version(&self) -> &str {
        self.worker_type.version()
    }

    /// Get the primary port (first port in the list)
    ///
    /// # Returns
    ///
    /// Returns the first port in the port list, or 0 if no ports are configured.
    pub fn primary_port(&self) -> u16 {
        self.listen_port.first().copied().unwrap_or(0)
    }

    /// Get the complete listen address including the primary port
    ///
    /// # Returns
    ///
    /// Returns a string in the format "host:port" using the primary port.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use workit::{WorkerInfo, WorkerType};
    ///
    /// let worker_info = WorkerInfo::new_single_port(
    ///     WorkerType::Deflux("1.2.0".to_string()),
    ///     "127.0.0.1".to_string(),
    ///     3000,
    /// );
    /// assert_eq!(worker_info.listen_address(), "127.0.0.1:3000");
    /// ```
    pub fn listen_address(&self) -> String {
        format!("{}:{}", self.listen_host, self.primary_port())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denix_worker() -> WorkerType {
        WorkerType::Denix("1.0.0".to_string())
    }

    #[test]
    fn test_worker_info_new() {
        let worker_info = WorkerInfo::new(denix_worker(), "0.0.0.0".to_string(), vec![80, 443]);

        assert_eq!(worker_info.version(), "1.0.0");
        assert_eq!(worker_info.listen_host, "0.0.0.0");
        assert_eq!(worker_info.listen_port, vec![80, 443]);
    }

    #[test]
    fn test_worker_info_new_single_port() {
        let worker_info =
            WorkerInfo::new_single_port(denix_worker(), "127.0.0.1".to_string(), 3000);

        assert_eq!(worker_info.version(), "1.0.0");
        assert_eq!(worker_info.listen_host, "127.0.0.1");
        assert_eq!(worker_info.listen_port, vec![3000]);
    }

    #[test]
    fn test_primary_port() {
        let worker_info = WorkerInfo::new(denix_worker(), "0.0.0.0".to_string(), vec![80, 443]);

        assert_eq!(worker_info.primary_port(), 80);

        let empty_ports = WorkerInfo::new(denix_worker(), "0.0.0.0".to_string(), vec![]);

        assert_eq!(empty_ports.primary_port(), 0);
    }

    #[test]
    fn test_listen_address() {
        let worker_info =
            WorkerInfo::new_single_port(denix_worker(), "127.0.0.1".to_string(), 3000);

        assert_eq!(worker_info.listen_address(), "127.0.0.1:3000");
    }

    #[test]
    fn test_version_retrieval() {
        let denix_info = WorkerInfo::new(
            WorkerType::Denix("0.1.0".to_string()),
            "localhost".to_string(),
            vec![80],
        );
        let deflux_info = WorkerInfo::new(
            WorkerType::Deflux("1.2.3".to_string()),
            "localhost".to_string(),
            vec![8080],
        );

        assert_eq!(denix_info.version(), "0.1.0");
        assert!(matches!(denix_info.worker_type, WorkerType::Denix(_)));

        assert_eq!(deflux_info.version(), "1.2.3");
        assert!(matches!(deflux_info.worker_type, WorkerType::Deflux(_)));
    }
}
