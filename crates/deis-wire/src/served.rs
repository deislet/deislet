//! A connection a data service accepted, in the shape tonic serves.
//!
//! deis-store, deis-queue and deis-telemetry terminate TLS themselves (see
//! `deis_identity::serve`), then hand tonic the finished stream through
//! `serve_with_incoming`. tonic asks every such stream for [`Connected`], and
//! implements it for a TLS stream only behind a feature that would bring its
//! own TLS configuration along with it - one that verifies by host name and
//! cannot follow a certificate renewal. This wrapper is the one line tonic
//! needs, kept generic so that this crate stays free of a TLS stack.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::{Connected, TcpConnectInfo};

/// An accepted stream, with the addresses of the socket underneath it.
///
/// The addresses are read once, when the connection is accepted, and are what
/// `Request::remote_addr` answers for the life of the connection - the same
/// answer a plaintext tonic server gives.
#[derive(Debug)]
pub struct Served<S> {
    io: S,
    info: TcpConnectInfo,
}

impl<S> Served<S> {
    pub fn new(io: S, local_addr: Option<SocketAddr>, remote_addr: Option<SocketAddr>) -> Self {
        Self {
            io,
            info: TcpConnectInfo {
                local_addr,
                remote_addr,
            },
        }
    }
}

impl<S> Connected for Served<S> {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Served<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Served<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}
