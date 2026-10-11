// Copyright (c) Microsoft Corporation. All rights reserved.

use super::storage::Directory;
use super::{Error, ErrorCategory, FileIdentity, Result};
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

fn location(path: &Path) -> Result<(Arc<Directory>, &str)> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("private control file needs a parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::invalid("private control filename must be Unicode"))?;
    Ok((Directory::open(parent)?, name))
}

/// Atomically publish a control file readable only by this OS account.
pub fn publish_private_control_file(path: &Path, value: &Value) -> Result<FileIdentity> {
    let (parent, name) = location(path)?;
    parent.publish_private_json(name, value)
}

/// Read bounded control data only after verifying its native owner-only access.
pub fn read_private_control_file(path: &Path) -> Result<Value> {
    let (parent, name) = location(path)?;
    let file = parent.open_file(name, false)?;
    verify_private(&file)?;
    let mut bytes = Vec::new();
    file.take(super::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > super::MAX_REQUEST_BYTES {
        return Err(Error::invalid(
            "private control file exceeds its size limit",
        ));
    }
    parent.verify()?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn permission() -> Error {
    Error::new(
        ErrorCategory::Permission,
        "private-control-permissions",
        "credential file must be owned by and accessible only to the current OS account",
    )
}

pub(crate) fn verify_private(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no pointer arguments or side effects.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.mode() & 0o400 == 0
        {
            return Err(permission());
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
                fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
            }
            // Darwin extended ACLs can grant access independently of mode bits.
            // Fail closed rather than publishing a secret under an inherited ACL.
            let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
            if !acl.is_null() {
                if unsafe { acl_free(acl) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                return Err(permission());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(error.into());
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows::verify(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(Error::incompatible(
            "native private control files are unsupported",
        ))
    }
}

#[cfg(windows)]
pub(crate) use windows::create_file;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION,
        EqualSid, GetAce, GetKernelObjectSecurity, GetLengthSid, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns the token handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    struct User(Vec<usize>);

    impl User {
        fn current() -> Result<Self> {
            let mut handle = std::ptr::null_mut();
            // SAFETY: a process pseudo-handle and a valid output pointer.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let token = Token(handle);
            let mut words =
                vec![0_usize; super::super::storage::SECURITY_BYTES / size_of::<usize>()];
            let mut needed = 0;
            // SAFETY: the owned token and aligned, bounded output buffer are live.
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    words.as_mut_ptr().cast(),
                    (words.len() * size_of::<usize>()) as u32,
                    &mut needed,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self(words))
        }

        fn sid(&self) -> PSID {
            // SAFETY: GetTokenInformation initialized this TOKEN_USER and its SID.
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    pub(crate) fn create_file(path: &Path) -> Result<File> {
        let user = User::current()?;
        let size = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
            + unsafe { GetLengthSid(user.sid()) } as usize;
        let mut acl = vec![0_u32; size.div_ceil(4)];
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let descriptor_ptr = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        // The protected DACL is supplied at creation, never tightened after a
        // different account could already have opened the new file.
        if unsafe {
            InitializeAcl(acl_ptr, (acl.len() * 4) as u32, ACL_REVISION) == 0
                || AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, user.sid()) == 0
                || InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) == 0
                || SetSecurityDescriptorOwner(descriptor_ptr, user.sid(), 0) == 0
                || SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) == 0
                || SetSecurityDescriptorControl(
                    descriptor_ptr,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                ) == 0
        } {
            return Err(std::io::Error::last_os_error().into());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the path, descriptor, ACL and SID outlive the native create.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: CreateFileW returned a new owned file handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub(super) fn verify(file: &File) -> Result<()> {
        let user = User::current()?;
        let mut words = vec![0_u32; super::super::storage::SECURITY_BYTES / 4];
        let descriptor = words.as_mut_ptr().cast();
        let mut needed = 0;
        let mut owner = std::ptr::null_mut();
        let mut defaulted = 0;
        let mut present = 0;
        let mut acl = std::ptr::null_mut();
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: all native outputs are live, aligned and bounded.
        if unsafe {
            GetKernelObjectSecurity(
                file.as_raw_handle(),
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                descriptor,
                (words.len() * 4) as u32,
                &mut needed,
            ) == 0
                || GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) == 0
                || GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted)
                    == 0
                || GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0
        } {
            return Err(std::io::Error::last_os_error().into());
        }
        if owner.is_null()
            || unsafe { EqualSid(owner, user.sid()) } == 0
            || present == 0
            || acl.is_null()
            || control & SE_DACL_PROTECTED == 0
            || unsafe { (*acl).AceCount } != 1
        {
            return Err(permission());
        }
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(acl, 0, &mut ace) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: the kernel descriptor contains this validated ACL entry.
        if unsafe {
            (*ace).Header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
                || (*ace).Header.AceFlags != 0
                || EqualSid(std::ptr::addr_of_mut!((*ace).SidStart).cast(), user.sid()) == 0
        } {
            return Err(permission());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_control_roundtrip_rotation_and_size_bound() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("credential.json");
        let first = publish_private_control_file(&path, &json!({"probe":1})).unwrap();
        assert_eq!(
            read_private_control_file(&path).unwrap(),
            json!({"probe":1})
        );
        let second = publish_private_control_file(&path, &json!({"probe":2})).unwrap();
        assert_ne!(first, second);
        assert!(super::super::remove_control_file(&path, &first).is_err());
        assert_eq!(
            read_private_control_file(&path).unwrap(),
            json!({"probe":2})
        );
        let mut oversized = b"{}".to_vec();
        oversized.resize(super::super::MAX_REQUEST_BYTES + 1, b' ');
        std::fs::write(&path, oversized).unwrap();
        assert_eq!(
            read_private_control_file(&path).unwrap_err().category,
            ErrorCategory::InvalidInput
        );
        super::super::remove_control_file(&path, &second).unwrap();
        temp.close().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_control_rejects_group_or_other_access_and_links() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("credential.json");
        let identity = publish_private_control_file(&path, &json!({"probe":true})).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for mode in [0o640, 0o604, 0o660, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_private_control_file(&path).unwrap_err().category,
                ErrorCategory::Permission
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_private_control_file(&link).is_err());
        std::fs::remove_file(link).unwrap();
        let link = temp.path().join("hardlink.json");
        std::fs::hard_link(&path, &link).unwrap();
        assert!(read_private_control_file(&path).is_err());
        std::fs::remove_file(link).unwrap();
        super::super::remove_control_file(&path, &identity).unwrap();
        temp.close().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_control_native_access_check_denies_restricted_principals() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Security::{
            AccessCheck, CreateRestrictedToken, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
            DISABLE_MAX_PRIVILEGE, DuplicateToken, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION,
            GetKernelObjectSecurity, OWNER_SECURITY_INFORMATION, SID_AND_ATTRIBUTES,
            SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_QUERY, WinWorldSid,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        fn owned(handle: windows_sys::Win32::Foundation::HANDLE) -> OwnedHandle {
            // SAFETY: each successful token API transfers one fresh handle.
            unsafe { OwnedHandle::from_raw_handle(handle) }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("credential.json");
        let identity = publish_private_control_file(&path, &json!({"probe":true})).unwrap();
        let file = File::open(&path).unwrap();
        let mut descriptor = vec![0_u32; super::super::storage::SECURITY_BYTES / 4];
        let mut needed = 0;
        assert_ne!(
            unsafe {
                GetKernelObjectSecurity(
                    file.as_raw_handle(),
                    OWNER_SECURITY_INFORMATION
                        | GROUP_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION,
                    descriptor.as_mut_ptr().cast(),
                    (descriptor.len() * 4) as u32,
                    &mut needed,
                )
            },
            0
        );
        let mut token = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_DUPLICATE,
                    &mut token,
                )
            },
            0
        );
        let token = owned(token);
        let mut sid = [0_u32; 32];
        let mut sid_len = size_of_val(&sid) as u32;
        assert_ne!(
            unsafe {
                CreateWellKnownSid(
                    WinWorldSid,
                    std::ptr::null_mut(),
                    sid.as_mut_ptr().cast(),
                    &mut sid_len,
                )
            },
            0
        );
        let restricting_sid = SID_AND_ATTRIBUTES {
            Sid: sid.as_mut_ptr().cast(),
            Attributes: 0,
        };
        let mut restricted = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                CreateRestrictedToken(
                    token.as_raw_handle(),
                    DISABLE_MAX_PRIVILEGE,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    1,
                    &restricting_sid,
                    &mut restricted,
                )
            },
            0
        );
        let restricted = owned(restricted);
        for (token, allowed) in [(&token, true), (&restricted, false)] {
            let mut impersonation = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    DuplicateToken(
                        token.as_raw_handle(),
                        SecurityImpersonation,
                        &mut impersonation,
                    )
                },
                0
            );
            let impersonation = owned(impersonation);
            let mapping = GENERIC_MAPPING {
                GenericRead: FILE_GENERIC_READ,
                GenericWrite: FILE_GENERIC_WRITE,
                GenericExecute: FILE_GENERIC_EXECUTE,
                GenericAll: FILE_ALL_ACCESS,
            };
            let mut privileges = [0_u64; 128];
            let mut privileges_len = size_of_val(&privileges) as u32;
            let mut granted = 0;
            let mut access = 0;
            assert_ne!(
                unsafe {
                    AccessCheck(
                        descriptor.as_mut_ptr().cast(),
                        impersonation.as_raw_handle(),
                        FILE_GENERIC_READ,
                        &mapping,
                        privileges.as_mut_ptr().cast(),
                        &mut privileges_len,
                        &mut granted,
                        &mut access,
                    )
                },
                0
            );
            assert_eq!(access != 0, allowed);
        }
        drop(file);
        super::super::remove_control_file(&path, &identity).unwrap();
        temp.close().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_control_rejects_a_null_dacl() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, InitializeSecurityDescriptor, SECURITY_DESCRIPTOR,
            SetKernelObjectSecurity, SetSecurityDescriptorDacl,
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("credential.json");
        let identity = publish_private_control_file(&path, &json!({"probe":true})).unwrap();
        let file = File::options()
            .access_mode(FILE_ALL_ACCESS)
            .open(&path)
            .unwrap();
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let descriptor = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        assert_ne!(
            unsafe { InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION) },
            0
        );
        assert_ne!(
            unsafe { SetSecurityDescriptorDacl(descriptor, 1, std::ptr::null_mut(), 0) },
            0
        );
        assert_ne!(
            unsafe {
                SetKernelObjectSecurity(file.as_raw_handle(), DACL_SECURITY_INFORMATION, descriptor)
            },
            0
        );
        drop(file);
        assert_eq!(
            read_private_control_file(&path).unwrap_err().category,
            ErrorCategory::Permission
        );
        super::super::remove_control_file(&path, &identity).unwrap();
        temp.close().unwrap();
    }
}
