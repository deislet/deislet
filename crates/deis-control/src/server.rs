use std::sync::Arc;
use std::net::SocketAddr;
use tonic::transport::Server;
use tracing::info;

use crate::config::ControlConfig;
use crate::session::SessionManager;
use crate::database::Database;
use crate::broadcast::EventBroadcaster;
use crate::command::CommandExecutor;
use crate::api::grpc::GrpcControlService;
use deis_wire::pb::control_service_server::ControlServiceServer;

pub struct ControlServer {
    config: Arc<ControlConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Arc<EventBroadcaster>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl ControlServer {
    pub fn new(config: ControlConfig) -> Self {
        Self {
            config: Arc::new(config),
            sessions: Arc::new(SessionManager::new()),
            database: None,
            broadcaster: Arc::new(EventBroadcaster::new()),
            command_executor: None,
        }
    }

    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    pub fn with_command_executor(mut self, command_executor: Arc<CommandExecutor>) -> Self {
        self.command_executor = Some(command_executor);
        self
    }

    pub fn sessions(&self) -> Arc<SessionManager> {
        Arc::clone(&self.sessions)
    }

    pub fn broadcaster(&self) -> Arc<EventBroadcaster> {
        Arc::clone(&self.broadcaster)
    }

    pub fn database(&self) -> Option<Arc<Database>> {
        self.database.as_ref().map(Arc::clone)
    }

    pub fn build_grpc_service(&self) -> ControlServiceServer<GrpcControlService> {
        let sessions = Arc::clone(&self.sessions);
        let config = Arc::clone(&self.config);
        let broadcaster = Arc::clone(&self.broadcaster);

        let mut grpc_service = GrpcControlService::new(
            config,
            sessions,
            broadcaster,
        );

        if let Some(db) = &self.database {
            grpc_service = grpc_service.with_database(db.clone());
        }

        if let Some(executor) = &self.command_executor {
            grpc_service = grpc_service.with_command_executor(executor.clone());
        }

        ControlServiceServer::new(grpc_service)
    }

    pub async fn start(&self) -> Result<(), anyhow::Error> {
        let addr_str = format!("{}:{}", self.config.bind_addr, self.config.port);
        let addr: SocketAddr = addr_str.parse()?;

        info!("Control gRPC listening on {}", addr);

        let grpc_service = self.build_grpc_service();

        Server::builder()
            .add_service(grpc_service)
            .serve(addr)
            .await?;

        Ok(())
    }
}
