use anyhow::{Context, Result};
use deno_core::snapshot::create_snapshot;
use deno_core::snapshot::CreateSnapshotOptions;
use deno_core::{ModuleSpecifier, PollEventLoopOptions};
use once_cell::sync::OnceCell;
use std::fs;
use std::path::PathBuf;

use super::runtime_init::{build_runtime_extensions, init_lazy_extensions};

static STARTUP_SNAPSHOT: OnceCell<Option<&'static [u8]>> = OnceCell::new();

pub fn load_or_create_startup_snapshot() -> Result<Option<&'static [u8]>> {
    STARTUP_SNAPSHOT
        .get_or_try_init(|| {
            let path = snapshot_path();
            let Some(path) = path else {
                tracing::debug!("Startup snapshot disabled: DEIS_RUNTIME_SNAPSHOT_PATH not set");
                return Ok(None);
            };

            if path.exists() {
                let snapshot = load_snapshot(&path)?;
                tracing::info!("Startup snapshot loaded: {}", path.display());
                return Ok(Some(snapshot));
            }

            let snapshot = create_startup_snapshot()?;
            fs::write(&path, snapshot)
                .with_context(|| format!("Failed to write snapshot to {}", path.display()))?;
            tracing::info!("Startup snapshot created: {}", path.display());
            Ok(None)
        })
        .copied()
}

fn snapshot_path() -> Option<PathBuf> {
    std::env::var("DEIS_RUNTIME_SNAPSHOT_PATH")
        .ok()
        .map(PathBuf::from)
}

fn load_snapshot(path: &PathBuf) -> Result<&'static [u8]> {
    let data = fs::read(path)
        .with_context(|| format!("Failed to read snapshot from {}", path.display()))?;
    Ok(Box::leak(data.into_boxed_slice()))
}

fn create_startup_snapshot() -> Result<&'static [u8]> {
    // Create snapshot in a separate thread to avoid nested tokio runtime issues
    // (create_snapshot may call block_on internally)
    std::thread::spawn(|| {
        let extensions = build_runtime_extensions();
        let output = create_snapshot(
            CreateSnapshotOptions {
                cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
                startup_snapshot: None,
                skip_op_registration: false,
                extensions,
                extension_transpiler: None,
                with_runtime_cb: Some(Box::new(|runtime| {
                    if let Err(err) = init_lazy_extensions(runtime) {
                        panic!("Failed to init lazy extensions for snapshot: {}", err);
                    }

                    // Execute bootstrap.js to initialize Web API global objects
                    let bootstrap_specifier =
                        ModuleSpecifier::parse("ext:bootstrap/src/runtime/js/bootstrap.js")
                            .expect("Failed to parse bootstrap specifier");

                    // Create temporary tokio runtime to execute async module loading
                    let tokio_rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to build tokio runtime for snapshot");

                    tokio_rt.block_on(async {
                        let bootstrap_id = runtime
                            .load_side_es_module(&bootstrap_specifier)
                            .await
                            .expect("Failed to load bootstrap module");
                        let bootstrap_eval = runtime.mod_evaluate(bootstrap_id);
                        runtime
                            .with_event_loop_promise(
                                bootstrap_eval,
                                PollEventLoopOptions::default(),
                            )
                            .await
                            .expect("Failed to evaluate bootstrap module");
                    });
                })),
            },
            None,
        )?;

        Ok(Box::leak(output.output) as &'static [u8])
    })
    .join()
    .unwrap()
}
