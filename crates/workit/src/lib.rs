//! Workit - Communication library for Denira supervisor and Denix worker servers
//!
//! This SDK provides a unified interface for communication between Denira supervisor
//! and Denix worker servers, including authentication, registration, and heartbeat functionality.
//!
//! # Examples
//!
//! ```rust,no_run
//! use workit::{Workit, SupervisorConfig, WorkerInfo, WorkerType};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = SupervisorConfig::new(
//!         "worker-001".to_string(),
//!         "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
//!         "http://localhost:8080".to_string(),
//!     );
//!     
//!     let worker_info = WorkerInfo::new_single_port(
//!         WorkerType::Denix("1.0.0".to_string()),
//!         "127.0.0.1".to_string(),
//!         3000,
//!     );
//!     
//!     let client = Workit::new(config, worker_info)?;
//!     let register_response = client.register().await?;
//!     println!("Registration: {}", register_response.message);
//!     
//!     Ok(())
//! }
//! ```

mod config;
mod error;
mod models;

// Public re-exports
pub use crate::{
    config::{SupervisorConfig, WorkerInfo, WorkerType},
    error::{SdkResult, SupervisorErrorResponse, WorkitError},
    models::{
        HeartbeatRequest, HeartbeatResponse, InstanceDetails, InstanceStatus, RegisterRequest,
        RegisterResponse,
        instance::instance_fsm::{self, InstanceFSM},
    },
};

use reqwest::Client as HttpClient;
use std::sync::Arc;
use tokio::{sync::RwLock, task::JoinHandle, time::Duration};
use tracing::{debug, info};
use yasm::StateMachineInstance;

const DEFAULT_RETRY_COUNT: u8 = 2;

/// Main Workit client for communication with Denira supervisor
///
/// This struct provides methods for registering with the supervisor,
/// sending heartbeats, and managing the worker instance state.
#[derive(Clone)]
pub struct Workit {
    /// HTTP client for network requests
    http_client: HttpClient,
    /// Supervisor configuration
    superv_config: SupervisorConfig,
    /// Worker information
    worker_info: WorkerInfo,
    /// Finite State Machine for instance status
    fsm: Arc<RwLock<StateMachineInstance<InstanceFSM>>>,
}

impl Workit {
    /// Create a new Workit client
    ///
    /// # Arguments
    ///
    /// * `config` - Supervisor connection configuration
    /// * `worker_info` - Worker instance information
    ///
    /// # Returns
    ///
    /// Returns `Ok(Workit)` on success, or `Err(WorkitError)` if configuration is invalid
    /// or HTTP client creation fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use workit::{Workit, SupervisorConfig, WorkerInfo, WorkerType};
    ///
    /// let config = SupervisorConfig::new(
    ///     "worker-001".to_string(),
    ///     "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
    ///     "http://localhost:8080".to_string(),
    /// );
    ///
    /// let worker_info = WorkerInfo::new_single_port(
    ///     WorkerType::Denix("1.0.0".to_string()),
    ///     "127.0.0.1".to_string(),
    ///     3000,
    /// );
    /// let client = Workit::new(config, worker_info)?;
    /// # Ok::<(), workit::WorkitError>(())
    /// ```
    pub fn new(config: SupervisorConfig, worker_info: WorkerInfo) -> SdkResult<Self> {
        // Validate configuration
        config.validate().map_err(WorkitError::ConfigurationError)?;

        // Create HTTP client with timeout
        let http_client = HttpClient::builder()
            .timeout(config.request_timeout_duration())
            .build()
            .map_err(|e| WorkitError::NetworkError(format!("Failed to create HTTP client: {e}")))?;

        Ok(Self {
            http_client,
            superv_config: config,
            worker_info,
            fsm: Arc::new(RwLock::new(StateMachineInstance::<InstanceFSM>::new())),
        })
    }

    /// Get current instance status
    ///
    /// # Returns
    ///
    /// Returns the current `InstanceStatus` of the worker.
    pub async fn get_status(&self) -> InstanceStatus {
        self.fsm.read().await.current_state().clone()
    }

    /// Register worker instance with the supervisor
    ///
    /// This method sends a registration request to the supervisor with the worker's
    /// information and authentication credentials.
    ///
    /// # Returns
    ///
    /// Returns `Ok(RegisterResponse)` on successful registration, or `Err(WorkitError)`
    /// if registration fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use workit::{Workit, SupervisorConfig, WorkerInfo, WorkerType};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), workit::WorkitError> {
    /// # let config = SupervisorConfig::new("worker-001".to_string(), "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(), "http://localhost:8080".to_string());
    /// # let worker_info = WorkerInfo::new_single_port(WorkerType::Denix("1.0.0".to_string()), "127.0.0.1".to_string(), 3000);
    /// # let client = Workit::new(config, worker_info)?;
    /// let response = client.register().await?;
    /// println!("Registration successful: {}", response.message);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn register(&self) -> SdkResult<RegisterResponse> {
        info!("Registering to Supervisor: {}", self.superv_config.addr);

        // Create register request with authentication
        let request = RegisterRequest::new(
            self.superv_config.associated_id.clone(),
            self.worker_info.clone(),
            self.superv_config.secret.as_bytes(),
        );

        // Send request with retry
        let response = self
            .send_request_with_retry(&self.superv_config.register_url(), &request, None)
            .await?;

        self.handle_register_response(response).await
    }

    /// Send heartbeat to supervisor
    ///
    /// This method sends a heartbeat request to the supervisor to indicate the worker
    /// is still alive and operational.
    ///
    /// # Returns
    ///
    /// Returns `Ok(HeartbeatResponse)` on successful heartbeat, or `Err(WorkitError)`
    /// if heartbeat fails.
    pub async fn heartbeat(&self) -> SdkResult<HeartbeatResponse> {
        debug!("Sending heartbeat to Supervisor");

        // Create heartbeat request
        let request = HeartbeatRequest::new(self.superv_config.associated_id.clone()).await;

        // Send request with retry
        let response = self
            .send_request_with_retry(&self.superv_config.heartbeat_url(), &request, None)
            .await?;

        self.handle_heartbeat_response(response).await
    }

    /// Start automatic heartbeat task
    ///
    /// This method starts a background task that sends heartbeats to the supervisor
    /// at regular intervals as configured in `SupervisorConfig`.
    ///
    /// # Returns
    ///
    /// Returns a `JoinHandle` that can be used to control or stop the heartbeat task.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use workit::{Workit, SupervisorConfig, WorkerInfo, WorkerType};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), workit::WorkitError> {
    /// # let config = SupervisorConfig::new("worker-001".to_string(), "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(), "http://localhost:8080".to_string());
    /// # let worker_info = WorkerInfo::new_single_port(WorkerType::Denix("1.0.0".to_string()), "127.0.0.1".to_string(), 3000);
    /// # let client = Workit::new(config, worker_info)?;
    /// let handle = client.start_heartbeat_task().await;
    ///
    /// // Later, stop the heartbeat task
    /// handle.abort();
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_heartbeat_task(&self) -> JoinHandle<()> {
        let client = self.clone();
        let interval = self.superv_config.heartbeat_duration();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;

                match client.heartbeat().await {
                    Ok(response) => {
                        debug!("Heartbeat successful: {}", response.message);
                    }
                    Err(e) => {
                        tracing::warn!("Heartbeat failed: {}", e);
                    }
                }
            }
        })
    }

    /// Get detailed instance information
    ///
    /// # Returns
    ///
    /// Returns `InstanceDetails` containing current instance information.
    pub async fn get_instance_details(&self) -> InstanceDetails {
        InstanceDetails {
            id: self.superv_config.associated_id.clone(),
            info: self.worker_info.clone(),
            status: self.get_status().await,
            last_heartbeat: Some(chrono::Utc::now()),
        }
    }

    // ------------------------------------------------------------------------------------------------
    // Private helper methods
    // ------------------------------------------------------------------------------------------------

    /// Send HTTP request with retry logic
    async fn send_request_with_retry<T: serde::Serialize>(
        &self,
        url: &str,
        payload: &T,
        retry_count: Option<u8>,
    ) -> SdkResult<reqwest::Response> {
        let mut last_error = None;
        let retry_count = retry_count.unwrap_or(DEFAULT_RETRY_COUNT);

        for attempt in 0..=retry_count {
            if attempt > 0 {
                // Exponential backoff: 2^attempt seconds, max 5
                let delay = Duration::from_secs(2_u64.pow(attempt.min(5).into()));
                debug!(
                    "Retrying attempt {}/{} with delay {:?}",
                    attempt, retry_count, delay
                );
                tokio::time::sleep(delay).await;
            }

            match self.http_client.post(url).json(payload).send().await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!(
                        "Request failed (attempt {}/{}): {}",
                        attempt + 1,
                        retry_count + 1,
                        e
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(WorkitError::NetworkError(last_error.unwrap().to_string()))
    }

    /// Handle registration response
    async fn handle_register_response(
        &self,
        response: reqwest::Response,
    ) -> SdkResult<RegisterResponse> {
        let status = response.status();

        if status.is_success() {
            let register_response: RegisterResponse = response.json().await?;

            if register_response.success {
                info!("Registration successful: {}", register_response.message);

                // Update FSM state to Active
                if let Err(e) = self
                    .fsm
                    .write()
                    .await
                    .transition(instance_fsm::Input::Registed)
                {
                    tracing::error!("Failed to transition to Active: {}", e);
                }

                Ok(register_response)
            } else {
                tracing::error!("Registration failed: {}", register_response.message);
                Err(WorkitError::SupervisorError {
                    code: "REGISTRATION_FAILED".to_string(),
                    message: register_response.message,
                    help: None,
                })
            }
        } else {
            let error_response: SupervisorErrorResponse =
                response
                    .json()
                    .await
                    .unwrap_or_else(|_| SupervisorErrorResponse {
                        success: false,
                        error: "UNKNOWN_ERROR".to_string(),
                        message: format!("HTTP {status}"),
                        help: None,
                    });

            tracing::error!(
                "Registration failed [{}]: {}",
                error_response.error,
                error_response.message
            );

            Err(WorkitError::SupervisorError {
                code: error_response.error,
                message: error_response.message,
                help: error_response.help,
            })
        }
    }

    /// Handle heartbeat response
    async fn handle_heartbeat_response(
        &self,
        response: reqwest::Response,
    ) -> SdkResult<HeartbeatResponse> {
        let status = response.status();

        if status.is_success() {
            let heartbeat_response: HeartbeatResponse = response.json().await?;

            if heartbeat_response.success {
                debug!(
                    "Heartbeat sent successfully: {}",
                    heartbeat_response.message
                );
                Ok(heartbeat_response)
            } else {
                tracing::warn!("Heartbeat response failed: {}", heartbeat_response.message);
                Err(WorkitError::SupervisorError {
                    code: "HEARTBEAT_FAILED".to_string(),
                    message: heartbeat_response.message,
                    help: None,
                })
            }
        } else {
            let error_response: SupervisorErrorResponse =
                response
                    .json()
                    .await
                    .unwrap_or_else(|_| SupervisorErrorResponse {
                        success: false,
                        error: "UNKNOWN_ERROR".to_string(),
                        message: format!("HTTP {status}"),
                        help: None,
                    });

            tracing::warn!(
                "Heartbeat failed [{}]: {}",
                error_response.error,
                error_response.message
            );

            Err(WorkitError::SupervisorError {
                code: error_response.error,
                message: error_response.message,
                help: error_response.help,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

    fn create_test_client() -> (Workit, SupervisorConfig) {
        let config = SupervisorConfig::new(
            "worker-001".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );
        let worker_info = WorkerInfo::new_single_port(
            WorkerType::Denix("0.1.0".to_string()),
            "127.0.0.1".to_string(),
            3000,
        );
        let client = Workit::new(config.clone(), worker_info).unwrap();
        (client, config)
    }

    #[test]
    fn test_workit_creation_with_valid_config() {
        let (client, config) = create_test_client();
        assert_eq!(client.superv_config, config);
        assert_eq!(client.worker_info.version(), "0.1.0");
    }

    #[test]
    fn test_workit_creation_with_invalid_config() {
        // Invalid secret
        let config = SupervisorConfig::new(
            "w1".to_string(),
            "invalid-secret".to_string(),
            "http://localhost:8080".to_string(),
        );
        let worker_info = WorkerInfo::new_single_port(
            WorkerType::Denix("1.0.0".to_string()),
            "127.0.0.1".to_string(),
            3000,
        );
        let result = Workit::new(config, worker_info);
        match result {
            Err(WorkitError::ConfigurationError(msg)) => {
                assert!(msg.contains("must be exactly 64 characters long"));
            }
            _ => panic!("Expected ConfigurationError with specific message"),
        }

        // Invalid address
        let config = SupervisorConfig::new(
            "w1".to_string(),
            VALID_SECRET.to_string(),
            "invalid-address".to_string(),
        );
        let worker_info = WorkerInfo::new_single_port(
            WorkerType::Denix("1.0.0".to_string()),
            "127.0.0.1".to_string(),
            3000,
        );
        let result = Workit::new(config, worker_info);
        match result {
            Err(WorkitError::ConfigurationError(msg)) => {
                assert!(msg.contains("must start with http:// or https://"));
            }
            _ => panic!("Expected ConfigurationError with specific message"),
        }
    }

    #[tokio::test]
    async fn test_initial_status_is_pending() {
        let (client, _) = create_test_client();
        assert_eq!(client.get_status().await, InstanceStatus::Pending);
    }

    #[tokio::test]
    async fn test_register_fails_without_server() {
        let (client, _) = create_test_client();

        // This should return a network error because the server is not running
        let result = client.register().await;
        assert!(result.is_err());
        if let Err(WorkitError::NetworkError(msg)) = result {
            // The exact message depends on the OS and reqwest version,
            // but it should indicate a connection failure.
            let lower_msg = msg.to_lowercase();
            assert!(
                lower_msg.contains("connection refused")
                    || lower_msg.contains("error sending request")
            );
        } else {
            panic!("Expected a NetworkError, but got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_instance_details() {
        let (client, config) = create_test_client();
        let details = client.get_instance_details().await;

        assert_eq!(details.id, config.associated_id);
        assert_eq!(details.info.listen_address(), "127.0.0.1:3000");
        assert_eq!(details.info.version(), "0.1.0");
        assert_eq!(details.status, InstanceStatus::Pending);
        assert!(details.last_heartbeat.is_some());
        assert!(matches!(details.info.worker_type, WorkerType::Denix(_)));
    }
}
