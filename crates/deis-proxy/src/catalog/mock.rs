//! An in-process stand-in for the control plane's routing-table endpoint.
//!
//! It speaks just enough HTTP/1.1 to be the other end of
//! [`super::CatalogClient`]: one connection per request, `Connection:
//! close`, and whatever answer the test asked for. It exists so the failure
//! paths that matter - a control plane that has gone away, an answer cut
//! short, a credential refused - are exercised against a real socket rather
//! than against a mocked-out client.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// What the next request is answered with.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// 200 with this body.
    Ok(String),
    /// This status with this body.
    Status(u16, String),
    /// 200 announcing the whole body and sending only the first `sent`
    /// bytes of it, then closing.
    CutShort(String, usize),
}

pub(crate) struct MockControl {
    /// Address in the form the configuration uses.
    pub addr: String,
    answer: Arc<Mutex<Answer>>,
    last_authorization: Arc<Mutex<Option<String>>>,
    accepting: JoinHandle<()>,
}

impl MockControl {
    pub(crate) async fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = Arc::new(Mutex::new(answer));
        let last_authorization = Arc::new(Mutex::new(None));

        let served = Arc::clone(&answer);
        let seen = Arc::clone(&last_authorization);
        let accepting = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = Arc::clone(&served);
                let seen = Arc::clone(&seen);
                // Serve on the accept task itself: one request at a time is
                // all the client ever makes, and it keeps every socket owned
                // by the task that `stop` aborts.
                serve(stream, served, seen).await;
            }
        });

        Self {
            addr: format!("http://127.0.0.1:{port}"),
            answer,
            last_authorization,
            accepting,
        }
    }

    /// What the next request is answered with.
    pub(crate) fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// The `Authorization` header of the last request served.
    pub(crate) fn last_authorization(&self) -> Option<String> {
        self.last_authorization.lock().unwrap().clone()
    }

    /// Stop listening, so the next connection is refused. This is a control
    /// plane going away, which is the case the data plane has to survive.
    pub(crate) async fn stop(&mut self) {
        self.accepting.abort();
        // Wait for the task to actually be gone, so the listening socket is
        // closed before the test dials again. Waiting on the join handle is
        // what makes this deterministic rather than timing-dependent.
        let _ = (&mut self.accepting).await;
    }
}

impl Drop for MockControl {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    answer: Arc<Mutex<Answer>>,
    last_authorization: Arc<Mutex<Option<String>>>,
) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => request.push(byte[0]),
        }
        if request.len() > 8192 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&request).to_string();
    *last_authorization.lock().unwrap() = text
        .lines()
        .find_map(|line| line.strip_prefix("Authorization: "))
        .map(|value| value.trim().to_string());

    let answer = answer.lock().unwrap().clone();
    let response = match answer {
        Answer::Ok(body) => head(200, body.len()) + &body,
        Answer::Status(status, body) => head(status, body.len()) + &body,
        Answer::CutShort(body, sent) => {
            let sent = sent.min(body.len());
            head(200, body.len()) + &body[..sent]
        }
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

fn head(status: u16, content_length: usize) -> String {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        503 => "Service Unavailable",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: \
         {content_length}\r\nConnection: close\r\n\r\n"
    )
}
