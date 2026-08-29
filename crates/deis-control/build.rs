fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The dashboard is embedded from `web/dist` with rust-embed, and that
    // directory is a build product of `web/` - `npm run build` writes it, git
    // does not carry it. rust-embed fails to expand when the directory is
    // absent, so a fresh clone could not compile this crate at all until
    // somebody happened to run npm first.
    //
    // Creating it here costs nothing and makes the failure the right shape: a
    // binary built without the dashboard compiles, serves the whole API, and
    // answers 404 for the dashboard pages - which is what
    // `src/api/static_files.rs` already says happens. An existing directory is
    // left exactly as it is, so a real build is never overwritten.
    let dist = std::path::Path::new("web/dist");
    if !dist.exists() {
        std::fs::create_dir_all(dist)?;
        println!(
            "cargo:warning=crates/deis-control/web/dist was missing, so this binary has no \
             dashboard: the API works and the dashboard pages answer 404. Run `npm install && \
             npm run build` in crates/deis-control/web and rebuild to include it."
        );
    }

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
