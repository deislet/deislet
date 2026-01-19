// build.rs
// 编译 Protocol Buffers 定义

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 编译 storage proto（客户端）
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/storage.proto"], &["proto"])?;

    // 编译 logging proto（客户端 - denix 推送日志）
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/logging.proto"], &["proto"])?;

    // 编译 management proto（服务端 - 接收 supervisor 控制）
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/management.proto"], &["proto"])?;

    // 当 proto 文件变化时重新编译
    println!("cargo:rerun-if-changed=proto/storage.proto");
    println!("cargo:rerun-if-changed=proto/logging.proto");
    println!("cargo:rerun-if-changed=proto/management.proto");

    Ok(())
}
