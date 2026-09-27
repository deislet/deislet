//! What bounds this node when everything inside it fails to.
//!
//! Every ceiling the runtime applies to an isolate -- the JS heap limit, the
//! subrequest allowance, the execution deadline -- is applied by code running
//! on the same machine, out of memory the same kernel gave it. A backstop is
//! the ceiling that holds when those do not: the kernel refusing the process
//! more memory than its cgroup allows, the operator's `OOMPolicy` ending the
//! service rather than the machine. Neither can be reimplemented inside the
//! process, which is why this module only *detects* -- it answers whether the
//! node runs under a cgroup v2 `memory.max`, and what it is.
//!
//! Detection happens once, at startup, and the answer feeds readiness: a node
//! without the backstop still serves, but reports `degraded` with a reason
//! saying so, because "running, and able to take its neighbours down if one
//! isolate runs away" is not the promise a `ready` answer makes. See
//! `docs/adr/018-resource-backstops.md`.

use std::path::Path;

/// What the node found when it looked for a cgroup memory ceiling on itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBackstop {
    /// The ceiling in bytes, when there is one. `None` means no usable
    /// backstop: not in a cgroup v2 hierarchy, no `memory.max` file, or one
    /// reading `max` -- unlimited, which is a backstop in name only.
    pub limit_bytes: Option<u64>,
    /// Where the answer came from, for the log line and the readiness
    /// detail: an operator told "the backstop is missing" next asks where
    /// the node looked.
    pub where_from: String,
}

impl MemoryBackstop {
    /// Whether a usable ceiling was found.
    pub fn present(&self) -> bool {
        self.limit_bytes.is_some()
    }
}

/// Look for this process's cgroup v2 memory ceiling.
///
/// `proc_cgroups` is normally `/proc/self/cgroup` and `cgroup_root`
/// `/sys/fs/cgroup`; both are parameters so a test can point them at files
/// it wrote. The unified-hierarchy line -- the one starting `0::` -- names
/// the directory under `cgroup_root` whose `memory.max` is this process's.
pub fn detect(proc_cgroups: &Path, cgroup_root: &Path) -> std::io::Result<MemoryBackstop> {
    let cgroup_line = std::fs::read_to_string(proc_cgroups)?;
    let relative = cgroup_line
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_string);
    let memory_max = relative.as_ref().and_then(|relative| {
        std::fs::read_to_string(
            cgroup_root
                .join(relative.trim_start_matches('/'))
                .join("memory.max"),
        )
        .ok()
    });
    Ok(from_parts(
        relative.as_deref(),
        memory_max.as_deref(),
        cgroup_root.to_string_lossy().as_ref(),
    ))
}

/// The decision, from the parts [`detect`] reads, so tests can inject the
/// contents of a `memory.max` without a container.
///
/// The rule, in the order the cases arise on a real node: a process outside
/// cgroup v2 (the `0::` line absent) has no ceiling at all; a hierarchy
/// without a `memory.max` file for it has a ceiling nobody set; and a
/// `memory.max` of `max` is the kernel's spelling for *no limit* -- present
/// as a file, useless as a backstop. Only a number bounds the node.
pub fn from_parts(
    cgroup_line: Option<&str>,
    memory_max: Option<&str>,
    root: &str,
) -> MemoryBackstop {
    let where_from = format!("{root}/memory.max");
    let Some(cgroup_line) = cgroup_line else {
        return MemoryBackstop {
            limit_bytes: None,
            where_from: format!("{where_from}: this process reports no cgroup v2 (0::) hierarchy"),
        };
    };
    let Some(memory_max) = memory_max else {
        return MemoryBackstop {
            limit_bytes: None,
            where_from: format!(
                "{where_from}: no memory.max in the cgroup this process runs in ({}), so no \
                 ceiling is set",
                cgroup_line.trim()
            ),
        };
    };
    let trimmed = memory_max.trim();
    if trimmed == "max" {
        return MemoryBackstop {
            limit_bytes: None,
            where_from: format!(
                "{where_from}: memory.max is `max`, which is unlimited and bounds nothing"
            ),
        };
    }
    match trimmed.parse::<u64>() {
        Ok(bytes) => MemoryBackstop {
            limit_bytes: Some(bytes),
            where_from,
        },
        Err(_) => MemoryBackstop {
            limit_bytes: None,
            where_from: format!(
                "{where_from}: memory.max is `{trimmed}`, which is neither a byte count nor \
                 `max`; treating it as no ceiling"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one shape a well-configured canary node has: a `0::` line in
    /// `/proc/self/cgroup` and a byte count in the matching `memory.max`.
    #[test]
    fn a_byte_count_in_memory_max_is_a_backstop() {
        let found = from_parts(
            Some("0::/system.slice/deis-runtime.service\n"),
            Some("2147483648\n"),
            "/sys/fs/cgroup",
        );
        assert_eq!(found.limit_bytes, Some(2_147_483_648));
        assert!(found.present());
    }

    /// Every way a node can end up without a usable ceiling, and each is
    /// named so the operator can act on it rather than puzzle over it.
    #[test]
    fn each_way_of_having_no_ceiling_is_told_apart() {
        let outside = from_parts(None, Some("4096\n"), "/sys/fs/cgroup");
        assert_eq!(outside.limit_bytes, None);
        assert!(
            outside.where_from.contains("no cgroup v2"),
            "{}",
            outside.where_from
        );

        let no_file = from_parts(Some("0::/user.slice\n"), None, "/sys/fs/cgroup");
        assert_eq!(no_file.limit_bytes, None);
        assert!(no_file.where_from.contains("no memory.max"));

        let unlimited = from_parts(Some("0::/\n"), Some("max\n"), "/sys/fs/cgroup");
        assert_eq!(unlimited.limit_bytes, None);
        assert!(unlimited.where_from.contains("unlimited"));

        let unparseable = from_parts(Some("0::/\n"), Some("soon\n"), "/sys/fs/cgroup");
        assert_eq!(unparseable.limit_bytes, None);
        assert!(unparseable.where_from.contains("neither a byte count"));
    }

    /// The end-to-end read, against files on disk: the cgroup path from
    /// `/proc/self/cgroup` picks the directory under the root the
    /// `memory.max` is read from.
    #[test]
    fn detection_reads_the_memory_max_of_the_cgroup_the_process_is_in() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("system.slice/deis-runtime.service"))
            .expect("the cgroup directory");
        std::fs::write(
            dir.path()
                .join("system.slice/deis-runtime.service/memory.max"),
            "1073741824\n",
        )
        .expect("the memory.max");
        let proc_cgroups = dir.path().join("self_cgroup");
        std::fs::write(
            &proc_cgroups,
            "11:cpu:/\n0::/system.slice/deis-runtime.service\n",
        )
        .expect("the cgroup list");

        let found = detect(&proc_cgroups, dir.path()).expect("the read");
        assert_eq!(found.limit_bytes, Some(1_073_741_824));
        assert!(found.present());

        // And a sibling cgroup's ceiling is not this process's: an empty
        // neighbour changes nothing.
        std::fs::create_dir_all(dir.path().join("user.slice")).expect("the neighbour");
        std::fs::write(dir.path().join("user.slice/memory.max"), "max\n").expect("the neighbour's");
        let found = detect(&proc_cgroups, dir.path()).expect("the read");
        assert_eq!(found.limit_bytes, Some(1_073_741_824));
    }

    /// A missing `/proc/self/cgroup` is an error at the read, which the
    /// caller turns into a missing backstop rather than a failed start --
    /// detection must never be the thing that keeps a node from serving.
    #[test]
    fn an_unreadable_proc_file_is_an_error_the_caller_degrades_on() {
        let found = detect(
            Path::new("/nonexistent/cgroup"),
            Path::new("/sys/fs/cgroup"),
        );
        assert!(found.is_err());
    }
}
