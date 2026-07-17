use std::sync::Arc;
use std::net::SocketAddr;
use tonic::transport::Server;
use tracing::info;

use crate::config::SupervisorConfig;
use crate::session::SessionManager;
use crate::database::Database;
use crate::broadcast::EventBroadcaster;
use crate::command::CommandExecutor;
use crate::api::grpc::GrpcSupervisorService;
use denira_workit::pb::supervisor_service_server::SupervisorServiceServer;

pub struct SupervisorServer {
    config: Arc<SupervisorConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Arc<EventBroadcaster>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl SupervisorServer {
    pub fn new(config: SupervisorConfig) -> Self {
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

    pub fn build_grpc_service(&self) -> SupervisorServiceServer<GrpcSupervisorService> {
        let sessions = Arc::clone(&self.sessions);
        let config = Arc::clone(&self.config);
        let broadcaster = Arc::clone(&self.broadcaster);

        let mut grpc_service = GrpcSupervisorService::new(
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

        SupervisorServiceServer::new(grpc_service)
    }

    pub async fn start(&self) -> Result<(), anyhow::Error> {
        let addr_str = format!("{}:{}", self.config.bind_addr, self.config.port);
        let addr: SocketAddr = addr_str.parse()?;

        info!("Supervisor gRPC listening on {}", addr);

        let grpc_service = self.build_grpc_service();

        Server::builder()
            .add_service(grpc_service)
            .serve(addr)
            .await?;

        Ok(())
    }
}
