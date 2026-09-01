//! Generates the gRPC clients this CLI speaks.
//!
//! * `deis.telemetry` - what `deis tail` and `deis logs` read from.
//! * `deis.management` - what `deis dev` uses to hand a freshly built bundle
//!   to the deis-runtime it started, exactly the way the control plane hands
//!   one to a node in production.
//! * `deis.storage` (KV and the SQL database) and `deis.blob` - what `deis
//!   kv`, `deis db` and `deis blob` read and write, the same two proto files
//!   deis-runtime's own storage clients are generated from.
//! * `deis.queue` - what `deis queue dead-letters` and `deis queue replay`
//!   read and write. Only the two operator calls are used here; producing and
//!   consuming messages is an application's job, and this CLI has no business
//!   draining a queue an application is meant to be working through.
//!
//! Every proto is read from the crate that serves it rather than copied here:
//! two copies of a contract drift, and the symptom would be a CLI that
//! silently stops matching the running service. Only the client halves are
//! generated - this crate serves nothing.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-telemetry/proto/telemetry.proto"],
            &["../deis-telemetry/proto"],
        )?;

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-runtime/proto/management.proto"],
            &["../deis-runtime/proto"],
        )?;

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &[
                "../deis-store/proto/storage.proto",
                "../deis-store/proto/blob.proto",
            ],
            &["../deis-store/proto"],
        )?;

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-queue/proto/queue.proto"],
            &["../deis-queue/proto"],
        )?;

    println!("cargo:rerun-if-changed=../deis-telemetry/proto/telemetry.proto");
    println!("cargo:rerun-if-changed=../deis-runtime/proto/management.proto");
    println!("cargo:rerun-if-changed=../deis-store/proto/storage.proto");
    println!("cargo:rerun-if-changed=../deis-store/proto/blob.proto");
    println!("cargo:rerun-if-changed=../deis-queue/proto/queue.proto");
    Ok(())
}
