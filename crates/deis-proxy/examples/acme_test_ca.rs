//! The stub certificate authority as a program, for `scripts/demo.sh`.
//!
//! The demo shows automatic certificates working end to end, which needs an
//! authority to talk to. It must not be a real one: Let's Encrypt's staging
//! environment has rate limits, needs names that resolve on the public
//! internet, and would make a local demo fail for reasons that have nothing
//! to do with this platform. So the demo runs this instead - the same stub
//! the integration tests drive, on a port the script chose, standing in the
//! role the demo's `python3 -m http.server` origin stands in: not a
//! component, just something real for a component to talk to.
//!
//! It is not built by `cargo build --workspace`; the demo asks for it by
//! name. Nothing in the shipped binary can reach it.

#[path = "../tests/support/acme_ca.rs"]
mod acme_ca;

use std::path::PathBuf;

use acme_ca::{CaOptions, StubCa};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut port: u16 = 0;
    let mut validation_port: u16 = 80;
    let mut root_out: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--port" => port = value().parse().expect("--port must be a port number"),
            "--validation-port" => {
                validation_port = value()
                    .parse()
                    .expect("--validation-port must be a port number")
            }
            "--root-out" => root_out = Some(PathBuf::from(value())),
            other => panic!(
                "unknown option {other}; usage: acme_test_ca --port <p> --validation-port <p> \
                 [--root-out <file>]"
            ),
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the stub authority");
    runtime.block_on(async move {
        let ca = StubCa::start_on(
            port,
            CaOptions {
                validation_port,
                ..Default::default()
            },
        )
        .await;
        if let Some(path) = &root_out {
            ca.write_root_pem(path);
        }
        // The line the script waits for, and the two facts it needs: where
        // the directory is, and where the root it must trust was written.
        println!("directory {}", ca.directory_url);
        if let Some(path) = &root_out {
            println!("root {}", path.display());
        }
        println!("validating against 127.0.0.1:{validation_port}");
        // Serving is the whole job; it ends when the script stops the
        // process.
        std::future::pending::<()>().await;
    });
}
