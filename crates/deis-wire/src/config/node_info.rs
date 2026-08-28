use serde::{Deserialize, Serialize};

/// Node Type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRole {
    #[serde(rename = "runtime")]
    Runtime(String),
    #[serde(rename = "proxy")]
    Proxy(String),
}

impl NodeRole {
    /// Get the version string from the node type
    pub fn version(&self) -> &str {
        match self {
            NodeRole::Runtime(v) => v,
            NodeRole::Proxy(v) => v,
        }
    }
}

/// Node instance information
///
/// Contains metadata about the node instance including version,
/// listening address, and port configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Node Type
    pub node_role: NodeRole,
    /// Listen host, e.g., "0.0.0.0" or "127.0.0.1"
    pub listen_host: String,
    /// Port numbers that the node listens on, e.g., [80, 443]
    pub listen_port: Vec<u16>,
}

impl NodeInfo {
    /// Create new node information with multiple ports
    ///
    /// # Arguments
    ///
    /// * `node_role` - Type of the node, including its version
    /// * `listen_host` - Host address to listen on
    /// * `listen_port` - Vector of ports to listen on
    ///
    /// # Examples
    ///
    /// ```rust
    /// use deis_wire::{NodeInfo, NodeRole};
    ///
    /// let node_info = NodeInfo::new(
    ///     NodeRole::Runtime("1.0.0".to_string()),
    ///     "0.0.0.0".to_string(),
    ///     vec![80, 443],
    /// );
    /// ```
    pub fn new(node_role: NodeRole, listen_host: String, listen_port: Vec<u16>) -> Self {
        Self {
            node_role,
            listen_host,
            listen_port,
        }
    }

    /// Create new node information with a single port (convenience method)
    ///
    /// # Arguments
    ///
    /// * `node_role` - Type of the node, including its version
    /// * `listen_host` - Host address to listen on
    /// * `port` - Single port to listen on
    ///
    /// # Examples
    ///
    /// ```rust
    /// use deis_wire::{NodeInfo, NodeRole};
    ///
    /// let node_info = NodeInfo::new_single_port(
    ///     NodeRole::Runtime("1.0.0".to_string()),
    ///     "127.0.0.1".to_string(),
    ///     3000,
    /// );
    /// ```
    pub fn new_single_port(node_role: NodeRole, listen_host: String, port: u16) -> Self {
        Self {
            node_role,
            listen_host,
            listen_port: vec![port],
        }
    }

    /// Get the version of the node
    pub fn version(&self) -> &str {
        self.node_role.version()
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
    /// use deis_wire::{NodeInfo, NodeRole};
    ///
    /// let node_info = NodeInfo::new_single_port(
    ///     NodeRole::Proxy("1.2.0".to_string()),
    ///     "127.0.0.1".to_string(),
    ///     3000,
    /// );
    /// assert_eq!(node_info.listen_address(), "127.0.0.1:3000");
    /// ```
    pub fn listen_address(&self) -> String {
        format!("{}:{}", self.listen_host, self.primary_port())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deis_runtime_node() -> NodeRole {
        NodeRole::Runtime("1.0.0".to_string())
    }

    #[test]
    fn test_node_info_new() {
        let node_info = NodeInfo::new(deis_runtime_node(), "0.0.0.0".to_string(), vec![80, 443]);

        assert_eq!(node_info.version(), "1.0.0");
        assert_eq!(node_info.listen_host, "0.0.0.0");
        assert_eq!(node_info.listen_port, vec![80, 443]);
    }

    #[test]
    fn test_node_info_new_single_port() {
        let node_info =
            NodeInfo::new_single_port(deis_runtime_node(), "127.0.0.1".to_string(), 3000);

        assert_eq!(node_info.version(), "1.0.0");
        assert_eq!(node_info.listen_host, "127.0.0.1");
        assert_eq!(node_info.listen_port, vec![3000]);
    }

    #[test]
    fn test_primary_port() {
        let node_info = NodeInfo::new(deis_runtime_node(), "0.0.0.0".to_string(), vec![80, 443]);

        assert_eq!(node_info.primary_port(), 80);

        let empty_ports = NodeInfo::new(deis_runtime_node(), "0.0.0.0".to_string(), vec![]);

        assert_eq!(empty_ports.primary_port(), 0);
    }

    #[test]
    fn test_listen_address() {
        let node_info =
            NodeInfo::new_single_port(deis_runtime_node(), "127.0.0.1".to_string(), 3000);

        assert_eq!(node_info.listen_address(), "127.0.0.1:3000");
    }

    #[test]
    fn test_version_retrieval() {
        let deis_runtime_info = NodeInfo::new(
            NodeRole::Runtime("0.1.0".to_string()),
            "localhost".to_string(),
            vec![80],
        );
        let deis_proxy_info = NodeInfo::new(
            NodeRole::Proxy("1.2.3".to_string()),
            "localhost".to_string(),
            vec![8080],
        );

        assert_eq!(deis_runtime_info.version(), "0.1.0");
        assert!(matches!(deis_runtime_info.node_role, NodeRole::Runtime(_)));

        assert_eq!(deis_proxy_info.version(), "1.2.3");
        assert!(matches!(deis_proxy_info.node_role, NodeRole::Proxy(_)));
    }
}
