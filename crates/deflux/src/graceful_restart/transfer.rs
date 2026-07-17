//! File descriptor transfer
//!
//! Uses Unix domain sockets and SCM_RIGHTS control messages to transfer file descriptors between processes.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Maximum number of file descriptors for FD transfer
const MAX_FDS: usize = 32;

/// Data structure for transfer
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FdPayload {
    /// Mapping from address to FD index (index corresponds to FD array)
    pub addr_to_fd_index: HashMap<SocketAddr, usize>,
}

/// Send file descriptors to a specified Unix socket
///
/// # Parameters
/// - `fds`: List of file descriptors to send
/// - `addrs`: List of addresses corresponding to FDs
/// - `socket_path`: Unix socket path
/// - `max_retry`: Maximum number of retries
/// - `retry_interval`: Retry interval
pub fn send_fds<P: AsRef<Path>>(
    fds: Vec<RawFd>,
    addrs: Vec<SocketAddr>,
    socket_path: P,
    max_retry: usize,
    retry_interval: Duration,
) -> Result<()> {
    if fds.is_empty() {
        return Err(anyhow!("No file descriptors to send"));
    }

    if fds.len() > MAX_FDS {
        return Err(anyhow!(
            "Too many file descriptors: {} (max: {})",
            fds.len(),
            MAX_FDS
        ));
    }

    if fds.len() != addrs.len() {
        return Err(anyhow!(
            "FD count ({}) does not match address count ({})",
            fds.len(),
            addrs.len()
        ));
    }

    // Build payload
    let addr_to_fd_index: HashMap<SocketAddr, usize> =
        addrs.iter().enumerate().map(|(i, addr)| (*addr, i)).collect();

    let payload = FdPayload { addr_to_fd_index };
    let payload_bytes =
        serde_json::to_vec(&payload).context("Failed to serialize FD payload")?;

    info!(
        "Sending {} FDs to socket {}",
        fds.len(),
        socket_path.as_ref().display()
    );
    debug!("FD payload: {} bytes", payload_bytes.len());

    // Connect to Unix socket (with retries)
    let mut stream = None;
    for attempt in 0..max_retry {
        match UnixStream::connect(&socket_path) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt < max_retry - 1 {
                    warn!(
                        "Failed to connect to socket (attempt {}/{}): {}",
                        attempt + 1,
                        max_retry,
                        e
                    );
                    std::thread::sleep(retry_interval);
                } else {
                    return Err(anyhow!("Failed to connect after {} attempts: {}", max_retry, e));
                }
            }
        }
    }

    let stream = stream.ok_or_else(|| anyhow!("Failed to connect to socket"))?;

    // Send FD
    send_fds_impl(stream, &fds, &payload_bytes)
        .context("Failed to send file descriptors")
}

/// Receive file descriptors from Unix socket
///
/// # Parameters
/// - `socket_path`: Unix socket path
///
/// # Returns
/// - Returns (address -> FD mapping)
pub fn receive_fds<P: AsRef<Path>>(
    socket_path: P,
) -> Result<HashMap<SocketAddr, RawFd>> {
    let socket_path = socket_path.as_ref();

    // Remove old socket file
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("Failed to remove old socket: {}", socket_path.display()))?;
    }

    // Create Unix socket
    let listener = UnixListener::bind(socket_path).with_context(|| {
        format!(
            "Failed to bind Unix socket: {}",
            socket_path.display()
        )
    })?;

    info!("Waiting for FDs on socket {}", socket_path.display());

    // Accept connection
    let (stream, _) = listener
        .accept()
        .context("Failed to accept connection on Unix socket")?;

    // Receive FD
    let (fds, payload_bytes) =
        receive_fds_impl(stream).context("Failed to receive file descriptors")?;

    // Parse payload
    let payload: FdPayload = serde_json::from_slice(&payload_bytes)
        .context("Failed to deserialize FD payload")?;

    info!("Received {} FDs", fds.len());

    // Build address-to-FD mapping
    let mut addr_to_fd = HashMap::new();
    for (addr, fd_index) in payload.addr_to_fd_index {
        if fd_index >= fds.len() {
            error!(
                "Invalid FD index {} for address {} (max: {})",
                fd_index,
                addr,
                fds.len() - 1
            );
            continue;
        }
        addr_to_fd.insert(addr, fds[fd_index]);
    }

    // Clean up socket file
    let _ = fs::remove_file(socket_path);

    Ok(addr_to_fd)
}

/// Actual FD send implementation (using sendmsg)
#[cfg(unix)]
fn send_fds_impl(stream: UnixStream, fds: &[RawFd], payload: &[u8]) -> Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::os::unix::io::AsRawFd;

    let iov = [IoSlice::new(payload)];
    let cmsg = [ControlMessage::ScmRights(fds)];

    let fd = stream.as_raw_fd();

    sendmsg::<()>(fd, &iov, &cmsg, MsgFlags::empty(), None)
        .context("sendmsg failed")?;

    debug!("Sent {} FDs via sendmsg", fds.len());

    Ok(())
}

/// Actual FD receive implementation (using recvmsg)
#[cfg(unix)]
fn receive_fds_impl(stream: UnixStream) -> Result<(Vec<RawFd>, Vec<u8>)> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    use std::os::unix::io::AsRawFd;

    // Prepare receive buffer
    let mut payload_buf = vec![0u8; 65536]; // 64KB should be sufficient
    let mut iov = [IoSliceMut::new(&mut payload_buf)];

    // Prepare control message buffer (for receiving FDs)
    let mut cmsg_buf = nix::cmsg_space!([RawFd; MAX_FDS]);

    let fd = stream.as_raw_fd();

    // Receive message
    let msg = recvmsg::<()>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty())
        .context("recvmsg failed")?;

    let bytes_received = msg.bytes;
    debug!("Received {} bytes via recvmsg", bytes_received);

    // Extract file descriptors
    let mut fds = Vec::new();

    // Note: nix 0.30 cmsgs() returns Result<CmsgIterator, Errno>
    let cmsgs = msg.cmsgs().context("Failed to get control messages")?;
    for cmsg in cmsgs {
        if let ControlMessageOwned::ScmRights(mut received_fds) = cmsg {
            fds.append(&mut received_fds);
        }
    }

    if fds.is_empty() {
        return Err(anyhow!("No file descriptors received"));
    }

    debug!("Extracted {} FDs from control message", fds.len());

    // Truncate actually received payload data
    let payload = payload_buf[..bytes_received].to_vec();

    Ok((fds, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn test_fd_payload_serialization() {
        let mut addr_to_fd_index = HashMap::new();
        addr_to_fd_index.insert("127.0.0.1:8080".parse().unwrap(), 0);
        addr_to_fd_index.insert("127.0.0.1:8081".parse().unwrap(), 1);

        let payload = FdPayload { addr_to_fd_index };

        let serialized = serde_json::to_vec(&payload).unwrap();
        let deserialized: FdPayload = serde_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.addr_to_fd_index.len(), deserialized.addr_to_fd_index.len());
    }

    #[test]
    fn test_send_receive_fds() {
        let socket_path = "/tmp/deflux-test-fd-transfer.sock";

        // Clean up old socket
        let _ = std::fs::remove_file(socket_path);

        // Create test listeners
        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();

        let addr1 = listener1.local_addr().unwrap();
        let addr2 = listener2.local_addr().unwrap();

        let fd1 = listener1.as_raw_fd();
        let fd2 = listener2.as_raw_fd();

        // Start receiver thread
        let socket_path_clone = socket_path.to_string();
        let receiver = std::thread::spawn(move || {
            receive_fds(&socket_path_clone).unwrap()
        });

        // Wait for receiver to be ready
        std::thread::sleep(Duration::from_millis(100));

        // Send FD
        send_fds(
            vec![fd1, fd2],
            vec![addr1, addr2],
            socket_path,
            5,
            Duration::from_secs(1),
        )
        .unwrap();

        // Wait for reception to complete
        let received_fds = receiver.join().unwrap();

        assert_eq!(received_fds.len(), 2);
        assert!(received_fds.contains_key(&addr1));
        assert!(received_fds.contains_key(&addr2));

        // Note: Since FDs are transferred, the original listeners should not be closed here
        std::mem::forget(listener1);
        std::mem::forget(listener2);
    }
}
