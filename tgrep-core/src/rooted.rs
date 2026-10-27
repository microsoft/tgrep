//! Handle-rooted regular-file opens shared by reconciliation and serving.

use std::borrow::Borrow;
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{self, Error, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const READ_BATCH_FILES: usize = 64;

/// Bound sibling pin lifetimes without reordering or dropping candidates.
pub fn sibling_batches(paths: &[String]) -> impl Iterator<Item = &[String]> {
    sibling_batches_sized(paths, READ_BATCH_FILES)
}

/// [`sibling_batches`] with at most `size` files per group, clamped to 1..=64,
/// so that one directory's candidates can be read in parallel.
pub fn sibling_batches_sized(paths: &[String], size: usize) -> impl Iterator<Item = &[String]> {
    let size = size.clamp(1, READ_BATCH_FILES);
    paths
        .chunk_by(|left, right| Path::new(left).parent() == Path::new(right).parent())
        .flat_map(move |siblings| siblings.chunks(size))
}

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

    pub(crate) fn directory_handle(&self) -> io::Result<File> {
        self.directory.try_clone()
    }

    pub(crate) fn batch_memory_bytes(&self, parent: &Path) -> u64 {
        // Native paths, ancestor handles/identities and path-query scratch.
        let path_bytes = (self.path.as_os_str().as_encoded_bytes().len()
            + parent.as_os_str().as_encoded_bytes().len()) as u64;
        path_bytes
            .saturating_mul(12)
            .saturating_add(parent.components().count() as u64 * 192)
            .saturating_add(4096)
    }

    pub(crate) fn batch_handles(parent: &Path) -> u64 {
        #[cfg(windows)]
        {
            // Retained chain plus the independent batch-end verification chain.
            parent.components().count() as u64 * 2 + 4
        }
        #[cfg(not(windows))]
        {
            let _ = parent;
            4
        }
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

    /// Pin one ancestor chain for a bounded group of sibling reads. Finishing
    /// releases the pins; the returned IDs are observations, not lifetime proof.
    pub(crate) fn batch(&self, parent: &Path) -> io::Result<RootedBatch<&Self>> {
        Self::batch_with(self, parent)
    }

    pub(crate) fn owned_batch(
        self: &Arc<Self>,
        parent: &Path,
    ) -> io::Result<RootedBatch<Arc<Self>>> {
        Self::batch_with(Arc::clone(self), parent)
    }

    /// Reuse pinned ancestors for one bounded group from [`sibling_batches`].
    /// Call `finish` to verify ancestor identities before reporting success.
    pub fn read_batch(&self, parent: &Path) -> io::Result<FileBatch<'_>> {
        self.batch(parent).map(FileBatch)
    }

    fn batch_with<R: Borrow<Self>>(root: R, parent: &Path) -> io::Result<RootedBatch<R>> {
        let rooted = root.borrow();
        let names = if parent.as_os_str().is_empty() {
            Vec::new()
        } else {
            components(parent)?
        };
        rooted.verify_root()?;
        let mut guards = Vec::with_capacity(names.len());
        let mut identities = Vec::with_capacity(names.len());
        #[cfg(windows)]
        let anchor = final_path_of(&rooted.directory)?;
        #[cfg(windows)]
        let mut path = rooted.path.clone();
        for name in names {
            let parent_file = guards.last().unwrap_or(&rooted.directory);
            #[cfg(unix)]
            let file = open_child(parent_file, &name, true)?;
            #[cfg(windows)]
            let file = {
                path.push(name);
                let file = open_windows(&path, true)?;
                check_parent(&file, parent_file, &anchor)?;
                file
            };
            #[cfg(not(any(unix, windows)))]
            let file: File = {
                let _ = (parent_file, name);
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "handle-rooted reads unsupported",
                ));
            };
            identities.push(crate::managed::FileIdentity::of(&file).map_err(Error::other)?);
            #[cfg(unix)]
            guards.clear();
            guards.push(file);
        }
        #[cfg(windows)]
        let resolved = final_path_of(guards.last().unwrap_or(&rooted.directory))?;
        Ok(RootedBatch {
            root,
            observation: ParentObservation {
                path: parent.into(),
                identities,
            },
            guards,
            #[cfg(windows)]
            path,
            #[cfg(windows)]
            anchor,
            #[cfg(windows)]
            resolved,
        })
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

/// Native IDs observed during a rooted batch, not an ancestor lifetime pin.
///
/// IDs may be reused after handles close. A later comparison can reject an
/// observed substitution, but cannot prove continuity through publication.
/// Read containment relies on live rooted handles, not these retained IDs.
pub(crate) struct ParentObservation {
    path: PathBuf,
    identities: Vec<crate::managed::FileIdentity>,
}

impl ParentObservation {
    pub(crate) fn verification_memory_bytes(&self, root: &RootedDir) -> u64 {
        root.batch_memory_bytes(&self.path)
    }

    pub(crate) fn verification_handles(&self) -> u64 {
        RootedDir::batch_handles(&self.path)
    }

    pub(crate) fn check_current_ids(&self, root: &RootedDir) -> io::Result<()> {
        let current = root.batch(&self.path)?;
        if current.observation.identities != self.identities {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "batch ancestor identity changed",
            ));
        }
        Ok(())
    }

    pub(crate) fn memory_bytes(&self) -> u64 {
        self.path.as_os_str().as_encoded_bytes().len() as u64 * 2
            + self.identities.len() as u64 * 64
            + 128
    }
}

pub(crate) struct RootedBatch<R> {
    root: R,
    observation: ParentObservation,
    guards: Vec<File>,
    #[cfg(windows)]
    path: PathBuf,
    #[cfg(windows)]
    anchor: PathBuf,
    #[cfg(windows)]
    resolved: PathBuf,
}

pub(crate) struct OpenedFile {
    pub(crate) file: File,
    pub(crate) metadata: Metadata,
}

/// A sibling reader retaining its ancestor handles until final verification.
pub struct FileBatch<'a>(RootedBatch<&'a RootedDir>);

impl FileBatch<'_> {
    pub fn open_file(&self, relative: &Path) -> io::Result<File> {
        self.0.open_observed(relative).map(|opened| opened.file)
    }

    pub fn finish(self) -> io::Result<()> {
        self.0.finish().map(|_| ())
    }
}

impl<R: Borrow<RootedDir>> RootedBatch<R> {
    pub(crate) fn root(&self) -> &RootedDir {
        self.root.borrow()
    }

    pub(crate) fn pinned_root(&self) -> &R {
        &self.root
    }

    #[cfg(test)]
    pub(crate) fn open_file(&self, relative: &Path) -> io::Result<File> {
        self.open_observed(relative).map(|opened| opened.file)
    }

    pub(crate) fn open_observed(&self, relative: &Path) -> io::Result<OpenedFile> {
        if relative.parent() != Some(self.observation.path.as_path()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "file is not a batch sibling",
            ));
        }
        let name = relative
            .file_name()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing batch file name"))?;
        components(Path::new(name))?;
        #[cfg(unix)]
        {
            let file = open_child(
                self.guards.last().unwrap_or(&self.root().directory),
                name,
                false,
            )?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
            }
            Ok(OpenedFile { metadata, file })
        }
        #[cfg(windows)]
        {
            let observed = open_windows_observed(&self.path.join(name), false)?;
            let opened = final_path_of(&observed.file)?;
            if !opened.starts_with(&self.anchor) || opened.parent() != Some(self.resolved.as_path())
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "file escaped batch parent",
                ));
            }
            Ok(observed)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(Error::new(
                ErrorKind::Unsupported,
                "handle-rooted reads unsupported",
            ))
        }
    }

    pub(crate) fn finish(self) -> io::Result<ParentObservation> {
        #[cfg(windows)]
        debug_assert_eq!(self.guards.len(), self.observation.identities.len());
        #[cfg(unix)]
        debug_assert_eq!(
            self.guards.len(),
            usize::from(!self.observation.identities.is_empty())
        );
        self.observation.check_current_ids(self.root())?;
        Ok(self.observation)
    }
}

#[cfg(unix)]
fn open_child(parent: &File, name: &std::ffi::OsStr, directory: bool) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "NUL in path component"))?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: a live pinned parent, one literal NUL-terminated child name.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: the successful openat returned a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn check_parent(file: &File, parent: &File, anchor: &Path) -> io::Result<()> {
    let opened = final_path_of(file)?;
    if !opened.starts_with(anchor) || opened.parent() != Some(final_path_of(parent)?.as_path()) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "file escaped pinned root",
        ));
    }
    Ok(())
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
    open_windows_observed(path, directory).map(|opened| opened.file)
}

#[cfg(windows)]
fn open_windows_observed(path: &Path, directory: bool) -> io::Result<OpenedFile> {
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
    if !directory && !metadata.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(OpenedFile { file, metadata })
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
