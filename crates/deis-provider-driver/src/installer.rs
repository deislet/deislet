use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};
use tempfile::Builder as TempDirBuilder;

#[cfg(windows)]
use std::sync::Arc;

use crate::{
    DriverHostDistribution, DriverReleaseComponentRole, DriverReleaseError, HostOperatingSystem,
    PinnedExecutable, Sha256Digest, ValidatedProviderDriverManifest,
    ValidatedProviderDriverRelease,
};

const INSTALL_LOCK_FILE: &str = ".install.lock";
const STAGING_PREFIX: &str = ".driver-staging.";

#[derive(Clone, Debug)]
pub struct DriverReleaseInstaller {
    root: PathBuf,
    #[cfg(windows)]
    _root_guard: Arc<File>,
}

#[derive(Clone, Debug)]
pub struct InstalledDriverRelease {
    pub release_digest: Sha256Digest,
    pub host_operating_system: HostOperatingSystem,
    pub root: PathBuf,
    distribution: DriverHostDistribution,
}

#[derive(Clone, Debug)]
pub struct InstalledReleaseExecutable {
    pub role: DriverReleaseComponentRole,
    pub name: String,
    pub executable: PinnedExecutable,
    pub argument_prefix: Vec<String>,
}

impl DriverReleaseInstaller {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, DriverInstallationError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(DriverInstallationError::Invalid(
                "provider driver installation root must be absolute",
            ));
        }
        validate_installation_root(root)?;
        let root = fs::canonicalize(root).map_err(|error| {
            DriverInstallationError::io("canonicalize installation root", error)
        })?;
        #[cfg(windows)]
        let root_guard = Arc::new(
            crate::windows_security::open_private_directory(&root)
                .map_err(installation_security_error)?,
        );
        Ok(Self {
            root,
            #[cfg(windows)]
            _root_guard: root_guard,
        })
    }

    pub fn install(
        &self,
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
        host: HostOperatingSystem,
        archive_path: impl AsRef<Path>,
    ) -> Result<InstalledDriverRelease, DriverInstallationError> {
        validate_installation_root(&self.root)?;
        release.bind_to_manifest(manifest)?;
        let distribution = release
            .distribution_for(host)
            .ok_or(DriverInstallationError::UnsupportedHost(host))?;
        let release_digest = release.canonical_sha256()?;
        let final_root = self
            .root
            .join(format!("{}-{host}", release_digest.as_str()));

        let lock = open_install_lock(&self.root)?;
        lock.lock().map_err(|error| {
            DriverInstallationError::io("lock provider driver installer", error)
        })?;
        cleanup_staging_directories(&self.root)?;

        if path_exists_without_following(&final_root)? {
            verify_install_tree(&final_root, distribution)?;
            return Ok(InstalledDriverRelease {
                release_digest,
                host_operating_system: host,
                root: final_root,
                distribution: distribution.clone(),
            });
        }

        let mut archive = open_and_verify_archive(archive_path.as_ref(), distribution)?;
        let staging = TempDirBuilder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&self.root)
            .map_err(|error| {
                DriverInstallationError::io("create driver staging directory", error)
            })?;
        extract_archive(&mut archive, staging.path(), distribution)?;
        make_tree_read_only(staging.path(), distribution)?;
        verify_install_tree(staging.path(), distribution)?;

        if path_exists_without_following(&final_root)? {
            verify_install_tree(&final_root, distribution)?;
        } else {
            fs::rename(staging.path(), &final_root).map_err(|error| {
                DriverInstallationError::io("publish driver installation", error)
            })?;
            sync_directory(&self.root)?;
        }
        verify_install_tree(&final_root, distribution)?;
        Ok(InstalledDriverRelease {
            release_digest,
            host_operating_system: host,
            root: final_root,
            distribution: distribution.clone(),
        })
    }

    /// Resolve an already-published release for this host and re-verify the
    /// complete tree. This never creates, repairs, or replaces an install.
    pub fn resolve(
        &self,
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
        host: HostOperatingSystem,
    ) -> Result<InstalledDriverRelease, DriverInstallationError> {
        validate_installation_root(&self.root)?;
        release.bind_to_manifest(manifest)?;
        let distribution = release
            .distribution_for(host)
            .ok_or(DriverInstallationError::UnsupportedHost(host))?;
        let release_digest = release.canonical_sha256()?;
        let final_root = self
            .root
            .join(format!("{}-{host}", release_digest.as_str()));
        verify_install_tree(&final_root, distribution)?;
        Ok(InstalledDriverRelease {
            release_digest,
            host_operating_system: host,
            root: final_root,
            distribution: distribution.clone(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl InstalledDriverRelease {
    pub fn verify(&self) -> Result<(), DriverInstallationError> {
        verify_install_tree(&self.root, &self.distribution)
    }

    pub fn executables(&self) -> Result<Vec<InstalledReleaseExecutable>, DriverInstallationError> {
        self.verify()?;
        Ok(self
            .distribution
            .components
            .iter()
            .filter_map(|component| {
                component
                    .executable
                    .as_ref()
                    .map(|entrypoint| InstalledReleaseExecutable {
                        role: component.role,
                        name: component.name.clone(),
                        executable: PinnedExecutable::new(
                            self.root.join(&entrypoint.path),
                            component.name.clone(),
                            component.version.clone(),
                            entrypoint.sha256.value.clone(),
                        ),
                        argument_prefix: entrypoint.argument_prefix.clone(),
                    })
            })
            .collect())
    }

    #[must_use]
    pub fn distribution(&self) -> &DriverHostDistribution {
        &self.distribution
    }
}

#[derive(Debug)]
pub enum DriverInstallationError {
    Release(DriverReleaseError),
    UnsupportedHost(HostOperatingSystem),
    Invalid(&'static str),
    Io {
        context: &'static str,
        source: io::Error,
    },
}

impl DriverInstallationError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }
}

impl fmt::Display for DriverInstallationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release(error) => write!(formatter, "{error}"),
            Self::UnsupportedHost(host) => {
                write!(
                    formatter,
                    "provider driver release has no {host} distribution"
                )
            }
            Self::Invalid(message) => formatter.write_str(message),
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
        }
    }
}

impl std::error::Error for DriverInstallationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Release(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::UnsupportedHost(_) | Self::Invalid(_) => None,
        }
    }
}

impl From<DriverReleaseError> for DriverInstallationError {
    fn from(value: DriverReleaseError) -> Self {
        Self::Release(value)
    }
}

fn open_install_lock(root: &Path) -> Result<File, DriverInstallationError> {
    let path = root.join(INSTALL_LOCK_FILE);
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                DriverInstallationError::io("inspect driver installer lock", error)
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(DriverInstallationError::Invalid(
                    "provider driver installer lock is not a regular file",
                ));
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|error| DriverInstallationError::io("open driver installer lock", error))
        }
        Err(error) => Err(DriverInstallationError::io(
            "create driver installer lock",
            error,
        )),
    }
}

fn path_exists_without_following(path: &Path) -> Result<bool, DriverInstallationError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DriverInstallationError::io(
            "inspect driver installation path",
            error,
        )),
    }
}

fn validate_installation_root(root: &Path) -> Result<(), DriverInstallationError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| DriverInstallationError::io("inspect installation root", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DriverInstallationError::Invalid(
            "provider driver installation root must be a real directory",
        ));
    }
    validate_installation_root_permissions(root, &metadata)
}

fn cleanup_staging_directories(root: &Path) -> Result<(), DriverInstallationError> {
    for entry in fs::read_dir(root)
        .map_err(|error| DriverInstallationError::io("read driver installation root", error))?
    {
        let entry = entry.map_err(|error| {
            DriverInstallationError::io("read driver installation entry", error)
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(DriverInstallationError::Invalid(
                "provider driver installation root contains a non-UTF-8 entry",
            ));
        };
        if !name.starts_with(STAGING_PREFIX) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| DriverInstallationError::io("inspect stale driver staging", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DriverInstallationError::Invalid(
                "provider driver staging entry is not a real directory",
            ));
        }
        prepare_staging_for_removal(&path)?;
        fs::remove_dir_all(&path)
            .map_err(|error| DriverInstallationError::io("remove stale driver staging", error))?;
    }
    Ok(())
}

fn prepare_staging_for_removal(root: &Path) -> Result<(), DriverInstallationError> {
    let mut directories = vec![root.to_path_buf()];
    let mut index = 0;
    while index < directories.len() {
        let directory = directories[index].clone();
        set_removal_permissions(&directory, true)?;
        for entry in fs::read_dir(&directory)
            .map_err(|error| DriverInstallationError::io("read stale driver staging", error))?
        {
            let path = entry
                .map_err(|error| DriverInstallationError::io("read stale driver entry", error))?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                DriverInstallationError::io("inspect stale driver entry", error)
            })?;
            if metadata.file_type().is_symlink() {
                return Err(DriverInstallationError::Invalid(
                    "stale provider driver staging contains a symlink",
                ));
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                set_removal_permissions(&path, false)?;
            } else {
                return Err(DriverInstallationError::Invalid(
                    "stale provider driver staging contains a non-regular entry",
                ));
            }
        }
        index += 1;
    }
    Ok(())
}

#[cfg(unix)]
fn set_removal_permissions(path: &Path, directory: bool) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )
    .map_err(|error| DriverInstallationError::io("make stale driver staging removable", error))
}

#[cfg(windows)]
fn set_removal_permissions(path: &Path, _directory: bool) -> Result<(), DriverInstallationError> {
    set_windows_writable(path)
        .map_err(|error| DriverInstallationError::io("make stale driver staging removable", error))
}

#[cfg(not(any(unix, windows)))]
fn set_removal_permissions(path: &Path, _directory: bool) -> Result<(), DriverInstallationError> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| DriverInstallationError::io("inspect stale driver permissions", error))?
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
        .map_err(|error| DriverInstallationError::io("make stale driver staging removable", error))
}

#[cfg(windows)]
fn set_windows_writable(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_READONLY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };

    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows provider driver path contains NUL",
        ));
    }
    encoded.push(0);

    // SAFETY: `encoded` is NUL-terminated and remains alive for the call.
    let attributes = unsafe { GetFileAttributesW(encoded.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    if attributes & FILE_ATTRIBUTE_READONLY == 0 {
        return Ok(());
    }
    // SAFETY: the path pointer is valid as above, and the attributes came from
    // the same path with only FILE_ATTRIBUTE_READONLY cleared.
    if unsafe { SetFileAttributesW(encoded.as_ptr(), attributes & !FILE_ATTRIBUTE_READONLY) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn validate_installation_root_permissions(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid has no arguments and only reads the process credential.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(DriverInstallationError::Invalid(
            "provider driver installation root must be owned by this service and not writable by group or others",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_installation_root_permissions(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Result<(), DriverInstallationError> {
    crate::windows_security::open_private_directory(path)
        .map(drop)
        .map_err(installation_security_error)
}

#[cfg(not(any(unix, windows)))]
fn validate_installation_root_permissions(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Result<(), DriverInstallationError> {
    Ok(())
}

#[cfg(windows)]
fn installation_security_error(
    error: crate::windows_security::WindowsSecurityError,
) -> DriverInstallationError {
    match error {
        crate::windows_security::WindowsSecurityError::Io(error) => {
            DriverInstallationError::io("validate Windows installation-root security", error)
        }
        crate::windows_security::WindowsSecurityError::Insecure(message) => {
            DriverInstallationError::Invalid(message)
        }
    }
}

fn open_and_verify_archive(
    path: &Path,
    distribution: &DriverHostDistribution,
) -> Result<File, DriverInstallationError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| DriverInstallationError::io("inspect driver archive", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DriverInstallationError::Invalid(
            "provider driver archive must be a regular non-symlink file",
        ));
    }
    let mut archive = File::open(path)
        .map_err(|error| DriverInstallationError::io("open driver archive", error))?;
    let opened_metadata = archive
        .metadata()
        .map_err(|error| DriverInstallationError::io("inspect opened driver archive", error))?;
    if !opened_metadata.is_file() || opened_metadata.len() != distribution.archive.size_bytes {
        return Err(DriverInstallationError::Invalid(
            "provider driver archive size differs from the release index",
        ));
    }
    let actual = hash_reader(&mut archive, distribution.archive.size_bytes)?;
    if actual != distribution.archive.sha256.value {
        return Err(DriverInstallationError::Invalid(
            "provider driver archive digest differs from the release index",
        ));
    }
    archive
        .seek(SeekFrom::Start(0))
        .map_err(|error| DriverInstallationError::io("rewind driver archive", error))?;
    Ok(archive)
}

fn hash_reader(
    reader: &mut File,
    expected_bytes: u64,
) -> Result<Sha256Digest, DriverInstallationError> {
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| DriverInstallationError::io("read driver archive", error))?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(DriverInstallationError::Invalid(
                "provider driver archive size overflowed",
            ))?;
        if bytes > expected_bytes {
            return Err(DriverInstallationError::Invalid(
                "provider driver archive grew while it was read",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    if bytes != expected_bytes {
        return Err(DriverInstallationError::Invalid(
            "provider driver archive changed while it was read",
        ));
    }
    Ok(hex::encode(hasher.finalize())
        .parse()
        .expect("hex encoding a SHA-256 always produces a canonical digest"))
}

fn extract_archive(
    archive: &mut File,
    staging: &Path,
    distribution: &DriverHostDistribution,
) -> Result<(), DriverInstallationError> {
    let mut expected = distribution
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut tar = tar::Archive::new(archive);
    let entries = tar
        .entries()
        .map_err(|error| DriverInstallationError::io("read driver archive entries", error))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|error| DriverInstallationError::io("read driver archive entry", error))?;
        if !entry.header().entry_type().is_file() {
            return Err(DriverInstallationError::Invalid(
                "provider driver archive contains a link or non-regular entry",
            ));
        }
        let path_bytes = entry.path_bytes();
        let relative = std::str::from_utf8(&path_bytes).map_err(|_| {
            DriverInstallationError::Invalid("provider driver archive path is not UTF-8")
        })?;
        let Some(file) = expected.remove(relative) else {
            return Err(DriverInstallationError::Invalid(
                "provider driver archive contains an extra or duplicate file",
            ));
        };
        if entry.size() != file.size_bytes {
            return Err(DriverInstallationError::Invalid(
                "provider driver archive member size differs from the file table",
            ));
        }
        let destination = staging.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                DriverInstallationError::io("create driver installation directory", error)
            })?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| {
                DriverInstallationError::io("create driver installation file", error)
            })?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = entry.read(&mut buffer).map_err(|error| {
                DriverInstallationError::io("read driver archive member", error)
            })?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .ok_or(DriverInstallationError::Invalid(
                    "provider driver archive member size overflowed",
                ))?;
            if copied > file.size_bytes {
                return Err(DriverInstallationError::Invalid(
                    "provider driver archive member exceeds the file table size",
                ));
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count]).map_err(|error| {
                DriverInstallationError::io("write driver installation file", error)
            })?;
        }
        if copied != file.size_bytes || hex::encode(hasher.finalize()) != file.sha256.value.as_str()
        {
            return Err(DriverInstallationError::Invalid(
                "provider driver archive member digest differs from the file table",
            ));
        }
        output
            .sync_all()
            .map_err(|error| DriverInstallationError::io("sync driver installation file", error))?;
    }
    if !expected.is_empty() {
        return Err(DriverInstallationError::Invalid(
            "provider driver archive omits a release-index file",
        ));
    }
    Ok(())
}

fn verify_install_tree(
    root: &Path,
    distribution: &DriverHostDistribution,
) -> Result<(), DriverInstallationError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| DriverInstallationError::io("inspect installed driver root", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver root is not a real directory",
        ));
    }
    directory_is_read_only(root, &metadata)?;
    let mut actual_paths = Vec::new();
    collect_tree_paths(root, root, &mut actual_paths)?;
    actual_paths.sort();
    let expected_paths = distribution
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    if actual_paths != expected_paths {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver tree has missing or extra files",
        ));
    }
    for file in &distribution.files {
        let path = root.join(&file.path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| DriverInstallationError::io("inspect installed driver file", error))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != file.size_bytes
        {
            return Err(DriverInstallationError::Invalid(
                "installed provider driver file metadata differs from the release index",
            ));
        }
        file_is_read_only_and_executable(&path, &metadata, file.executable)?;
        let mut input = File::open(&path)
            .map_err(|error| DriverInstallationError::io("open installed driver file", error))?;
        if hash_reader(&mut input, file.size_bytes)? != file.sha256.value {
            return Err(DriverInstallationError::Invalid(
                "installed provider driver file digest differs from the release index",
            ));
        }
    }
    Ok(())
}

fn collect_tree_paths(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
) -> Result<(), DriverInstallationError> {
    for entry in fs::read_dir(directory)
        .map_err(|error| DriverInstallationError::io("read installed driver directory", error))?
    {
        let entry = entry
            .map_err(|error| DriverInstallationError::io("read installed driver entry", error))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            DriverInstallationError::io("inspect installed driver entry", error)
        })?;
        if metadata.file_type().is_symlink() {
            return Err(DriverInstallationError::Invalid(
                "installed provider driver tree contains a symlink",
            ));
        }
        if metadata.is_dir() {
            directory_is_read_only(&path, &metadata)?;
            collect_tree_paths(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                DriverInstallationError::Invalid("installed driver path escaped its root")
            })?;
            files.push(path_to_portable_string(relative)?);
        } else {
            return Err(DriverInstallationError::Invalid(
                "installed provider driver tree contains a non-regular entry",
            ));
        }
    }
    Ok(())
}

fn path_to_portable_string(path: &Path) -> Result<String, DriverInstallationError> {
    let mut value = String::new();
    for component in path.components() {
        let component = component
            .as_os_str()
            .to_str()
            .ok_or(DriverInstallationError::Invalid(
                "installed provider driver path is not UTF-8",
            ))?;
        if !value.is_empty() {
            value.push('/');
        }
        value.push_str(component);
    }
    Ok(value)
}

fn make_tree_read_only(
    root: &Path,
    distribution: &DriverHostDistribution,
) -> Result<(), DriverInstallationError> {
    for file in &distribution.files {
        set_file_read_only(&root.join(&file.path), file.executable)?;
    }
    let mut directories = BTreeSet::new();
    directories.insert(root.to_path_buf());
    for entry in walk_directories(root)? {
        directories.insert(entry);
    }
    for directory in directories.iter().rev() {
        set_directory_read_only(directory)?;
    }
    Ok(())
}

fn walk_directories(root: &Path) -> Result<Vec<PathBuf>, DriverInstallationError> {
    let mut pending = vec![root.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| DriverInstallationError::io("read driver staging directory", error))?
        {
            let path = entry
                .map_err(|error| DriverInstallationError::io("read driver staging entry", error))?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                DriverInstallationError::io("inspect driver staging entry", error)
            })?;
            if metadata.file_type().is_symlink() {
                return Err(DriverInstallationError::Invalid(
                    "provider driver staging tree contains a symlink",
                ));
            }
            if metadata.is_dir() {
                directories.push(path.clone());
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(DriverInstallationError::Invalid(
                    "provider driver staging tree contains a non-regular entry",
                ));
            }
        }
    }
    Ok(directories)
}

#[cfg(unix)]
fn set_file_read_only(path: &Path, executable: bool) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
    )
    .map_err(|error| DriverInstallationError::io("set driver file permissions", error))
}

#[cfg(not(unix))]
fn set_file_read_only(path: &Path, _executable: bool) -> Result<(), DriverInstallationError> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| DriverInstallationError::io("inspect driver file permissions", error))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|error| DriverInstallationError::io("set driver file permissions", error))
}

#[cfg(unix)]
fn set_directory_read_only(path: &Path) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o555))
        .map_err(|error| DriverInstallationError::io("set driver directory permissions", error))
}

#[cfg(not(unix))]
fn set_directory_read_only(path: &Path) -> Result<(), DriverInstallationError> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| {
            DriverInstallationError::io("inspect driver directory permissions", error)
        })?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|error| DriverInstallationError::io("set driver directory permissions", error))
}

#[cfg(unix)]
fn file_is_read_only_and_executable(
    _path: &Path,
    metadata: &fs::Metadata,
    executable: bool,
) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = metadata.permissions().mode();
    if mode & 0o222 != 0 || (mode & 0o111 != 0) != executable {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver file permissions differ from the release index",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn file_is_read_only_and_executable(
    _path: &Path,
    metadata: &fs::Metadata,
    _executable: bool,
) -> Result<(), DriverInstallationError> {
    if !metadata.permissions().readonly() {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver file is writable",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn directory_is_read_only(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), DriverInstallationError> {
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o222 != 0 {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver directory is writable",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn directory_is_read_only(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), DriverInstallationError> {
    if !metadata.permissions().readonly() {
        return Err(DriverInstallationError::Invalid(
            "installed provider driver directory is writable",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), DriverInstallationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| DriverInstallationError::io("sync driver installation directory", error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), DriverInstallationError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    use crate::{
        DriverReleaseComponent, DriverReleaseFile, ProviderDriverManifest, ProviderDriverRelease,
    };
    use serde::Serialize;
    use tempfile::TempDir;

    const RELEASE_FIXTURE: &[u8] =
        include_bytes!("../../../contracts/v1/fixtures/valid/provider-driver-release.json");
    const MANIFEST_FIXTURE: &[u8] =
        include_bytes!("../../../contracts/v1/fixtures/valid/provider-driver.json");

    #[derive(Clone, Copy)]
    enum ExtraArchiveEntry {
        None,
        Regular,
        Symlink,
    }

    struct Fixture {
        temporary: TempDir,
        install_root: PathBuf,
        archive: PathBuf,
        host: HostOperatingSystem,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
    }

    impl Fixture {
        fn new(extra: ExtraArchiveEntry) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let install_root = temporary.path().join("installed");
            fs::create_dir(&install_root).unwrap();
            make_test_install_root_private(&install_root);
            let archive = temporary.path().join("driver.tar");
            let host = HostOperatingSystem::current().unwrap_or(HostOperatingSystem::LinuxAmd64);

            let template: ProviderDriverRelease = serde_json::from_slice(RELEASE_FIXTURE).unwrap();
            let mut distribution = template
                .distributions
                .iter()
                .find(|distribution| distribution.host_operating_system == host)
                .unwrap_or(&template.distributions[0])
                .clone();
            distribution.host_operating_system = host;
            let contents = distribution
                .files
                .iter()
                .map(|file| {
                    let byte = if file.path.contains("driver") {
                        b'd'
                    } else {
                        b'w'
                    };
                    (
                        file.path.clone(),
                        vec![byte; if byte == b'd' { 128 } else { 256 }],
                    )
                })
                .collect::<BTreeMap<_, _>>();
            bind_file_table(&mut distribution.files, &contents);
            bind_components(&mut distribution.components, &distribution.files);
            distribution.tree_digest.value = digest_jcs(&distribution.files);
            write_archive(&archive, &contents, extra);
            let archive_bytes = fs::read(&archive).unwrap();
            distribution.archive.size_bytes = archive_bytes.len() as u64;
            distribution.archive.sha256.value = digest_bytes(&archive_bytes);

            let mut release = template;
            release.distributions = vec![distribution];
            let release = ValidatedProviderDriverRelease::new(release).unwrap();
            let mut manifest: ProviderDriverManifest =
                serde_json::from_slice(MANIFEST_FIXTURE).unwrap();
            manifest.supported_host_operating_systems = vec![host.as_str().to_owned()];
            manifest.driver.distribution_digest.value = release.canonical_sha256().unwrap();
            let manifest = ValidatedProviderDriverManifest::new(manifest).unwrap();

            Self {
                temporary,
                install_root,
                archive,
                host,
                release,
                manifest,
            }
        }

        fn installer(&self) -> DriverReleaseInstaller {
            DriverReleaseInstaller::new(&self.install_root).unwrap()
        }

        fn install(&self) -> Result<InstalledDriverRelease, DriverInstallationError> {
            self.installer()
                .install(&self.release, &self.manifest, self.host, &self.archive)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            make_writable_for_cleanup(self.temporary.path());
        }
    }

    fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
        hex::encode(Sha256::digest(bytes)).parse().unwrap()
    }

    fn digest_jcs(value: &impl Serialize) -> Sha256Digest {
        digest_bytes(&serde_jcs::to_vec(value).unwrap())
    }

    fn bind_file_table(files: &mut [DriverReleaseFile], contents: &BTreeMap<String, Vec<u8>>) {
        for file in files {
            let content = &contents[&file.path];
            file.size_bytes = content.len() as u64;
            file.sha256.value = digest_bytes(content);
        }
    }

    fn bind_components(components: &mut [DriverReleaseComponent], files: &[DriverReleaseFile]) {
        for component in components {
            let paths = component.file_paths.as_ref().unwrap();
            let component_files = paths
                .iter()
                .map(|path| files.iter().find(|file| &file.path == path).unwrap())
                .collect::<Vec<_>>();
            component.materialization_digest.value = digest_jcs(&component_files);
            let executable = component.executable.as_mut().unwrap();
            executable.sha256 = files
                .iter()
                .find(|file| file.path == executable.path)
                .unwrap()
                .sha256
                .clone();
        }
    }

    fn write_archive(path: &Path, contents: &BTreeMap<String, Vec<u8>>, extra: ExtraArchiveEntry) {
        let file = File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        for (path, content) in contents {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_slice())
                .unwrap();
        }
        match extra {
            ExtraArchiveEntry::None => {}
            ExtraArchiveEntry::Regular => {
                let content = b"not in the release index";
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(content.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, "extra", &content[..])
                    .unwrap();
            }
            ExtraArchiveEntry::Symlink => {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_link_name("bin/driver").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, "extra-link", io::empty())
                    .unwrap();
            }
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap().sync_all().unwrap();
    }

    fn make_writable_for_cleanup(root: &Path) {
        let Ok(metadata) = fs::symlink_metadata(root) else {
            return;
        };
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && let Ok(entries) = fs::read_dir(root)
        {
            for entry in entries.flatten() {
                make_writable_for_cleanup(&entry.path());
            }
        }
        set_cleanup_permissions(root, metadata.is_dir());
    }

    #[cfg(unix)]
    fn make_test_install_root_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(windows)]
    fn make_test_install_root_private(path: &Path) {
        crate::windows_security::set_private_acl_for_test(path, true, false, true).unwrap();
    }

    #[cfg(not(any(unix, windows)))]
    fn make_test_install_root_private(_path: &Path) {}

    #[cfg(unix)]
    fn set_cleanup_permissions(path: &Path, directory: bool) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        );
    }

    #[cfg(windows)]
    fn set_cleanup_permissions(path: &Path, _directory: bool) {
        let _ = set_windows_writable(path);
    }

    #[cfg(not(any(unix, windows)))]
    fn set_cleanup_permissions(path: &Path, _directory: bool) {
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }

    #[cfg(unix)]
    fn make_test_file_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn make_test_file_writable(path: &Path) {
        set_windows_writable(path).unwrap();
    }

    #[cfg(not(any(unix, windows)))]
    fn make_test_file_writable(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn installs_and_reuses_one_content_addressed_read_only_tree() {
        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let first = fixture.install().unwrap();
        first.verify().unwrap();
        let executables = first.executables().unwrap();
        assert_eq!(executables.len(), 2);
        assert!(
            executables
                .iter()
                .all(|entry| entry.executable.path.is_absolute())
        );

        let second = fixture.install().unwrap();
        assert_eq!(first.root, second.root);
        let resolved = fixture
            .installer()
            .resolve(&fixture.release, &fixture.manifest, fixture.host)
            .unwrap();
        assert_eq!(resolved.root, first.root);
        resolved.verify().unwrap();
        assert_eq!(
            first.release_digest,
            fixture.manifest.get().driver.distribution_digest.value
        );
    }

    #[test]
    fn refuses_archive_digest_tampering_before_publication() {
        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let mut archive = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.archive)
            .unwrap();
        let mut byte = [0_u8; 1];
        archive.read_exact(&mut byte).unwrap();
        archive.seek(SeekFrom::Start(0)).unwrap();
        archive.write_all(&[byte[0] ^ 0xff]).unwrap();
        archive.sync_all().unwrap();

        assert!(fixture.install().is_err());
        assert_eq!(fs::read_dir(&fixture.install_root).unwrap().count(), 1);
    }

    #[test]
    fn refuses_extra_regular_files_and_links_even_when_the_archive_digest_matches() {
        for extra in [ExtraArchiveEntry::Regular, ExtraArchiveEntry::Symlink] {
            let fixture = Fixture::new(extra);
            assert!(fixture.install().is_err());
            assert_eq!(fs::read_dir(&fixture.install_root).unwrap().count(), 1);
        }
    }

    #[test]
    fn refuses_a_host_that_the_release_does_not_contain() {
        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let wrong_host = match fixture.host {
            HostOperatingSystem::WindowsAmd64 => HostOperatingSystem::LinuxAmd64,
            _ => HostOperatingSystem::WindowsAmd64,
        };
        assert!(matches!(
            fixture.installer().install(
                &fixture.release,
                &fixture.manifest,
                wrong_host,
                &fixture.archive,
            ),
            Err(DriverInstallationError::UnsupportedHost(host)) if host == wrong_host
        ));
    }

    #[test]
    fn removes_only_owned_stale_staging_before_installing() {
        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let stale = fixture
            .install_root
            .join(format!("{STAGING_PREFIX}crashed"));
        fs::create_dir(&stale).unwrap();
        let stale_file = stale.join("partial");
        fs::write(&stale_file, b"partial").unwrap();
        set_file_read_only(&stale_file, false).unwrap();
        set_directory_read_only(&stale).unwrap();

        fixture.install().unwrap();
        assert!(!stale.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_group_writable_installation_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("unsafe-root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            DriverReleaseInstaller::new(&root),
            Err(DriverInstallationError::Invalid(message))
                if message.contains("not writable by group or others")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn revalidates_installation_root_permissions_for_every_install() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let installer = fixture.installer();
        fs::set_permissions(&fixture.install_root, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(
            installer
                .install(
                    &fixture.release,
                    &fixture.manifest,
                    fixture.host,
                    &fixture.archive,
                )
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn refuses_unprotected_or_untrusted_windows_installation_roots_and_pins_the_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("windows-private-root");
        fs::create_dir(&root).unwrap();

        crate::windows_security::set_private_acl_for_test(&root, true, false, false).unwrap();
        assert!(matches!(
            DriverReleaseInstaller::new(&root),
            Err(DriverInstallationError::Invalid(message)) if message.contains("protected")
        ));

        crate::windows_security::set_private_acl_for_test(&root, true, true, true).unwrap();
        assert!(matches!(
            DriverReleaseInstaller::new(&root),
            Err(DriverInstallationError::Invalid(message)) if message.contains("untrusted")
        ));

        crate::windows_security::set_private_acl_for_test(&root, true, false, true).unwrap();
        let installer = DriverReleaseInstaller::new(&root).unwrap();
        let moved = temporary.path().join("moved-root");
        assert!(fs::rename(&root, &moved).is_err());
        drop(installer);
        fs::rename(&root, &moved).unwrap();
    }

    #[test]
    fn an_existing_installation_is_verified_and_never_repaired_in_place() {
        let fixture = Fixture::new(ExtraArchiveEntry::None);
        let installed = fixture.install().unwrap();
        let file = installed
            .distribution
            .files
            .first()
            .map(|file| installed.root.join(&file.path))
            .unwrap();
        make_test_file_writable(&file);
        let mut output = OpenOptions::new().write(true).open(&file).unwrap();
        output.write_all(b"X").unwrap();
        output.sync_all().unwrap();

        assert!(installed.verify().is_err());
        let before = fs::read(&file).unwrap();
        assert!(fixture.install().is_err());
        assert_eq!(fs::read(&file).unwrap(), before);
    }
}
