/// 内存模式示例 - 演示如何在不写文件的情况下获取生成的代码
///
/// 使用方法:
/// ```bash
/// cargo run --example memory_mode_example
/// ```
use deforge::{BuildArtifacts, Compiler, CompilerOptions, OutputMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Deforge 内存模式示例 ===\n");

    // 示例 1: 仅内存模式
    println!("【示例 1】仅内存模式 - 不写文件");
    println!("{}", "-".repeat(50));

    let options = CompilerOptions {
        project_dir: "./examples/hello-world".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Memory, // 仅返回内存内容
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    match compiler.build().await {
        Ok(result) => {
            if let Some(build) = result.builds.get("cloudflare") {
                println!("✓ 编译成功（内存模式）");
                println!("  文件输出路径: {}", build.output_path);
                println!("  （注意：实际未写入文件）");

                if let Some(artifacts) = &build.artifacts {
                    display_artifacts(artifacts);
                } else {
                    println!("  ✗ 未获取到产物内容");
                }
            }
        }
        Err(e) => {
            println!("✗ 编译失败: {}", e);
            println!("  （可能是示例项目不存在，这是正常的）");
        }
    }

    println!("\n");

    // 示例 2: 同时写文件和返回内存
    println!("【示例 2】Both 模式 - 既写文件又返回内存");
    println!("{}", "-".repeat(50));

    let options = CompilerOptions {
        project_dir: "./examples/hello-world".to_string(),
        target_vendor: Some("deno".to_string()),
        output_mode: OutputMode::Both,
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    match compiler.build().await {
        Ok(result) => {
            if let Some(build) = result.builds.get("deno") {
                println!("✓ 编译成功（Both 模式）");
                println!("  文件已写入: {}", build.output_path);

                if let Some(artifacts) = &build.artifacts {
                    display_artifacts(artifacts);

                    // 可以进一步处理内存中的代码
                    println!("\n  💡 可以对内存中的代码进行:");
                    println!("     - 上传到 CDN");
                    println!("     - 发送到构建服务");
                    println!("     - 进行后处理分析");
                    println!("     - ...");
                }
            }
        }
        Err(e) => {
            println!("✗ 编译失败: {}", e);
            println!("  （可能是示例项目不存在，这是正常的）");
        }
    }

    Ok(())
}

/// 显示 BuildArtifacts 的详细信息
fn display_artifacts(artifacts: &BuildArtifacts) {
    println!("\n  📦 构建产物:");
    println!("     入口文件: {}", artifacts.entry_filename);
    println!("     入口代码: {} 字节", artifacts.entry_code.len());

    if !artifacts.config_files.is_empty() {
        println!("\n  📄 配置文件 ({} 个):", artifacts.config_files.len());
        for (name, content) in &artifacts.config_files {
            println!("     - {}: {} 字节", name, content.len());
        }
    }

    if !artifacts.source_files.is_empty() {
        println!("\n  📝 源文件 ({} 个):", artifacts.source_files.len());
        for (path, content) in artifacts.source_files.iter().take(5) {
            println!("     - {}: {} 字节", path, content.len());
        }
        if artifacts.source_files.len() > 5 {
            println!("     ... 还有 {} 个文件", artifacts.source_files.len() - 5);
        }
    }

    if !artifacts.system_files.is_empty() {
        println!("\n  🔧 系统文件 ({} 个):", artifacts.system_files.len());
        for (path, content) in &artifacts.system_files {
            println!("     - {}: {} 字节", path, content.len());
        }
    }
}
