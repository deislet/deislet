// build.rs
// Compiling Protocol Buffers definitions and embedding every lazy Deno source
// that a runtime-created startup snapshot may leave unevaluated.

use deno_core::{Extension, ModuleCodeString, ModuleName};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let residual_sources = write_residual_lazy_sources()?;
    write_snapshot_fingerprint(&residual_sources)?;

    // Compiling storage proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-store/proto/storage.proto"],
            &["../deis-store/proto"],
        )?;

    // Compiling queue proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-queue/proto/queue.proto"],
            &["../deis-queue/proto"],
        )?;

    // Compiling blob proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-store/proto/blob.proto"],
            &["../deis-store/proto"],
        )?;

    // Compiling telemetry proto (client - both services in this file are
    // consumed by this crate: LogService by src/logging/shipper.rs and
    // MetricService by src/runtime/metrics.rs, so neither generated client
    // can be dropped without breaking the build)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["../deis-telemetry/proto/telemetry.proto"],
            &["../deis-telemetry/proto"],
        )?;

    // Compiling management proto (server - receives control-plane commands)
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/management.proto"], &["proto"])?;

    // Compiling trigger proto (server)
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/trigger.proto"], &["proto"])?;

    // Recompile when proto files change
    println!("cargo:rerun-if-changed=../deis-store/proto/storage.proto");
    println!("cargo:rerun-if-changed=../deis-queue/proto/queue.proto");
    println!("cargo:rerun-if-changed=../deis-store/proto/blob.proto");
    println!("cargo:rerun-if-changed=../deis-telemetry/proto/telemetry.proto");
    println!("cargo:rerun-if-changed=proto/management.proto");
    println!("cargo:rerun-if-changed=proto/trigger.proto");

    Ok(())
}

fn lazy_source_extensions() -> Vec<Extension> {
    vec![
        deno_runtime::deno_webidl::deno_webidl::init(),
        deno_runtime::deno_web::deno_web::lazy_init(),
        deno_runtime::deno_fetch::deno_fetch::lazy_init(),
        deno_runtime::deno_websocket::deno_websocket::lazy_init(),
        deno_runtime::deno_crypto::deno_crypto::lazy_init(),
        deno_runtime::deno_tls::deno_tls::init(),
        deno_runtime::deno_net::deno_net::lazy_init(),
        deno_runtime::deno_io::deno_io::lazy_init(),
        deno_runtime::deno_fs::deno_fs::lazy_init(),
        deno_runtime::deno_os::deno_os::lazy_init(),
        deno_node_crypto::deno_node_crypto::init(),
        deno_runtime::deno_node::deno_node::lazy_init::<
            deno_resolver::npm::DenoInNpmPackageChecker,
            deno_resolver::npm::NpmResolver<sys_traits::impls::RealSys>,
            sys_traits::impls::RealSys,
        >(),
    ]
}

fn write_residual_lazy_sources() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut lazy_js = Vec::new();
    let mut lazy_esm = Vec::new();

    for extension in lazy_source_extensions() {
        for file in extension.lazy_loaded_js_files.iter() {
            lazy_js.push(transpiled_source(file)?);
        }
        for file in extension.lazy_loaded_esm_files.iter() {
            lazy_esm.push(transpiled_source(file)?);
        }
    }

    // These two are owned by this crate rather than an upstream extension.
    // Bootstrap consumes them today, but embedding them keeps a future change
    // in fetch initialization from turning into a deployment-only failure.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    for (specifier, relative) in [
        (
            "ext:deno_telemetry/telemetry.ts",
            "src/runtime/deno_telemetry_stub/telemetry.ts",
        ),
        (
            "ext:deno_telemetry/util.ts",
            "src/runtime/deno_telemetry_stub/util.ts",
        ),
    ] {
        let path = manifest_dir.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        let source = fs::read_to_string(path)?;
        let (source, _) = deno_runtime::transpile::maybe_transpile_source(
            ModuleName::from(specifier.to_string()),
            ModuleCodeString::from(source),
        )?;
        lazy_js.push((specifier.to_string(), source.as_str().to_string()));
    }

    lazy_js.sort_by(|left, right| left.0.cmp(&right.0));
    lazy_esm.sort_by(|left, right| left.0.cmp(&right.0));
    lazy_js.dedup_by(|left, right| left.0 == right.0);
    lazy_esm.dedup_by(|left, right| left.0 == right.0);

    let generated = format!(
        "pub static RESIDUAL_LAZY_JS_SOURCES: &[(&str, &str)] = &{};\n\
         pub static RESIDUAL_LAZY_ESM_SOURCES: &[(&str, &str)] = &{};\n",
        source_table(&lazy_js),
        source_table(&lazy_esm),
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("runtime_lazy_sources.rs");
    fs::write(&out, generated)?;
    Ok(out)
}

fn write_snapshot_fingerprint(residual_sources: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("deis-runtime must be inside the workspace crates directory")?;
    let mut inputs = Vec::new();
    collect_snapshot_sources(&manifest_dir.join("src/runtime"), &mut inputs)?;
    inputs.extend([
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("build.rs"),
        workspace.join("Cargo.lock"),
    ]);
    inputs.sort();

    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"deis-runtime-startup-snapshot-v1");
    for path in inputs {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path.strip_prefix(workspace).unwrap_or(&path);
        hash_part(&mut hasher, relative.to_string_lossy().as_bytes());
        hash_part(&mut hasher, &fs::read(&path)?);
    }
    hash_part(&mut hasher, b"generated/runtime_lazy_sources.rs");
    hash_part(&mut hasher, &fs::read(residual_sources)?);

    let digest = hasher.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("cargo:rustc-env=DEIS_RUNTIME_SNAPSHOT_FINGERPRINT={encoded}");
    Ok(())
}

fn collect_snapshot_sources(
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_snapshot_sources(&path, output)?;
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.ends_with("_test.rs") || name == "isolate_test.rs" {
            continue;
        }
        if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("rs" | "js" | "ts")
        ) {
            output.push(path);
        }
    }
    Ok(())
}

fn hash_part(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn transpiled_source(
    file: &deno_core::ExtensionFileSource,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    #[allow(deprecated, reason = "the path variant identifies build inputs")]
    if let deno_core::ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(path) = &file.code {
        println!("cargo:rerun-if-changed={path}");
    }
    let source = file.load()?;
    let (source, _) = deno_runtime::transpile::maybe_transpile_source(
        ModuleName::from(file.specifier.to_string()),
        source,
    )?;
    Ok((file.specifier.to_string(), source.as_str().to_string()))
}

fn source_table(entries: &[(String, String)]) -> String {
    let entries = entries
        .iter()
        .map(|(specifier, source)| format!("({specifier:?}, {source:?})"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("[{entries}]")
}
