//! Windows ownership and DACL checks for provider-driver trust material.
//!
//! The read-only file attribute is not an authorization boundary on Windows.
//! These checks require the current Control service SID to own the object and
//! reject every DACL grant that lets another principal mutate, replace,
//! delete, or take control of it. LocalSystem and the built-in Administrators
//! group remain trusted administrative principals.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _},
    path::Path,
    ptr::{copy_nonoverlapping, null_mut},
    slice,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_ALL, GENERIC_WRITE, HANDLE, HLOCAL,
        LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACL,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, GetTokenInformation, IsValidAcl, IsValidSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    },
    System::{
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
            ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_COMPOUND_ACE_TYPE,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

const DANGEROUS_FILE_RIGHTS: u32 = GENERIC_ALL
    | GENERIC_WRITE
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | FILE_DELETE_CHILD;

#[derive(Debug)]
pub(crate) enum WindowsSecurityError {
    Io(io::Error),
    Insecure(&'static str),
}

impl WindowsSecurityError {
    fn last_os_error() -> Self {
        Self::Io(io::Error::last_os_error())
    }

    fn status(status: u32) -> Self {
        Self::Io(io::Error::from_raw_os_error(status as i32))
    }
}

pub(crate) fn open_private_directory(path: &Path) -> Result<File, WindowsSecurityError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path).map_err(WindowsSecurityError::Io)?;
    let metadata = directory.metadata().map_err(WindowsSecurityError::Io)?;
    use std::os::windows::fs::MetadataExt as _;
    if !metadata.is_dir()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(WindowsSecurityError::Insecure(
            "provider driver installation root must be a non-reparse directory",
        ));
    }
    validate_private_handle(&directory)?;
    Ok(directory)
}

pub(crate) fn validate_private_file(file: &File) -> Result<(), WindowsSecurityError> {
    validate_private_handle(file)
}

fn validate_private_handle(file: &File) -> Result<(), WindowsSecurityError> {
    let service_sid = process_user_sid()?;
    let system_sid = well_known_sid(WinLocalSystemSid)?;
    let administrators_sid = well_known_sid(WinBuiltinAdministratorsSid)?;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the file handle is live for the call and every out pointer is
    // valid. The returned descriptor owns the owner and DACL pointers.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(WindowsSecurityError::status(status));
    }
    let _descriptor = LocalDescriptor(descriptor);
    if descriptor.is_null() || owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver security descriptor has no valid owner",
        ));
    }
    if !sid_eq(owner, service_sid.as_ptr()) {
        return Err(WindowsSecurityError::Insecure(
            "provider driver security object must be owned by the current service identity",
        ));
    }
    if dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver security object requires a valid non-null DACL",
        ));
    }
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: `descriptor` remains owned by `_descriptor` and the output
    // pointers reference initialized stack values.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(WindowsSecurityError::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver security object DACL must be protected from parent inheritance",
        ));
    }

    let ace_count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(ace_count) {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `dacl` passed `IsValidAcl`; `index` is below its ACE count;
        // `ace` is a valid output pointer.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(WindowsSecurityError::last_os_error());
        }
        validate_ace(ace, &service_sid, &system_sid, &administrators_sid)?;
    }
    Ok(())
}

fn validate_ace(
    ace: *mut c_void,
    service_sid: &OwnedSid,
    system_sid: &OwnedSid,
    administrators_sid: &OwnedSid,
) -> Result<(), WindowsSecurityError> {
    if ace.is_null() {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL contains a null ACE",
        ));
    }
    // SAFETY: `ace` came from `GetAce` on a valid ACL and begins with an
    // `ACE_HEADER` for at least its declared size.
    let header = unsafe { &*(ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
    let ace_size = usize::from(header.AceSize);
    if ace_size < 8 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL contains a truncated ACE",
        ));
    }
    let allow_types = [
        ACCESS_ALLOWED_ACE_TYPE as u8,
        ACCESS_ALLOWED_COMPOUND_ACE_TYPE as u8,
        ACCESS_ALLOWED_OBJECT_ACE_TYPE as u8,
        ACCESS_ALLOWED_CALLBACK_ACE_TYPE as u8,
        ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE as u8,
    ];
    if !allow_types.contains(&header.AceType) {
        return Ok(());
    }
    // Every access-allowed ACE family starts with ACE_HEADER then ACCESS_MASK.
    // SAFETY: the eight-byte minimum above covers that common prefix.
    let bytes = unsafe { slice::from_raw_parts(ace.cast::<u8>(), ace_size) };
    let mask = u32::from_ne_bytes(bytes[4..8].try_into().expect("four-byte access mask"));
    if mask & DANGEROUS_FILE_RIGHTS == 0 {
        return Ok(());
    }
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL contains an unsupported conditional or object-specific write grant",
        ));
    }
    let sid_offset = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    if ace_size < sid_offset + 8 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL contains a truncated writer SID",
        ));
    }
    // SAFETY: the standard ACCESS_ALLOWED_ACE SID begins at `SidStart`; the
    // minimum SID header fits inside the checked ACE bounds.
    let sid = unsafe { ace.cast::<u8>().add(sid_offset).cast::<c_void>() };
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL contains an invalid writer SID",
        ));
    }
    let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
    if sid_length == 0 || sid_offset.saturating_add(sid_length) > ace_size {
        return Err(WindowsSecurityError::Insecure(
            "provider driver DACL writer SID exceeds its ACE",
        ));
    }
    if sid_eq(sid, service_sid.as_ptr())
        || sid_eq(sid, system_sid.as_ptr())
        || sid_eq(sid, administrators_sid.as_ptr())
    {
        return Ok(());
    }
    Err(WindowsSecurityError::Insecure(
        "provider driver DACL grants write or security-control rights to an untrusted principal",
    ))
}

fn process_user_sid() -> Result<OwnedSid, WindowsSecurityError> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `GetCurrentProcess` returns the current pseudo-handle and token
    // is a valid output pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(WindowsSecurityError::last_os_error());
    }
    let _token = TokenHandle(token);
    let mut required = 0_u32;
    // SAFETY: a null zero-sized first call obtains the required buffer size.
    let first = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required) };
    if first != 0
        || required < size_of::<TOKEN_USER>() as u32
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        return Err(WindowsSecurityError::last_os_error());
    }
    let words = usize::try_from(required)
        .map_err(|_| WindowsSecurityError::Insecure("Windows token size exceeds this process"))?
        .div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let bytes = buffer
        .len()
        .checked_mul(size_of::<usize>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(WindowsSecurityError::Insecure(
            "Windows token buffer size exceeds its API range",
        ))?;
    // SAFETY: the word buffer is aligned for TOKEN_USER and has `bytes`
    // writable bytes; the output length pointer is valid.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut required,
        )
    } == 0
    {
        return Err(WindowsSecurityError::last_os_error());
    }
    // SAFETY: the successful API call initialized a TOKEN_USER at the start of
    // the suitably aligned buffer.
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    OwnedSid::copy_from(user.User.Sid)
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<OwnedSid, WindowsSecurityError> {
    let mut words = vec![0_usize; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<usize>())];
    let mut bytes = SECURITY_MAX_SID_SIZE;
    // SAFETY: the aligned output buffer has SECURITY_MAX_SID_SIZE bytes and
    // the size pointer is valid. These machine-wide SIDs need no domain SID.
    if unsafe { CreateWellKnownSid(kind, null_mut(), words.as_mut_ptr().cast(), &mut bytes) } == 0 {
        return Err(WindowsSecurityError::last_os_error());
    }
    OwnedSid::copy_from(words.as_mut_ptr().cast())
}

fn sid_eq(left: PSID, right: PSID) -> bool {
    // SAFETY: all SIDs passed here were validated or produced by Windows.
    unsafe { EqualSid(left, right) != 0 }
}

struct OwnedSid {
    words: Vec<usize>,
}

impl OwnedSid {
    fn copy_from(sid: PSID) -> Result<Self, WindowsSecurityError> {
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(WindowsSecurityError::Insecure(
                "Windows security identity is invalid",
            ));
        }
        // SAFETY: the SID was validated immediately above.
        let bytes = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
        if bytes < 8 || bytes > SECURITY_MAX_SID_SIZE as usize {
            return Err(WindowsSecurityError::Insecure(
                "Windows security identity has an invalid size",
            ));
        }
        let mut words = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: both buffers have at least `bytes` valid bytes and do not
        // overlap. The aligned destination remains owned by this value.
        unsafe { copy_nonoverlapping(sid.cast::<u8>(), words.as_mut_ptr().cast(), bytes) };
        Ok(Self { words })
    }

    fn as_ptr(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }
}

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this handle came from a successful OpenProcessToken call
            // and is closed exactly once here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc
            // and transfers ownership to the caller.
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_private_acl_for_test(
    path: &Path,
    directory: bool,
    add_untrusted_writer: bool,
    protected: bool,
) -> Result<(), WindowsSecurityError> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Security::{
        ACL_REVISION, AddAccessAllowedAceEx, Authorization::SetNamedSecurityInfoW,
        CONTAINER_INHERIT_ACE, InitializeAcl, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_DACL_SECURITY_INFORMATION, WinWorldSid,
    };

    let mut sids = vec![
        process_user_sid()?,
        well_known_sid(WinLocalSystemSid)?,
        well_known_sid(WinBuiltinAdministratorsSid)?,
    ];
    if add_untrusted_writer {
        sids.push(well_known_sid(WinWorldSid)?);
    }
    let acl_bytes = size_of::<ACL>()
        + sids
            .iter()
            .map(|sid| {
                size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
                    + unsafe { windows_sys::Win32::Security::GetLengthSid(sid.as_ptr()) } as usize
            })
            .sum::<usize>();
    let mut acl_words = vec![0_usize; acl_bytes.div_ceil(size_of::<usize>())];
    let acl_size = acl_words
        .len()
        .checked_mul(size_of::<usize>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(WindowsSecurityError::Insecure(
            "test DACL size exceeds the Windows API range",
        ))?;
    let acl = acl_words.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` points to an aligned writable buffer of `acl_size` bytes.
    if unsafe { InitializeAcl(acl, acl_size, ACL_REVISION) } == 0 {
        return Err(WindowsSecurityError::last_os_error());
    }
    let inheritance = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    for sid in &sids {
        // SAFETY: the ACL buffer has the exact sum of all ACE sizes and every
        // SID is valid for the duration of the call.
        if unsafe {
            AddAccessAllowedAceEx(acl, ACL_REVISION, inheritance, GENERIC_ALL, sid.as_ptr())
        } == 0
        {
            return Err(WindowsSecurityError::last_os_error());
        }
    }
    let mut encoded = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if encoded[..encoded.len().saturating_sub(1)].contains(&0) {
        return Err(WindowsSecurityError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows test security path contains NUL",
        )));
    }
    let inheritance_mode = if protected {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: the path is NUL-terminated; the ACL and all SID storage remain
    // alive; the owner is explicitly the process token user while group and
    // SACL are intentionally unchanged.
    let status = unsafe {
        SetNamedSecurityInfoW(
            encoded.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | inheritance_mode,
            sids[0].as_ptr(),
            null_mut(),
            acl,
            null_mut(),
        )
    };
    if status != 0 {
        return Err(WindowsSecurityError::status(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_service_owned_dacl_rejects_inheritance_and_untrusted_writers() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("driver-root");
        std::fs::create_dir(&root).unwrap();

        set_private_acl_for_test(&root, true, false, true).unwrap();
        open_private_directory(&root).unwrap();

        set_private_acl_for_test(&root, true, false, false).unwrap();
        assert!(matches!(
            open_private_directory(&root),
            Err(WindowsSecurityError::Insecure(message)) if message.contains("protected")
        ));

        set_private_acl_for_test(&root, true, true, true).unwrap();
        assert!(matches!(
            open_private_directory(&root),
            Err(WindowsSecurityError::Insecure(message)) if message.contains("untrusted")
        ));

        set_private_acl_for_test(&root, true, false, true).unwrap();
    }
}
