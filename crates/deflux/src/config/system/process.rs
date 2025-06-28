use std::fs::File;
use std::os::unix::process::parent_id;
use anyhow::{Context, Result};
use daemonize_me::{Daemon, Group, User};
use tracing::info;

/// 检查当前进程是否以守护进程模式运行
pub fn is_daemon() -> bool {
    // 检查进程的父进程是否为 init (PID 1)
    parent_id() == 1
}

/// 将当前进程转换为守护进程
///
/// # Arguments
///
/// * `username` - 可选的运行用户名. 如果提供, `groupname` 也必须提供.
/// * `groupname` - 可选的运行用户组. 如果提供, `username` 也必须提供.
///
/// # Errors
///
/// 如果守护进程化失败，将返回错误.
pub fn daemonize(username: Option<&str>, groupname: Option<&str>) -> Result<()> {
    if is_daemon() {
        info!("Already running in daemon mode, skipping daemonization.");
        return Ok(());
    }

    let stdout = File::create("/tmp/deflux.out")
        .context("Failed to create stdout log file")?;
    let stderr = File::create("/tmp/deflux.err")
        .context("Failed to create stderr log file")?;

    let mut daemon = Daemon::new()
        .pid_file("/tmp/deflux.pid", Some(true))
        .umask(0o027)
        .work_dir("/")
        .stdout(stdout)
        .stderr(stderr);

    // 根据 daemonize-me 的文档, user 和 group 必须同时提供
    if let (Some(user), Some(group)) = (username, groupname) {
        daemon = daemon
            .user(User::try_from(user).context("Invalid user specified")?)
            .group(Group::try_from(group).context("Invalid group specified")?);
    } else if username.is_some() || groupname.is_some() {
        return Err(anyhow::anyhow!(
            "Both user and group must be specified for privilege drop, or neither."
        ));
    }

    daemon
        .start()
        .map_err(|e| anyhow::anyhow!("Failed to start daemon: {}", e))?;

    info!("Process successfully daemonized.");
    Ok(())
}