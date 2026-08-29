fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The deis-runtime protos are the single source of truth and live in the
    // deis-runtime crate; only the client side is generated here.
    //
    // Two services are dialled from the control plane: management (deployment
    // delivery, status, health) and trigger (cron fires a scheduled event on
    // one chosen node).
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &[
                "../deis-runtime/proto/management.proto",
                "../deis-runtime/proto/trigger.proto",
            ],
            &["../deis-runtime/proto"],
        )?;

    println!("cargo:rerun-if-changed=../deis-runtime/proto/management.proto");
    println!("cargo:rerun-if-changed=../deis-runtime/proto/trigger.proto");
    Ok(())
}
