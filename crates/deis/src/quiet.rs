//! Keeping `--json` output parseable.
//!
//! `deis deploy --json` has to print one JSON document on stdout and nothing
//! else. The compiler it calls into (`deis-build`) reports its progress with
//! `println!`, which goes to the same stream, so a script piping the result
//! into `jq` would be handed a build log with a JSON document at the end of
//! it.
//!
//! This guard points file descriptor 1 at file descriptor 2 for as long as it
//! is alive, so that progress still reaches the terminal - on stderr, where
//! progress belongs - while stdout stays empty for the document. Dropping the
//! guard puts the original descriptor back.
//!
//! The right long-term fix is for `deis-build` to write its progress to
//! stderr itself; this guard is what keeps `--json` honest until it does.

use std::io::Write;

/// Redirects this process's stdout to stderr until it is dropped.
pub struct StdoutToStderr {
    #[cfg(unix)]
    saved: Option<std::os::fd::OwnedFd>,
}

impl StdoutToStderr {
    /// Start redirecting. A failure is reported on stderr and then ignored:
    /// the command it wraps is worth running either way, and the caller finds
    /// out because the JSON it prints will have a build log in front of it.
    #[cfg(unix)]
    pub fn start() -> Self {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // Anything already buffered belongs on the real stdout.
        let _ = std::io::stdout().flush();

        // SAFETY: `dup` and `dup2` on the process's own standard descriptors.
        // The duplicate of fd 1 is wrapped in an `OwnedFd` so it is closed
        // exactly once, and nothing else in this process touches raw
        // descriptors.
        let saved = unsafe {
            let copy = libc::dup(libc::STDOUT_FILENO);
            if copy < 0 {
                eprintln!(
                    "warning: could not duplicate stdout, so build progress will be mixed into \
                     the JSON output of this command"
                );
                return Self { saved: None };
            }
            let saved = OwnedFd::from_raw_fd(copy);
            if libc::dup2(std::io::stderr().as_raw_fd(), libc::STDOUT_FILENO) < 0 {
                eprintln!(
                    "warning: could not redirect stdout, so build progress will be mixed into \
                     the JSON output of this command"
                );
                return Self { saved: None };
            }
            saved
        };

        Self { saved: Some(saved) }
    }

    #[cfg(not(unix))]
    pub fn start() -> Self {
        eprintln!(
            "warning: on this platform build progress cannot be moved off stdout, so the JSON \
             output of this command may have a build log in front of it"
        );
        Self {}
    }
}

impl Drop for StdoutToStderr {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // Whatever the wrapped code printed went to stderr; flush it
            // there before the descriptor changes back underneath it.
            let _ = std::io::stdout().flush();
            if let Some(saved) = self.saved.take() {
                // SAFETY: `saved` is this process's own former stdout,
                // duplicated in `start`.
                unsafe {
                    libc::dup2(saved.as_raw_fd(), libc::STDOUT_FILENO);
                }
            }
        }
    }
}
