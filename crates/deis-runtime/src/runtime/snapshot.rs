use anyhow::{anyhow, bail, Context, Result};
use deno_core::snapshot::create_snapshot;
use deno_core::snapshot::CreateSnapshotOptions;
use fs2::FileExt;
use once_cell::sync::OnceCell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::fetch_policy::FetchPolicy;
use super::runtime_init::{build_snapshot_extensions, init_snapshot_lazy_extensions};

const SNAPSHOT_MAGIC: &[u8] = b"DEIS_RUNTIME_SNAPSHOT\0V1\n";
const SNAPSHOT_DIGEST_BYTES: usize = 32;
const SNAPSHOT_LENGTH_BYTES: usize = 8;

static STARTUP_SNAPSHOT: OnceCell<Option<&'static [u8]>> = OnceCell::new();
static SNAPSHOT_FINGERPRINT: OnceCell<[u8; SNAPSHOT_DIGEST_BYTES]> = OnceCell::new();

pub fn load_or_create_startup_snapshot() -> Result<Option<&'static [u8]>> {
    STARTUP_SNAPSHOT
        .get_or_try_init(|| {
            let Some(path) = snapshot_path() else {
                let snapshot = create_startup_snapshot()?;
                tracing::info!(
                    "Startup snapshot created in memory; set DEIS_RUNTIME_SNAPSHOT_PATH to reuse it across process restarts"
                );
                return Ok(Some(snapshot));
            };

            reject_snapshot_symlink(&path)?;
            let _lock = lock_snapshot_path(&path)?;
            reject_snapshot_symlink(&path)?;

            if path.exists() {
                match load_snapshot(&path) {
                    Ok(snapshot) => {
                        tracing::info!("Startup snapshot loaded: {}", path.display());
                        return Ok(Some(snapshot));
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "Startup snapshot cache is incompatible or damaged; rebuilding it"
                        );
                    }
                }
            }

            let snapshot = create_startup_snapshot()?;
            write_snapshot(&path, snapshot)?;
            tracing::info!("Startup snapshot created: {}", path.display());
            Ok(Some(snapshot))
        })
        .copied()
}

fn snapshot_path() -> Option<PathBuf> {
    std::env::var("DEIS_RUNTIME_SNAPSHOT_PATH")
        .ok()
        .map(PathBuf::from)
}

fn reject_snapshot_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "Snapshot cache path must not be a symlink: {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to inspect snapshot path {}", path.display())),
    }
}

fn lock_snapshot_path(path: &Path) -> Result<File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Snapshot path must name a file: {}", path.display()))?;
    let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));

    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    set_private_create_mode(&mut options);
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("Failed to open snapshot lock {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("Failed to lock snapshot cache {}", path.display()))?;
    Ok(lock)
}

fn load_snapshot(path: &Path) -> Result<&'static [u8]> {
    let data = fs::read(path)
        .with_context(|| format!("Failed to read snapshot from {}", path.display()))?;
    let snapshot = decode_snapshot(&data)
        .with_context(|| format!("Snapshot cache {} is not reusable", path.display()))?;
    Ok(Box::leak(snapshot.to_vec().into_boxed_slice()))
}

fn snapshot_fingerprint() -> &'static [u8; SNAPSHOT_DIGEST_BYTES] {
    SNAPSHOT_FINGERPRINT.get_or_init(|| {
        let decoded = hex::decode(env!("DEIS_RUNTIME_SNAPSHOT_FINGERPRINT"))
            .expect("build.rs emits a hexadecimal startup snapshot fingerprint");
        decoded
            .try_into()
            .expect("the startup snapshot fingerprint is exactly 32 bytes")
    })
}

fn encode_snapshot(snapshot: &[u8]) -> Vec<u8> {
    let mut envelope = Vec::with_capacity(
        SNAPSHOT_MAGIC.len() + SNAPSHOT_DIGEST_BYTES + SNAPSHOT_LENGTH_BYTES + snapshot.len(),
    );
    envelope.extend_from_slice(SNAPSHOT_MAGIC);
    envelope.extend_from_slice(snapshot_fingerprint());
    envelope.extend_from_slice(&(snapshot.len() as u64).to_be_bytes());
    envelope.extend_from_slice(snapshot);
    envelope
}

fn decode_snapshot(envelope: &[u8]) -> Result<&[u8]> {
    let header_len = SNAPSHOT_MAGIC.len() + SNAPSHOT_DIGEST_BYTES + SNAPSHOT_LENGTH_BYTES;
    if envelope.len() < header_len {
        bail!("snapshot envelope is truncated");
    }
    if &envelope[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        bail!("snapshot envelope has an unknown format or version");
    }

    let fingerprint_start = SNAPSHOT_MAGIC.len();
    let fingerprint_end = fingerprint_start + SNAPSHOT_DIGEST_BYTES;
    if &envelope[fingerprint_start..fingerprint_end] != snapshot_fingerprint() {
        bail!("snapshot fingerprint does not match this runtime build");
    }

    let length_end = fingerprint_end + SNAPSHOT_LENGTH_BYTES;
    let declared = u64::from_be_bytes(
        envelope[fingerprint_end..length_end]
            .try_into()
            .expect("the length field has a fixed width"),
    );
    let payload = &envelope[length_end..];
    if declared == 0 || declared != payload.len() as u64 {
        bail!(
            "snapshot payload length is invalid: declared {declared}, found {}",
            payload.len()
        );
    }
    Ok(payload)
}

fn write_snapshot(path: &Path, snapshot: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Snapshot path must name a file: {}", path.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let envelope = encode_snapshot(snapshot);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        set_private_create_mode(&mut options);
        let mut file = options.open(&temporary).with_context(|| {
            format!(
                "Failed to create snapshot staging file {}",
                temporary.display()
            )
        })?;
        file.write_all(&envelope).with_context(|| {
            format!(
                "Failed to write snapshot staging file {}",
                temporary.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to sync snapshot staging file {}",
                temporary.display()
            )
        })?;
        drop(file);

        replace_snapshot_file(&temporary, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_create_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_create_mode(_options: &mut OpenOptions) {}

#[cfg(not(windows))]
fn replace_snapshot_file(temporary: &Path, destination: &Path) -> Result<()> {
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "Failed to install snapshot cache {} from {}",
            destination.display(),
            temporary.display()
        )
    })
}

#[cfg(windows)]
fn replace_snapshot_file(temporary: &Path, destination: &Path) -> Result<()> {
    // Windows rename does not replace an existing file. The process lock
    // prevents another Deislet process from observing this cache transition;
    // a crash between removal and rename leaves a missing cache, which the
    // next start safely rebuilds rather than a partial V8 blob.
    if destination.exists() {
        fs::remove_file(destination).with_context(|| {
            format!(
                "Failed to replace stale snapshot cache {}",
                destination.display()
            )
        })?;
    }
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "Failed to install snapshot cache {} from {}",
            destination.display(),
            temporary.display()
        )
    })
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Failed to sync snapshot directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

fn create_startup_snapshot() -> Result<&'static [u8]> {
    // Create snapshot in a separate thread to avoid nested tokio runtime issues
    // (create_snapshot may call block_on internally).
    std::thread::spawn(|| {
        let extensions = build_snapshot_extensions();
        let output = create_snapshot(
            CreateSnapshotOptions {
                cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
                startup_snapshot: None,
                skip_op_registration: false,
                extensions,
                extension_transpiler: Some(Rc::new(|specifier, source| {
                    deno_runtime::transpile::maybe_transpile_source(specifier, source)
                })),
                with_runtime_cb: Some(Box::new(|runtime| {
                    // deno_node's vm implementation stores its context in a
                    // designated snapshot slot. This is the supported setup
                    // used by deno_runtime's own snapshot builder.
                    {
                        let isolate = runtime.v8_isolate();
                        deno_core::v8::scope!(scope, isolate);
                        let template = deno_runtime::deno_node::init_global_template(
                            scope,
                            deno_runtime::deno_node::ContextInitMode::ForSnapshot,
                        );
                        let context = deno_runtime::deno_node::create_v8_context(
                            scope,
                            template,
                            deno_runtime::deno_node::ContextInitMode::ForSnapshot,
                            std::ptr::null_mut(),
                        );
                        assert_eq!(
                            scope.add_context(context),
                            deno_runtime::deno_node::VM_CONTEXT_INDEX
                        );
                    }

                    // The snapshot is the shape of the isolate, not any one
                    // application's permissions: nothing fetches while it is
                    // being taken, and every isolate restored from it installs
                    // its own policy in `AppRuntime::new`.
                    if let Err(err) = init_snapshot_lazy_extensions(
                        runtime,
                        &FetchPolicy::default(),
                        "<snapshot>",
                    ) {
                        panic!("Failed to init lazy extensions for snapshot: {err}");
                    }

                    runtime
                        .execute_script(
                            "<node-next-tick>",
                            r#"globalThis.Deno.core
                                .loadExtScript("ext:deno_node/_next_tick.ts")
                                .enableNextTick();"#,
                        )
                        .expect("initialize the selected Node nextTick queue");
                    runtime
                        .execute_script("<node-compat>", include_str!("js/node_compat.js"))
                        .expect("initialize the selected Node facade");

                    // Extension entry points have already been evaluated by
                    // `JsRuntimeForSnapshot::new`. Evaluating bootstrap again
                    // here would allocate platform state twice. In particular,
                    // snapshot V8 isolates intentionally have no CppHeap, so a
                    // cppgc-backed Web Crypto singleton would abort instead of
                    // returning an error.
                })),
            },
            None,
        )?;

        Ok(Box::leak(output.output) as &'static [u8])
    })
    .join()
    .map_err(|_| anyhow!("Startup snapshot worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_envelope_round_trips_only_for_this_runtime_build() {
        let payload = b"not a real V8 snapshot, only an envelope fixture";
        let encoded = encode_snapshot(payload);
        assert_eq!(decode_snapshot(&encoded).unwrap(), payload);

        let mut wrong_build = encoded.clone();
        wrong_build[SNAPSHOT_MAGIC.len()] ^= 0xff;
        assert!(decode_snapshot(&wrong_build)
            .unwrap_err()
            .to_string()
            .contains("fingerprint"));

        let mut truncated = encoded.clone();
        truncated.pop();
        assert!(decode_snapshot(&truncated)
            .unwrap_err()
            .to_string()
            .contains("length"));

        let mut old_raw_blob = payload.to_vec();
        old_raw_blob.resize(encoded.len(), 0);
        assert!(decode_snapshot(&old_raw_blob)
            .unwrap_err()
            .to_string()
            .contains("format"));
    }
}
