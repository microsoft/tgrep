//! Handle-rooted regular-file opens shared by reconciliation and serving.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Error, ErrorKind};
use std::path::{Component, Path, PathBuf};

/// Validate normalized root-relative index paths using the host's path rules.
pub fn validate_index_path(path: &str) -> io::Result<()> {
    if path.contains(['\\', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || (cfg!(windows) && path.contains(':'))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "index path must be a normalized root-relative path",
        ));
    }
    Ok(())
}

/// A stable directory handle, not a pathname-only containment check.
///
/// The root may be a caller-selected symlink at construction; its resolved
/// directory is pinned. Descendants must not traverse links/reparse points.
/// On Windows directory handles deny deletion while held.
pub struct RootedDir {
    path: PathBuf,
    directory: File,
    identity: same_file::Handle,
}

impl RootedDir {
    pub fn open(root: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(root)?;
        let directory = Self::open_directory(&path)?;
        let identity = same_file::Handle::from_file(directory.try_clone()?)?;
        let root = Self {
            path,
            directory,
            identity,
        };
        root.verify_root()?;
        Ok(root)
    }

    /// Fail if the registered pathname no longer names the pinned directory.
    /// This detects changes; safe file opens rely on handles, not this check.
    pub fn verify_root(&self) -> io::Result<()> {
        if same_file::Handle::from_file(Self::open_directory(&self.path)?)? != self.identity {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "root directory identity changed",
            ));
        }
        Ok(())
    }

    fn open_directory(path: &Path) -> io::Result<File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
        }
        #[cfg(windows)]
        {
            open_windows(path, true)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(Error::new(
                ErrorKind::Unsupported,
                "handle-rooted reads unsupported",
            ))
        }
    }

    /// Open a nonempty root-relative regular file, never following links.
    /// Unix opens are nonblocking so a raced-in FIFO cannot stall the caller.
    pub fn open_file(&self, relative: &Path) -> io::Result<File> {
        self.open_file_with(relative, |_| {})
    }

    fn open_file_with(
        &self,
        relative: &Path,
        mut before_component: impl FnMut(usize),
    ) -> io::Result<File> {
        let components = components(relative)?;
        self.verify_root()?;
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;

            let mut directory = self.directory.try_clone()?;
            for (index, component) in components.iter().enumerate() {
                let name = std::ffi::CString::new(component.as_bytes())
                    .map_err(|_| Error::new(ErrorKind::InvalidInput, "NUL in path component"))?;
                let mut flags =
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
                if index + 1 != components.len() {
                    flags |= libc::O_DIRECTORY;
                }
                before_component(index);
                // SAFETY: live parent descriptor and a single NUL-terminated
                // component; no component is resolved against process cwd.
                let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
                if fd < 0 {
                    return Err(Error::last_os_error());
                }
                // SAFETY: openat returned a fresh descriptor owned here.
                directory = unsafe { File::from_raw_fd(fd) };
            }
            regular(directory)
        }
        #[cfg(windows)]
        {
            // Prevent ancestor replacement, then verify the opened handle's
            // actual parent as well (including in-place reparse changes).
            let mut guards = Vec::new();
            let mut path = self.path.clone();
            for (index, component) in components.iter().enumerate() {
                path.push(component);
                before_component(index);
                let is_dir = index + 1 != components.len();
                let file = open_windows(&path, is_dir)?;
                let anchor = final_path_of(&self.directory)?;
                let parent = final_path_of(guards.last().unwrap_or(&self.directory))?;
                let opened = final_path_of(&file)?;
                if !opened.starts_with(&anchor) || opened.parent() != Some(parent.as_path()) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "file escaped pinned root",
                    ));
                }
                if !is_dir {
                    return regular(file);
                }
                guards.push(file);
            }
            unreachable!("nonempty validated components")
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (components, before_component);
            Err(Error::new(
                ErrorKind::Unsupported,
                "handle-rooted reads unsupported",
            ))
        }
    }
}

fn regular(file: File) -> io::Result<File> {
    if !file.metadata()?.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

fn components(relative: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => {
                #[cfg(windows)]
                if name.to_string_lossy().contains(':') {
                    return Err(Error::new(ErrorKind::InvalidInput, "alternate data stream"));
                }
                names.push(name.to_os_string());
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "non-literal relative path",
                ));
            }
        }
    }
    if names.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "path is the root"));
    }
    Ok(names)
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn open_windows(path: &Path, directory: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = File::options();
    options
        .share_mode(if directory {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        })
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    if directory {
        options.access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES);
    } else {
        options.read(true);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_reparse(&metadata) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "path traverses a reparse point",
        ));
    }
    if directory && !metadata.is_dir() {
        return Err(Error::new(ErrorKind::NotADirectory, "not a directory"));
    }
    Ok(file)
}

/// Query the resolved path of the actual open Windows handle.
#[cfg(windows)]
pub fn final_path_of(file: &File) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: a live borrowed handle and a buffer with its actual capacity.
        let needed = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as _,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if needed == 0 {
            return Err(Error::last_os_error());
        }
        if (needed as usize) < buffer.len() {
            buffer.truncate(needed as usize);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(needed as usize + 1, 0);
    }
}

#[cfg(test)]
pub(crate) mod tests;
