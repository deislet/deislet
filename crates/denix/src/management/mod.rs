// 管理服务模块 - gRPC 接口供 supervisor 调用

mod server;
mod service;

pub use server::ManagementServer;
pub use service::DenixManagementService;

// 生成的 protobuf 代码
pub mod proto {
    tonic::include_proto!("denix.management");
}
