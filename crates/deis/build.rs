//! Generates the two gRPC clients this CLI speaks.
//!
//! * `deis.telemetry` - what `deis tail` and `deis logs` read from.
//! * `deis.management` - what `deis dev` uses to hand a freshly built bundle
//!   to the deis-runtime it started, exactly the way the control plane hands
//!   one to a node in production.
//!
//! Both protos are read from the crate that serves them rather than copied
//! here: two copies of a contract drift, and the symptom would be a CLI that
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

    println!("cargo:rerun-if-changed=../deis-telemetry/proto/telemetry.proto");
    println!("cargo:rerun-if-changed=../deis-runtime/proto/management.proto");
    Ok(())
}
