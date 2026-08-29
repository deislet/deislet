//! Noticing that the project changed.
//!
//! The watcher reports *that* something changed, not what: `deis dev`
//! answers every change the same way - build the bundle again and hand it to
//! the node - so classifying files here would only be a second place for the
//! rules about which files matter to disagree with the compiler's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{Receiver, channel};

/// The one file in the project root worth watching.
///
/// The root is watched without recursion and filtered down to this, because
/// the build output directory is also in the root: rebuilding because the
/// build wrote something would rebuild forever.
const ROOT_FILE: &str = ".config.json";

/// Holds the watcher alive. Dropping it stops the notifications.
pub struct Changes {
    _watcher: RecommendedWatcher,
    pub rx: Receiver<PathBuf>,
}

/// Watch the source directories in `roots`, plus `.config.json` in
/// `project_dir`, and report changed files.
pub fn watch(project_dir: &Path, roots: &[PathBuf]) -> Result<Changes> {
    let (tx, rx) = channel(256);
    let project_dir = project_dir.to_path_buf();
    let watched_root = project_dir.clone();

    let mut watcher = RecommendedWatcher::new(
        move |event: Result<Event, notify::Error>| {
            let Ok(event) = event else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for path in event.paths {
                if interesting(&path, &watched_root) {
                    // A full channel means a rebuild is already queued, which
                    // is exactly what this event would have asked for.
                    let _ = tx.try_send(path);
                }
            }
        },
        Config::default(),
    )
    .context("starting the file watcher")?;

    watcher
        .watch(&project_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", project_dir.display()))?;

    for root in roots {
        if root.exists() {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", root.display()))?;
        }
    }

    Ok(Changes {
        _watcher: watcher,
        rx,
    })
}

/// Whether a path is worth a rebuild.
fn interesting(path: &Path, project_dir: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    // Anything directly in the project root is either the build output or
    // configuration, so only the one file that changes what gets built is
    // taken; `dist/` landing here is how a rebuild loop would start.
    if path.parent() == Some(project_dir) {
        return name == ROOT_FILE;
    }

    // Editors write a swarm of temporary files next to the one being saved.
    // Rebuilding on those means rebuilding several times per keystroke.
    !(name.starts_with('.')
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_asset_changes_both_count() {
        let root = Path::new("/p");
        assert!(interesting(Path::new("/p/functions/index.ts"), root));
        assert!(interesting(Path::new("/p/public/logo.svg"), root));
        assert!(interesting(Path::new("/p/public/css/app.css"), root));
    }

    /// The build writes into the project root, so anything the build itself
    /// touches has to be ignored or every rebuild schedules the next one.
    #[test]
    fn the_build_output_in_the_project_root_never_triggers_a_rebuild() {
        let root = Path::new("/p");
        assert!(!interesting(Path::new("/p/dist"), root));
        assert!(!interesting(Path::new("/p/node_modules"), root));
        assert!(interesting(Path::new("/p/.config.json"), root));
    }

    #[test]
    fn an_editors_scratch_files_do_not_trigger_a_rebuild() {
        let root = Path::new("/p");
        assert!(!interesting(Path::new("/p/functions/.index.ts.swp"), root));
        assert!(!interesting(Path::new("/p/functions/index.ts~"), root));
        assert!(!interesting(Path::new("/p/functions/index.ts.tmp"), root));
    }
}
