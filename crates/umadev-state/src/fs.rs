use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn metadata_is_real_dir(meta: &fs::Metadata) -> bool {
    if !meta.file_type().is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[must_use]
pub fn metadata_is_real_file(meta: &fs::Metadata) -> bool {
    if !meta.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[must_use]
pub fn real_dir(path: &Path) -> bool {
    symlink_metadata_path(path).is_ok_and(|meta| metadata_is_real_dir(&meta))
}

#[must_use]
pub fn real_file(path: &Path) -> bool {
    symlink_metadata_path(path).is_ok_and(|meta| metadata_is_real_file(&meta))
}

#[must_use]
pub fn real_single_link_file(path: &Path) -> bool {
    use cap_fs_ext::MetadataExt as _;
    let Ok((parent, name)) = ManagedParent::open_for(path) else {
        return false;
    };
    parent
        .capability
        .symlink_metadata(&name)
        .is_ok_and(|meta| meta.is_file() && meta.nlink() == 1)
}

pub fn ensure_real_child_dir(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || !real_dir(parent)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe managed directory component",
        ));
    }
    let child = parent.join(name);
    match symlink_metadata_path(&child) {
        Ok(meta) if metadata_is_real_dir(&meta) => Ok(child),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed path is not a real directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (managed_parent, child_name) = ManagedParent::open_for(&child)?;
            match managed_parent.create_dir(&child_name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            managed_parent
                .directory_state(&child_name)
                .ok()
                .filter(|is_dir| *is_dir == Some(true))
                .map(|_| child)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed directory changed during creation",
                    )
                })
        }
        Err(error) => Err(error),
    }
}

struct ManagedParent {
    directory: File,
    capability: cap_std::fs::Dir,
}

impl ManagedParent {
    fn open_for(path: &Path) -> std::io::Result<(Self, OsString)> {
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed file has no parent",
            )
        })?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "managed file has no name")
        })?;
        validate_child_name(name)?;
        let (directory, capability) = open_managed_directory(parent)?;
        Ok((
            Self {
                directory,
                capability,
            },
            name.to_os_string(),
        ))
    }

    #[allow(clippy::fn_params_excessive_bools)]
    fn open_child(
        &self,
        name: &OsStr,
        read: bool,
        append: bool,
        create: bool,
        create_new: bool,
        unix_mode: u32,
    ) -> std::io::Result<File> {
        validate_child_name(name)?;
        #[cfg(unix)]
        let file = {
            use rustix::fs::{openat, Mode, OFlags};
            let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW;
            flags |= if read { OFlags::RDWR } else { OFlags::WRONLY };
            if append {
                flags |= OFlags::APPEND;
            }
            if create_new {
                flags |= OFlags::CREATE | OFlags::EXCL;
            } else if create {
                flags |= OFlags::CREATE;
            }
            let mode = unix_mode.try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid Unix file mode")
            })?;
            File::from(openat(
                &self.directory,
                name,
                flags,
                Mode::from_bits_truncate(mode),
            )?)
        };
        #[cfg(not(unix))]
        let (file, single_link) = {
            use cap_fs_ext::MetadataExt as _;
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
            let mut options = cap_std::fs::OpenOptions::new();
            options
                .write(true)
                .read(read)
                .append(append)
                .create(create)
                .create_new(create_new)
                .follow(FollowSymlinks::No);
            let _ = unix_mode;
            let file = retry_transient(|| self.capability.open_with(name, &options))?;
            let metadata = file.metadata()?;
            (file.into_std(), metadata.is_file() && metadata.nlink() == 1)
        };
        let metadata = file.metadata()?;
        #[cfg(unix)]
        let single_link = std::os::unix::fs::MetadataExt::nlink(&metadata) == 1;
        if !metadata_is_real_file(&metadata) || !single_link {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed path is not an exclusively linked real file",
            ));
        }
        Ok(file)
    }

    fn open_read(&self, name: &OsStr) -> std::io::Result<File> {
        use cap_fs_ext::MetadataExt as _;

        validate_child_name(name)?;
        let entry_metadata = self.capability.symlink_metadata(name)?;
        if !entry_metadata.is_file() || entry_metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed input is not an exclusively linked real file",
            ));
        }
        #[cfg(unix)]
        let file = {
            use rustix::fs::{openat, Mode, OFlags};
            File::from(openat(
                &self.directory,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )?)
        };
        #[cfg(not(unix))]
        let (file, single_link) = {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let file = retry_transient(|| self.capability.open_with(name, &options))?;
            let metadata = file.metadata()?;
            (file.into_std(), metadata.is_file() && metadata.nlink() == 1)
        };
        let metadata = file.metadata()?;
        #[cfg(unix)]
        let single_link = std::os::unix::fs::MetadataExt::nlink(&metadata) == 1;
        if !metadata_is_real_file(&metadata) || !single_link {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed input is not an exclusively linked real file",
            ));
        }
        Ok(file)
    }

    fn open_existing_or_new(
        &self,
        name: &OsStr,
        read: bool,
        append: bool,
        create: bool,
        unix_mode: u32,
    ) -> std::io::Result<File> {
        if !create {
            return self.open_child(name, read, append, false, false, unix_mode);
        }

        // `open(O_CREAT)` is not a reliable first-creator arbitration point
        // on every supported Unix under a cross-process race. Split it into
        // open-existing and create-exclusive operations so exactly one writer
        // publishes the inode and all peers subsequently open that same inode.
        for _ in 0..16 {
            match self.open_child(name, read, append, false, false, unix_mode) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match self.open_child(name, read, append, false, true, unix_mode) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            std::thread::yield_now();
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "managed file creation did not stabilize",
        ))
    }

    fn create_new(&self, name: &OsStr) -> std::io::Result<File> {
        self.open_child(name, false, false, false, true, 0o600)
    }

    fn create_new_read_write(&self, name: &OsStr) -> std::io::Result<File> {
        self.open_child(name, true, false, false, true, 0o600)
    }

    fn file_state(&self, name: &OsStr) -> std::io::Result<Option<bool>> {
        validate_child_name(name)?;
        match retry_transient(|| self.capability.symlink_metadata(name)) {
            Ok(meta) => Ok(Some(meta.is_file())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn directory_state(&self, name: &OsStr) -> std::io::Result<Option<bool>> {
        validate_child_name(name)?;
        match retry_transient(|| self.capability.symlink_metadata(name)) {
            Ok(meta) => Ok(Some(meta.is_dir())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn safe_file_or_absent(&self, name: &OsStr) -> std::io::Result<bool> {
        Ok(self.file_state(name)?.is_none_or(|is_file| is_file))
    }

    fn remove_file(&self, name: &OsStr) -> std::io::Result<()> {
        validate_child_name(name)?;
        retry_transient(|| self.capability.remove_file(name))
    }

    fn remove_dir(&self, name: &OsStr) -> std::io::Result<()> {
        validate_child_name(name)?;
        retry_transient(|| self.capability.remove_dir(name))
    }

    fn create_dir(&self, name: &OsStr) -> std::io::Result<()> {
        validate_child_name(name)?;
        retry_transient(|| self.capability.create_dir(name))
    }

    fn rename(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        self.rename_to(from, self, to)
    }

    fn rename_to(&self, from: &OsStr, target_parent: &Self, to: &OsStr) -> std::io::Result<()> {
        validate_child_name(from)?;
        validate_child_name(to)?;
        retry_transient(|| self.capability.rename(from, &target_parent.capability, to))
    }

    fn hard_link(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        validate_child_name(from)?;
        validate_child_name(to)?;
        retry_transient(|| self.capability.hard_link(from, &self.capability, to))
    }

    fn remove_file_or_link(&self, name: &OsStr) -> std::io::Result<()> {
        use cap_fs_ext::DirExt as _;
        validate_child_name(name)?;
        retry_transient(|| self.capability.remove_file_or_symlink(name))
    }

    fn sync(&self) {
        let _ = self.directory.sync_all();
    }
}

/// A directory capability opened once and reused for a related set of
/// workspace mutations. Renaming or replacing the path used to open the root
/// cannot redirect later operations to another directory tree.
pub struct RootedDir {
    root: ManagedParent,
}

/// Bounded metadata exposed by [`RootedDir::list_regular_files`]. The entry
/// name is a single component relative to the enumerated directory; no ambient
/// or absolute path is retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedRegularFile {
    pub name: OsString,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// One regular file discovered by a bounded, capability-relative tree walk.
///
/// `relative` is relative to the [`RootedDir`] itself (not to the requested
/// subtree). No ambient path is retained, so a renamed or replaced root cannot
/// redirect a later operation that uses this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedTreeFile {
    pub relative: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// One child discovered without following links beneath a [`RootedDir`].
///
/// Callers can decide which ordinary files or directories are relevant while
/// still seeing unsafe entries explicitly. `Unsafe` covers links, reparse
/// points, hard-linked files, and non-file/non-directory objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootedEntryKind {
    Directory,
    RegularFile,
    Unsafe,
}

/// Bounded metadata for one capability-relative directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedEntry {
    pub name: OsString,
    pub kind: RootedEntryKind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// Stable contents plus an open identity handle for one rooted regular file.
///
/// The identity is intentionally opaque. Callers can publish `bytes()` to a
/// second rooted namespace and then ask the originating [`RootedDir`] to
/// unlink the source only if both its file identity and complete contents are
/// still unchanged.
pub struct RootedFileRead {
    bytes: Vec<u8>,
    identity: same_file::Handle,
    len: u64,
    modified: Option<SystemTime>,
}

impl std::fmt::Debug for RootedFileRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootedFileRead")
            .field(
                "bytes",
                &format_args!("<{} private bytes>", self.bytes.len()),
            )
            .field("len", &self.len)
            .field("modified", &self.modified)
            .finish_non_exhaustive()
    }
}

impl RootedFileRead {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Debug for RootedDir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A capability is deliberately opaque: do not expose an OS handle or
        // recoverable filesystem path through diagnostics.
        formatter
            .debug_struct("RootedDir")
            .field("root", &"<directory capability>")
            .finish()
    }
}

impl RootedDir {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let canonical = fs::canonicalize(root)?;
        let (directory, capability) = open_managed_directory(&canonical)?;
        Ok(Self {
            root: ManagedParent {
                directory,
                capability,
            },
        })
    }

    /// Open the caller-designated root itself without resolving a link or
    /// reparse point at that boundary.
    pub fn open_no_follow(root: &Path) -> std::io::Result<Self> {
        let (directory, capability) = open_managed_directory(root)?;
        Ok(Self {
            root: ManagedParent {
                directory,
                capability,
            },
        })
    }

    /// Duplicate this directory capability without resolving its original
    /// ambient path again.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            root: ManagedParent {
                directory: self.root.directory.try_clone()?,
                capability: self.root.capability.try_clone()?,
            },
        })
    }

    fn parent(
        &self,
        relative: &Path,
        create_parents: bool,
    ) -> std::io::Result<(ManagedParent, OsString)> {
        parent_from(&self.root, relative, create_parents)
    }

    /// Returns whether `path` still names the directory captured by this
    /// capability. A renamed/replaced root therefore fails closed before a
    /// caller exposes an ambient path to another process.
    pub fn matches_path(&self, path: &Path) -> std::io::Result<bool> {
        if !real_dir(path) {
            return Ok(false);
        }
        let opened = same_file::Handle::from_file(self.root.directory.try_clone()?)?;
        match same_file::Handle::from_path(path) {
            Ok(current) => Ok(opened == current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Compare two already-open directory capabilities without resolving
    /// either original ambient path again.
    pub fn same_directory(&self, other: &Self) -> std::io::Result<bool> {
        let left = same_file::Handle::from_file(self.root.directory.try_clone()?)?;
        let right = same_file::Handle::from_file(other.root.directory.try_clone()?)?;
        Ok(left == right)
    }

    /// Validate every existing component without following links. A missing
    /// suffix is valid because a later rooted write may create it.
    pub fn validate_path(&self, relative: &Path) -> std::io::Result<()> {
        use cap_fs_ext::DirExt as _;

        let parts = relative_parts(relative)?;
        let mut capability = self.root.capability.try_clone()?;
        for (index, part) in parts.iter().enumerate() {
            let final_component = index + 1 == parts.len();
            let metadata = match capability.symlink_metadata(part) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if metadata.is_dir() {
                let next = capability.open_dir_nofollow(part)?;
                if !next
                    .try_clone()?
                    .into_std_file()
                    .metadata()
                    .is_ok_and(|metadata| metadata_is_real_dir(&metadata))
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "checkpoint path contains an unsafe directory",
                    ));
                }
                if !final_component {
                    capability = next;
                }
            } else if metadata.is_file() && final_component {
                let parent = ManagedParent {
                    directory: capability.try_clone()?.into_std_file(),
                    capability: capability.try_clone()?,
                };
                let file = parent.open_read(part)?;
                if !metadata_is_real_file(&file.metadata()?) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "checkpoint path contains an unsafe file",
                    ));
                }
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "checkpoint path contains a symlink/reparse/special entry",
                ));
            }
        }
        Ok(())
    }

    pub fn create_new_private(
        &self,
        relative: &Path,
        create_parents: bool,
    ) -> std::io::Result<File> {
        let (parent, name) = self.parent(relative, create_parents)?;
        parent.create_new_read_write(&name)
    }

    /// Create one directory below this capability. Existing entries are never
    /// accepted, and optional parent creation also stays below this root.
    pub fn create_dir(&self, relative: &Path, create_parents: bool) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        parent.create_dir(&name)
    }

    /// Ensure one real directory exists below this capability. A concurrent
    /// creator is accepted only after reopening the entry without following
    /// links or reparse points.
    pub fn ensure_dir(&self, relative: &Path, create_parents: bool) -> std::io::Result<()> {
        use cap_fs_ext::DirExt as _;

        let (parent, name) = self.parent(relative, create_parents)?;
        match parent.create_dir(&name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        match parent.directory_state(&name)? {
            Some(true) => {}
            Some(false) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "rooted directory is linked, reparsed, or special",
                ));
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "rooted directory disappeared during creation",
                ));
            }
        }
        let directory = parent.capability.open_dir_nofollow(&name)?;
        if !directory
            .into_std_file()
            .metadata()
            .is_ok_and(|metadata| metadata_is_real_dir(&metadata))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted directory is linked, reparsed, or special",
            ));
        }
        Ok(())
    }

    pub fn atomic_write(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_parents: bool,
    ) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        atomic_write_in(&parent, &name, bytes, None)
    }

    /// Publish a complete private file without replacing an existing entry.
    pub fn publish_new_private(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_parents: bool,
    ) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        publish_new_private_in(&parent, &name, bytes)
    }

    /// Open or create a private advisory-lock file below this capability.
    pub fn open_private_lock(
        &self,
        relative: &Path,
        create_parents: bool,
    ) -> std::io::Result<File> {
        let (parent, name) = self.parent(relative, create_parents)?;
        if !parent.safe_file_or_absent(&name)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe rooted lock path",
            ));
        }
        parent.open_existing_or_new(&name, true, false, true, 0o600)
    }

    /// Open an existing private advisory-lock file without manufacturing a
    /// missing lease while probing stale ownership.
    pub fn open_private_existing_lock(&self, relative: &Path) -> std::io::Result<File> {
        let (parent, name) = self.parent(relative, false)?;
        if !parent.safe_file_or_absent(&name)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe rooted existing lock path",
            ));
        }
        parent.open_existing_or_new(&name, true, false, false, 0o600)
    }

    /// Open or create an ordinary umask-controlled append file below this
    /// capability. New Unix files are requested with mode `0666`.
    pub fn open_regular_append(
        &self,
        relative: &Path,
        create_parents: bool,
    ) -> std::io::Result<File> {
        let (parent, name) = self.parent(relative, create_parents)?;
        if !parent.safe_file_or_absent(&name)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe rooted append path",
            ));
        }
        parent.open_existing_or_new(&name, false, true, true, 0o666)
    }

    /// Append to a private regular file below this capability.
    pub fn append_private(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_parents: bool,
    ) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        let mut file = parent.open_existing_or_new(&name, false, true, true, 0o600)?;
        file.write_all(bytes)
    }

    /// Append to a private regular file and flush it before returning.
    pub fn append_private_synced(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_parents: bool,
    ) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        let mut file = parent.open_existing_or_new(&name, false, true, true, 0o600)?;
        file.write_all(bytes)?;
        file.sync_data()
    }

    /// Read one bounded regular file through this root. The leaf is reopened
    /// through the same parent capability after reading so replacement or
    /// concurrent growth is rejected.
    pub fn read_bounded(&self, relative: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        let (parent, name) = self.parent(relative, false)?;
        let file = open_read_source_in(&parent, &name)?;
        read_bounded_stable_in(&parent, &name, &file, max_bytes)
    }

    /// Read at most the first `max_bytes` bytes of a rooted regular file.
    ///
    /// Unlike [`Self::read_bounded`], a larger file is accepted and clipped.
    /// The leaf and all ancestors remain no-follow, and the open identity plus
    /// metadata are rechecked after the read.
    pub fn read_prefix(&self, relative: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        let (parent, name) = self.parent(relative, false)?;
        let file = open_read_source_in(&parent, &name)?;
        let opened = same_file::Handle::from_file(file.try_clone()?)?;
        let before = file.metadata()?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(before.len().min(max_bytes))
                .unwrap_or(0)
                .min(64 * 1024),
        );
        (&file).take(max_bytes).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let current = open_read_source_in(&parent, &name)?;
        if opened != same_file::Handle::from_file(current)?
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed input changed while its prefix was read",
            ));
        }
        Ok(bytes)
    }

    /// Read a single-link regular file through this capability and retain its
    /// open filesystem identity for a later conditional unlink.
    pub fn read_bounded_verified(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> std::io::Result<RootedFileRead> {
        use cap_fs_ext::MetadataExt as _;

        let (parent, name) = self.parent(relative, false)?;
        let metadata = parent.capability.symlink_metadata(&name)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted source is linked, reparsed, or special",
            ));
        }
        let file = parent.open_read(&name)?;
        let opened_metadata = file.metadata()?;
        let identity = same_file::Handle::from_file(file.try_clone()?)?;
        let bytes = read_bounded_stable_in(&parent, &name, &file, max_bytes)?;
        Ok(RootedFileRead {
            len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            modified: opened_metadata.modified().ok(),
            bytes,
            identity,
        })
    }

    /// Unlink a rooted regular file only when it is still the exact file read
    /// by [`Self::read_bounded_verified`] and its complete bounded contents
    /// remain byte-for-byte identical.
    ///
    /// Keeping the original identity handle alive closes the usual
    /// preflight-to-unlink replacement window on supported platforms. The
    /// operation is capability-relative and never resolves an ambient root.
    pub fn remove_regular_file_if_unchanged(
        &self,
        relative: &Path,
        proof: &RootedFileRead,
    ) -> std::io::Result<bool> {
        use cap_fs_ext::MetadataExt as _;

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let metadata = match parent.capability.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted unlink target is linked, reparsed, or special",
            ));
        }
        let current = parent.open_read(&name)?;
        let current_metadata = current.metadata()?;
        let current_identity = same_file::Handle::from_file(current.try_clone()?)?;
        if current_identity != proof.identity
            || current_metadata.len() != proof.len
            || current_metadata.modified().ok() != proof.modified
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "rooted file identity changed before unlink",
            ));
        }
        let bytes = read_bounded_stable_in(&parent, &name, &current, proof.len)?;
        if bytes != proof.bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "rooted file contents changed before unlink",
            ));
        }
        // Re-open once more after the content check. Namespace replacement is
        // rejected before the capability-relative quarantine rename.
        let final_file = parent.open_read(&name)?;
        let final_identity = same_file::Handle::from_file(final_file)?;
        if final_identity != proof.identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "rooted file identity changed at unlink",
            ));
        }

        // There is no portable unlink-if-file-id syscall. Move the leaf to an
        // unpredictable sibling through the already pinned parent, then prove
        // the moved identity and contents before unlinking that sibling. If a
        // racer replaced the leaf immediately before rename, put that entry
        // back with a no-replace hard link or leave the quarantine recoverable;
        // never delete the unproven replacement.
        let quarantine = loop {
            use std::fmt::Write as _;

            let mut nonce = [0u8; 16];
            getrandom::getrandom(&mut nonce)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let mut suffix = String::with_capacity("umadev-unlink-".len() + nonce.len() * 2);
            suffix.push_str("umadev-unlink-");
            for byte in nonce {
                write!(&mut suffix, "{byte:02x}").expect("writing to String cannot fail");
            }
            let candidate = sibling_name(&name, &suffix)?;
            if parent.file_state(&candidate)?.is_none() {
                break candidate;
            }
        };
        parent.rename(&name, &quarantine)?;
        let quarantined = match parent.open_read(&quarantine) {
            Ok(file) => file,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "rooted unlink quarantine could not be verified ({error}); file remains recoverable"
                    ),
                ));
            }
        };
        let quarantined_identity = same_file::Handle::from_file(quarantined.try_clone()?)?;
        let quarantined_bytes =
            read_bounded_stable_in(&parent, &quarantine, &quarantined, proof.len)?;
        if quarantined_identity != proof.identity || quarantined_bytes != proof.bytes {
            let restored = parent.hard_link(&quarantine, &name);
            if restored.is_ok() {
                let _ = parent.remove_file(&quarantine);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                if restored.is_ok() {
                    "rooted unlink quarantined a replaced file and restored it"
                } else {
                    "rooted unlink quarantined a replaced file; it remains recoverable"
                },
            ));
        }
        parent.remove_file(&quarantine)?;
        parent.sync();
        Ok(true)
    }

    /// Rename an entry within this root. Both parents remain capability-pinned
    /// for the complete namespace operation.
    pub fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let (from_parent, from_name) = self.parent(from, false)?;
        let (to_parent, to_name) = self.parent(to, false)?;
        from_parent.rename_to(&from_name, &to_parent, &to_name)
    }

    /// Return the modification timestamp for one real directory entry.
    pub fn directory_modified(&self, relative: &Path) -> std::io::Result<Option<SystemTime>> {
        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match parent.capability.symlink_metadata(&name) {
            Ok(metadata) if metadata.is_dir() => metadata
                .modified()
                .map(|modified| Some(modified.into_std())),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted entry is not a real directory",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Return whether a leaf exists as a regular non-link file. A linked,
    /// reparsed, directory, or special leaf is rejected rather than reported
    /// as an ordinary absence.
    pub fn regular_file_exists(&self, relative: &Path) -> std::io::Result<bool> {
        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match parent.file_state(&name)? {
            Some(true) => Ok(true),
            Some(false) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted entry is not a regular file",
            )),
            None => Ok(false),
        }
    }

    /// Return the length of one exclusively linked regular file without
    /// resolving the root or any ancestor through an ambient path.
    pub fn regular_file_len(&self, relative: &Path) -> std::io::Result<Option<u64>> {
        use cap_fs_ext::MetadataExt as _;

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = match parent.capability.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted entry is not an exclusively linked regular file",
            ));
        }
        Ok(Some(metadata.len()))
    }

    /// Atomically replace one regular file with a sibling regular file through
    /// this already-open root. Both names must share the same relative parent.
    pub fn replace_regular_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if from.parent() != to.parent() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe cross-directory rooted regular-file replacement",
            ));
        }
        let (parent, to_name) = self.parent(to, false)?;
        let from_name = from.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "rooted replacement source has no name",
            )
        })?;
        validate_child_name(from_name)?;
        if parent.file_state(from_name)? != Some(true) || !parent.safe_file_or_absent(&to_name)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe rooted regular-file replacement",
            ));
        }
        recover_pending_in(&parent, &to_name)?;
        rename_replacing_in(&parent, from_name, &to_name)
    }

    /// Enumerate a single directory as exclusively linked regular files.
    /// Linked/reparse/special entries and unbounded directories fail closed.
    pub fn list_regular_files(
        &self,
        relative: &Path,
        max_entries: usize,
    ) -> std::io::Result<Vec<RootedRegularFile>> {
        use cap_fs_ext::{DirExt as _, MetadataExt as _};

        let (parent, name) = self.parent(relative, false)?;
        let directory = parent.capability.open_dir_nofollow(&name)?;
        let mut files = Vec::new();
        for entry in directory.entries()? {
            if files.len() >= max_entries {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "rooted directory exceeds its entry bound",
                ));
            }
            let entry = entry?;
            let name = entry.file_name();
            validate_child_name(&name)?;
            let metadata = directory.symlink_metadata(&name)?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "rooted directory contains a linked, reparsed, or special entry",
                ));
            }
            files.push(RootedRegularFile {
                name,
                len: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .map(cap_std::time::SystemTime::into_std),
            });
        }
        files.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(files)
    }

    /// Enumerate one directory through this capability with a hard entry cap.
    ///
    /// An empty `relative` path names the root itself. Enumeration never follows
    /// child links or reparse points; those entries are returned as `Unsafe`.
    pub fn list_entries(
        &self,
        relative: &Path,
        max_entries: usize,
    ) -> std::io::Result<Vec<RootedEntry>> {
        use cap_fs_ext::{
            DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _,
        };

        let directory = self.open_relative_directory(relative)?;
        let mut entries = Vec::new();
        for entry in directory.entries()? {
            if entries.len() >= max_entries {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "rooted directory exceeds its entry bound",
                ));
            }
            let entry = entry?;
            let name = entry.file_name();
            validate_child_name(&name)?;
            let metadata = directory.symlink_metadata(&name)?;
            let kind = if metadata.is_dir() {
                match directory.open_dir_nofollow(&name) {
                    Ok(child) => {
                        if child
                            .into_std_file()
                            .metadata()
                            .is_ok_and(|value| metadata_is_real_dir(&value))
                        {
                            RootedEntryKind::Directory
                        } else {
                            RootedEntryKind::Unsafe
                        }
                    }
                    _ => RootedEntryKind::Unsafe,
                }
            } else if metadata.is_file() && metadata.nlink() == 1 {
                let mut options = cap_std::fs::OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No);
                match directory.open_with(&name, &options) {
                    Ok(file) => {
                        if file
                            .into_std()
                            .metadata()
                            .is_ok_and(|value| metadata_is_real_file(&value))
                        {
                            RootedEntryKind::RegularFile
                        } else {
                            RootedEntryKind::Unsafe
                        }
                    }
                    _ => RootedEntryKind::Unsafe,
                }
            } else {
                RootedEntryKind::Unsafe
            };
            entries.push(RootedEntry {
                name,
                kind,
                len: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .map(cap_std::time::SystemTime::into_std),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn open_relative_directory(&self, relative: &Path) -> std::io::Result<cap_std::fs::Dir> {
        use cap_fs_ext::DirExt as _;

        let mut directory = self.root.capability.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::CurDir) {
                    continue;
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed directory contains an escaping component",
                ));
            };
            let next = directory.open_dir_nofollow(name)?;
            if !next
                .try_clone()?
                .into_std_file()
                .metadata()
                .is_ok_and(|metadata| metadata_is_real_dir(&metadata))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed directory is linked, reparsed, or special",
                ));
            }
            directory = next;
        }
        Ok(directory)
    }

    /// Open a descendant directory as another pinned capability.
    pub fn open_dir(&self, relative: &Path) -> std::io::Result<Self> {
        let capability = self.open_relative_directory(relative)?;
        let directory = capability.try_clone()?.into_std_file();
        if !directory
            .metadata()
            .is_ok_and(|metadata| metadata_is_real_dir(&metadata))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed directory is linked, reparsed, or special",
            ));
        }
        Ok(Self {
            root: ManagedParent {
                directory,
                capability,
            },
        })
    }

    /// Recursively enumerate a bounded rooted tree, accepting only ordinary
    /// single-link regular files and real directories. Symlinks, reparse
    /// points, special files, hard-linked files, excessive depth/node counts,
    /// and excessive aggregate bytes all fail closed.
    pub fn list_regular_tree(
        &self,
        relative: &Path,
        max_depth: usize,
        max_nodes: usize,
        max_files: usize,
        max_bytes: u64,
    ) -> std::io::Result<Vec<RootedTreeFile>> {
        use cap_fs_ext::{DirExt as _, MetadataExt as _};

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let metadata = match parent.capability.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if metadata.is_file() {
            if metadata.nlink() != 1
                || metadata.len() > max_bytes
                || max_files == 0
                || max_nodes == 0
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "rooted file exceeds bounds or is linked",
                ));
            }
            let opened = parent.open_read(&name)?;
            if !metadata_is_real_file(&opened.metadata()?) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "rooted file is reparsed or special",
                ));
            }
            return Ok(vec![RootedTreeFile {
                relative: relative.to_path_buf(),
                len: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .map(cap_std::time::SystemTime::into_std),
            }]);
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted tree contains a linked, reparsed, or special entry",
            ));
        }
        let directory = parent.capability.open_dir_nofollow(&name)?;
        let mut files = Vec::new();
        let mut visited = 0usize;
        let mut total_bytes = 0u64;
        collect_regular_tree(
            &directory,
            relative,
            0,
            &mut visited,
            &mut total_bytes,
            &mut files,
            max_depth,
            max_nodes,
            max_files,
            max_bytes,
        )?;
        files.sort_by(|left, right| left.relative.cmp(&right.relative));
        Ok(files)
    }

    /// Verify that a directory tree contains only real directories and no
    /// remaining file or special entry.
    pub fn validate_empty_directory_tree(
        &self,
        relative: &Path,
        max_depth: usize,
        max_nodes: usize,
    ) -> std::io::Result<()> {
        use cap_fs_ext::DirExt as _;

        let (parent, name) = self.parent(relative, false)?;
        let directory = parent.capability.open_dir_nofollow(&name)?;
        let mut visited = 0usize;
        validate_empty_tree(&directory, 0, &mut visited, max_depth, max_nodes)
    }

    /// Remove a tree only when every descendant is an empty real directory.
    /// Files, links, reparse points, and concurrently replaced entries are
    /// never removed.
    pub fn remove_empty_directory_tree(
        &self,
        relative: &Path,
        max_depth: usize,
        max_nodes: usize,
    ) -> std::io::Result<bool> {
        use cap_fs_ext::DirExt as _;

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let directory = match parent.capability.open_dir_nofollow(&name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let opened_file = directory.try_clone()?.into_std_file();
        let mut visited = 0usize;
        remove_empty_tree_contents(&directory, 0, &mut visited, max_depth, max_nodes)?;
        let current = parent.capability.open_dir_nofollow(&name)?.into_std_file();
        if same_file::Handle::from_file(opened_file)? != same_file::Handle::from_file(current)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "rooted directory changed before removal",
            ));
        }
        parent.remove_dir(&name)?;
        Ok(true)
    }

    #[cfg(unix)]
    pub fn atomic_write_with_unix_mode(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_parents: bool,
        mode: u32,
    ) -> std::io::Result<()> {
        let (parent, name) = self.parent(relative, create_parents)?;
        atomic_write_in(&parent, &name, bytes, Some(mode))
    }

    pub fn remove_regular_file(&self, relative: &Path) -> std::io::Result<bool> {
        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        recover_pending_in(&parent, &name)?;
        match parent.file_state(&name)? {
            Some(true) => {
                parent.remove_file(&name)?;
                Ok(true)
            }
            Some(false) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to remove a non-regular rooted path",
            )),
            None => Ok(false),
        }
    }

    pub fn remove_file_entry(&self, relative: &Path) -> std::io::Result<bool> {
        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match parent.capability.symlink_metadata(&name) {
            Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to remove a rooted directory as a file",
            )),
            Ok(_) => {
                parent.remove_file_or_link(&name)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn remove_empty_dir(&self, relative: &Path) -> std::io::Result<bool> {
        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match parent.directory_state(&name)? {
            Some(true) => {
                parent.remove_dir(&name)?;
                Ok(true)
            }
            Some(false) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to remove a non-directory rooted path",
            )),
            None => Ok(false),
        }
    }

    pub fn is_real_dir(&self, relative: &Path) -> std::io::Result<bool> {
        use cap_fs_ext::DirExt as _;

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match parent.directory_state(&name)? {
            Some(true) => {
                let directory = parent.capability.open_dir_nofollow(&name)?;
                Ok(directory
                    .into_std_file()
                    .metadata()
                    .is_ok_and(|metadata| metadata_is_real_dir(&metadata)))
            }
            Some(false) | None => Ok(false),
        }
    }

    #[cfg(unix)]
    pub fn unix_file_mode(&self, relative: &Path) -> std::io::Result<Option<u32>> {
        use std::os::unix::fs::PermissionsExt as _;

        let (parent, name) = match self.parent(relative, false) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match parent.file_state(&name)? {
            Some(true) => Ok(Some(
                parent.open_read(&name)?.metadata()?.permissions().mode() & 0o777,
            )),
            Some(false) | None => Ok(None),
        }
    }
}

fn validate_empty_tree(
    directory: &cap_std::fs::Dir,
    depth: usize,
    visited: &mut usize,
    max_depth: usize,
    max_nodes: usize,
) -> std::io::Result<()> {
    use cap_fs_ext::DirExt as _;

    if depth > max_depth || *visited >= max_nodes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted directory tree exceeds its validation bound",
        ));
    }
    *visited += 1;
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        validate_child_name(&name)?;
        let metadata = directory.symlink_metadata(&name)?;
        if metadata.is_dir() {
            let child = directory.open_dir_nofollow(&name)?;
            validate_empty_tree(&child, depth + 1, visited, max_depth, max_nodes)?;
        } else if metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::DirectoryNotEmpty,
                "rooted directory tree still contains a regular file",
            ));
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted directory tree contains a linked, reparsed, or special entry",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_regular_tree(
    directory: &cap_std::fs::Dir,
    prefix: &Path,
    depth: usize,
    visited: &mut usize,
    total_bytes: &mut u64,
    files: &mut Vec<RootedTreeFile>,
    max_depth: usize,
    max_nodes: usize,
    max_files: usize,
    max_bytes: u64,
) -> std::io::Result<()> {
    use cap_fs_ext::DirExt as _;

    if depth > max_depth || *visited >= max_nodes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted tree exceeds its depth or node bound",
        ));
    }
    *visited += 1;
    let mut names = Vec::new();
    for entry in directory.entries()? {
        let name = entry?.file_name();
        validate_child_name(&name)?;
        names.push(name);
        if names.len() > max_nodes.saturating_sub(*visited) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rooted tree exceeds its node bound",
            ));
        }
    }
    names.sort();
    for name in names {
        if *visited >= max_nodes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rooted tree exceeds its node bound",
            ));
        }
        *visited += 1;
        let metadata = directory.symlink_metadata(&name)?;
        let relative = prefix.join(&name);
        if metadata.is_file() {
            collect_regular_tree_file(
                directory,
                &name,
                &metadata,
                relative,
                total_bytes,
                files,
                (max_files, max_bytes),
            )?;
        } else if metadata.is_dir() {
            if depth >= max_depth {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "rooted tree exceeds its depth bound",
                ));
            }
            let child = directory.open_dir_nofollow(&name)?;
            collect_regular_tree(
                &child,
                &relative,
                depth + 1,
                visited,
                total_bytes,
                files,
                max_depth,
                max_nodes,
                max_files,
                max_bytes,
            )?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "rooted tree contains a linked, reparsed, or special entry",
            ));
        }
    }
    Ok(())
}

fn collect_regular_tree_file(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    metadata: &cap_std::fs::Metadata,
    relative: PathBuf,
    total_bytes: &mut u64,
    files: &mut Vec<RootedTreeFile>,
    limits: (usize, u64),
) -> std::io::Result<()> {
    use cap_fs_ext::{FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};

    if metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "rooted tree contains a linked regular file",
        ));
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let opened = directory.open_with(name, &options)?;
    if !metadata_is_real_file(&opened.into_std().metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "rooted tree contains a reparsed or special file",
        ));
    }
    if files.len() >= limits.0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted tree exceeds its file bound",
        ));
    }
    *total_bytes = total_bytes.checked_add(metadata.len()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted tree byte count overflow",
        )
    })?;
    if *total_bytes > limits.1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted tree exceeds its byte bound",
        ));
    }
    files.push(RootedTreeFile {
        relative,
        len: metadata.len(),
        modified: metadata
            .modified()
            .ok()
            .map(cap_std::time::SystemTime::into_std),
    });
    Ok(())
}

fn remove_empty_tree_contents(
    directory: &cap_std::fs::Dir,
    depth: usize,
    visited: &mut usize,
    max_depth: usize,
    max_nodes: usize,
) -> std::io::Result<()> {
    use cap_fs_ext::DirExt as _;

    if depth > max_depth || *visited >= max_nodes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rooted directory tree exceeds its removal bound",
        ));
    }
    *visited += 1;
    let mut names = Vec::new();
    for entry in directory.entries()? {
        let name = entry?.file_name();
        validate_child_name(&name)?;
        names.push(name);
        if names.len() > max_nodes.saturating_sub(*visited) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rooted directory tree exceeds its removal bound",
            ));
        }
    }
    names.sort();
    for name in names {
        let metadata = directory.symlink_metadata(&name)?;
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                if metadata.is_file() {
                    std::io::ErrorKind::DirectoryNotEmpty
                } else {
                    std::io::ErrorKind::PermissionDenied
                },
                "rooted directory tree contains a non-directory entry",
            ));
        }
        let child = directory.open_dir_nofollow(&name)?;
        let opened = child.try_clone()?.into_std_file();
        remove_empty_tree_contents(&child, depth + 1, visited, max_depth, max_nodes)?;
        let current = directory.open_dir_nofollow(&name)?.into_std_file();
        if same_file::Handle::from_file(opened)? != same_file::Handle::from_file(current)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "rooted child directory changed before removal",
            ));
        }
        retry_transient(|| directory.remove_dir(&name))?;
    }
    Ok(())
}

fn relative_parts(relative: &Path) -> std::io::Result<Vec<OsString>> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed path contains an escaping component",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed path is empty",
        ));
    }
    Ok(parts)
}

fn parent_beneath(
    root: &Path,
    relative: &Path,
    create_parents: bool,
) -> std::io::Result<(ManagedParent, OsString)> {
    RootedDir::open(root)?.parent(relative, create_parents)
}

fn parent_from(
    root: &ManagedParent,
    relative: &Path,
    create_parents: bool,
) -> std::io::Result<(ManagedParent, OsString)> {
    use cap_fs_ext::DirExt as _;

    let parts = relative_parts(relative)?;
    let mut capability = root.capability.try_clone()?;
    for part in &parts[..parts.len() - 1] {
        match capability.open_dir_nofollow(part) {
            Ok(next) => capability = next,
            Err(error) if create_parents && error.kind() == std::io::ErrorKind::NotFound => {
                match capability.create_dir(part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                capability = capability.open_dir_nofollow(part)?;
            }
            Err(error) => return Err(error),
        }
    }
    let directory = capability.try_clone()?.into_std_file();
    Ok((
        ManagedParent {
            directory,
            capability,
        },
        parts.last().expect("non-empty path checked above").clone(),
    ))
}

fn open_managed_directory(path: &Path) -> std::io::Result<(File, cap_std::fs::Dir)> {
    let before_meta = symlink_metadata_path(path)?;
    if !metadata_is_real_dir(&before_meta) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed directory",
        ));
    }
    let before = same_file::Handle::from_path(path)?;
    let directory = open_directory_no_follow(path)?;
    let opened = same_file::Handle::from_file(directory.try_clone()?)?;
    let current = same_file::Handle::from_path(path)?;
    if !directory
        .metadata()
        .is_ok_and(|meta| metadata_is_real_dir(&meta))
        || opened != before
        || opened != current
        || !real_dir(path)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed directory changed while opening",
        ));
    }
    let capability = cap_std::fs::Dir::from_std_file(directory.try_clone()?);
    Ok((directory, capability))
}

fn validate_child_name(name: &OsStr) -> std::io::Result<()> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe managed filename",
        ))
    }
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_directory_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    // Omitting FILE_SHARE_DELETE pins this directory and its ancestor chain
    // while capability-relative namespace operations resolve their paths.
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    retry_transient(|| options.open(path))
}

#[cfg(all(not(unix), not(windows)))]
fn open_directory_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn open_existing_or_new_no_follow(
    path: &Path,
    read: bool,
    append: bool,
    create: bool,
) -> std::io::Result<File> {
    let (parent, name) = ManagedParent::open_for(path).map_err(|error| {
        std::io::Error::new(error.kind(), format!("open managed parent: {error}"))
    })?;
    if !parent.safe_file_or_absent(&name).map_err(|error| {
        std::io::Error::new(error.kind(), format!("inspect managed child: {error}"))
    })? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed file path",
        ));
    }
    parent
        .open_existing_or_new(&name, read, append, create, 0o600)
        .map_err(|error| std::io::Error::new(error.kind(), format!("open managed child: {error}")))
}

#[cfg(test)]
fn sibling(path: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid filename"))?;
    Ok(path.with_file_name(format!(".{name}.{suffix}")))
}

#[cfg(test)]
fn pending_path(path: &Path) -> std::io::Result<PathBuf> {
    sibling(path, "umadev-replace-pending")
}

fn sibling_name(name: &OsStr, suffix: &str) -> std::io::Result<OsString> {
    validate_child_name(name)?;
    if suffix.is_empty() || suffix.contains('/') || suffix.contains('\\') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid managed sibling suffix",
        ));
    }
    let mut sibling = OsString::from(".");
    sibling.push(name);
    sibling.push(".");
    sibling.push(suffix);
    Ok(sibling)
}

fn recover_pending_in(parent: &ManagedParent, name: &OsStr) -> std::io::Result<()> {
    let pending = sibling_name(name, "umadev-replace-pending")?;
    match parent.file_state(&pending)? {
        None => return Ok(()),
        Some(true) => {}
        Some(false) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe replacement recovery file",
            ));
        }
    }
    match parent.file_state(name)? {
        Some(true) => parent.remove_file(&pending),
        Some(false) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe replacement target",
        )),
        None => parent.rename(&pending, name),
    }
}

fn rename_replacing_in(
    parent: &ManagedParent,
    temp: &OsStr,
    target: &OsStr,
) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        parent.rename(temp, target)
    }
    #[cfg(windows)]
    {
        match parent.file_state(target)? {
            None => return parent.rename(temp, target),
            Some(true) => {}
            Some(false) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unsafe replacement target",
                ));
            }
        }
        let pending = sibling_name(target, "umadev-replace-pending")?;
        if !parent.safe_file_or_absent(target)? || !parent.safe_file_or_absent(&pending)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe replacement path",
            ));
        }
        parent.rename(target, &pending)?;
        match parent.rename(temp, target) {
            Ok(()) => {
                let _ = parent.remove_file(&pending);
                Ok(())
            }
            Err(error) => match parent.rename(&pending, target) {
                Ok(()) => Err(error),
                Err(restore) => Err(std::io::Error::new(
                    error.kind(),
                    format!("{error}; previous data remains recoverable ({restore})"),
                )),
            },
        }
    }
}

#[cfg(windows)]
fn retry_transient_windows_fs<T>(
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let started = std::time::Instant::now();
    let retry_for = std::time::Duration::from_secs(2);
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && started.elapsed() < retry_for =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Retry a transient Windows filesystem denial for a bounded interval so the
/// whole durable-store family shares one robustness level.
///
/// An antivirus scanner, search indexer, or a just-closing handle can briefly
/// deny a managed-path operation with a sharing/lock/access-denied native
/// error. This wrapper re-attempts only those transient denials and, off
/// Windows, is a single pass-through call. Fail-open: it never changes the
/// operation's own success or its terminal error, it only re-runs a transient
/// failure. Sibling durable-store paths (lock directories, ledger tail reads)
/// route their raw `std::fs` calls through here or the retrying helpers below.
pub fn retry_transient<T>(operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(windows)]
    {
        retry_transient_windows_fs(operation)
    }
    #[cfg(not(windows))]
    {
        let mut operation = operation;
        operation()
    }
}

#[cfg(windows)]
fn symlink_metadata_path(path: &Path) -> std::io::Result<fs::Metadata> {
    retry_transient_windows_fs(|| fs::symlink_metadata(path))
}

#[cfg(not(windows))]
fn symlink_metadata_path(path: &Path) -> std::io::Result<fs::Metadata> {
    fs::symlink_metadata(path)
}

fn atomic_write_in(
    parent: &ManagedParent,
    name: &OsStr,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> std::io::Result<()> {
    if !parent.safe_file_or_absent(name)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed output path",
        ));
    }
    recover_pending_in(parent, name)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = sibling_name(
        name,
        &format!("{}.{}.{}.tmp", std::process::id(), stamp, sequence),
    )?;
    let mut file = parent.create_new(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
        #[cfg(not(unix))]
        let _ = unix_mode;
        file.sync_all()
    })();
    if let Err(error) = result {
        drop(file);
        let _ = parent.remove_file(&temp);
        return Err(error);
    }
    drop(file);
    if !parent.safe_file_or_absent(name)? {
        let _ = parent.remove_file(&temp);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed output path changed during write",
        ));
    }
    let result = rename_replacing_in(parent, &temp, name);
    if result.is_err() {
        let _ = parent.remove_file(&temp);
    }
    if result.is_ok() {
        parent.sync();
    }
    result
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (parent, name) = ManagedParent::open_for(path)?;
    atomic_write_in(&parent, &name, bytes, None)
}

/// Open a newly created private regular file without following links or
/// replacing an existing path. The returned handle is identity-checked against
/// the directory entry; callers that stream content must sync it before publish.
pub fn create_new_private(path: &Path) -> std::io::Result<File> {
    let (parent, name) = ManagedParent::open_for(path)?;
    parent.create_new(&name)
}

/// Creates a new private regular file without following links or replacing an
/// existing path. Callers should assemble non-streaming output before calling
/// this function so a failure cannot expose a logically partial artifact.
pub fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use cap_fs_ext::MetadataExt as _;

    let (parent, name) = ManagedParent::open_for(path)?;
    let mut file = parent.create_new(&name)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = parent.remove_file(&name);
        return Err(error);
    }
    let metadata = parent.capability.symlink_metadata(&name)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "new managed file changed identity while writing",
        ));
    }
    drop(file);
    parent.sync();
    Ok(())
}

fn publish_new_private_in(
    parent: &ManagedParent,
    name: &OsStr,
    bytes: &[u8],
) -> std::io::Result<()> {
    match parent.file_state(name)? {
        None => {}
        Some(true) => return Err(std::io::ErrorKind::AlreadyExists.into()),
        Some(false) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed publication target is not a regular file",
            ));
        }
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = sibling_name(
        name,
        &format!("publish.{}.{}.{}.tmp", std::process::id(), stamp, sequence),
    )?;
    let mut file = parent.create_new(&temp)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = parent.remove_file(&temp);
        return Err(error);
    }
    drop(file);
    let published = parent.hard_link(&temp, name);
    let _ = parent.remove_file(&temp);
    if published.is_ok() {
        parent.sync();
    }
    published
}

/// Publish a complete private regular file without replacing an existing
/// entry. Bytes are assembled in a private sibling and become visible through
/// one no-replace hard-link operation, so a crash cannot expose a partial
/// destination file.
pub fn publish_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (parent, name) = ManagedParent::open_for(path)?;
    publish_new_private_in(&parent, &name, bytes)
}

/// Root-anchored variant of [`publish_new_private`]. All descendant directory
/// components are opened without following links and remain capability-pinned
/// through publication.
pub fn publish_new_private_beneath(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    create_parents: bool,
) -> std::io::Result<()> {
    let (parent, name) = parent_beneath(root, relative, create_parents)?;
    publish_new_private_in(&parent, &name, bytes)
}

/// Open a persistent private regular file suitable for an OS advisory lock.
/// Existing symlinks, reparse points, directories, and special files are
/// rejected; a newly created file is private on Unix.
pub fn open_private_lock(path: &Path) -> std::io::Result<File> {
    open_existing_or_new_no_follow(path, true, false, true)
}

/// Open or create a private advisory-lock file beneath a trusted root while
/// keeping every descendant directory capability-pinned.
pub fn open_private_lock_beneath(
    root: &Path,
    relative: &Path,
    create_parents: bool,
) -> std::io::Result<File> {
    let (parent, name) = parent_beneath(root, relative, create_parents)?;
    if !parent.safe_file_or_absent(&name)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed lock path",
        ));
    }
    parent.open_existing_or_new(&name, true, false, true, 0o600)
}

/// Open an existing private regular lock file without creating a missing path.
/// This is used by stale-lock recovery so merely probing an ownerless crash
/// directory cannot manufacture a lease and make it look live.
pub fn open_private_existing_lock(path: &Path) -> std::io::Result<File> {
    open_existing_or_new_no_follow(path, true, false, false)
}

/// Open or create a project-visible regular file for append without following
/// links. The parent directory remains capability-anchored until the handle is
/// open; new files use ordinary umask-controlled permissions.
pub fn open_regular_append(path: &Path) -> std::io::Result<File> {
    let (parent, name) = ManagedParent::open_for(path)?;
    if !parent.safe_file_or_absent(&name)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed append path",
        ));
    }
    parent.open_child(&name, false, true, true, false, 0o666)
}

/// Root-anchored variant of [`open_regular_append`].
pub fn open_regular_append_beneath(
    root: &Path,
    relative: &Path,
    create_parents: bool,
) -> std::io::Result<File> {
    let (parent, name) = parent_beneath(root, relative, create_parents)?;
    if !parent.safe_file_or_absent(&name)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed append path",
        ));
    }
    parent.open_existing_or_new(&name, false, true, true, 0o666)
}

/// Append one assembled byte slice to a private regular file without following
/// a leaf link. Callers that need records to remain indivisible across partial
/// writes must hold an inter-process lock for the duration of this call.
pub fn append_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = open_existing_or_new_no_follow(path, false, true, true)?;
    file.write_all(bytes)
}

/// Append one assembled byte slice and flush it to durable storage before
/// returning. Callers must hold the appropriate inter-process store lock when
/// multiple writers may target the same log.
pub fn append_private_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = open_existing_or_new_no_follow(path, false, true, true)?;
    file.write_all(bytes)?;
    file.sync_data()
}

/// Open the current readable contents of a managed file WITHOUT mutating the
/// namespace, tolerating an in-progress [`atomic_write`] replacement window.
///
/// On Windows `atomic_write` replaces a value in two steps: it first moves the
/// previous bytes to a `pending` sibling, then moves the replacement onto
/// `target`. During that gap `target` is briefly absent while `pending` still
/// holds the previous, still-committed value. A reader must fall back to
/// `pending` when `target` is absent, but it must NEVER rename `pending` into
/// place: readers do not hold the store lock, so such a rename could replace a
/// value a concurrent writer already committed to `target`, silently reverting
/// it (a data-integrity and privacy defect). The authoritative rename-recovery
/// is left to the next writer under the lock ([`recover_pending`]).
///
/// At every instant at least one of `target`/`pending` exists while the logical
/// file exists, so a single re-check of `target` resolves the narrow case where
/// a writer finished (removing `pending`) between our two probes. Off Windows
/// `pending` is never produced, so the common path opens `target` directly and
/// the observable result is unchanged.
fn open_read_source_in(parent: &ManagedParent, name: &OsStr) -> std::io::Result<File> {
    match parent.open_read(name) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let pending = sibling_name(name, "umadev-replace-pending")?;
            match parent.open_read(&pending) {
                Ok(file) => Ok(file),
                Err(pending_error) if pending_error.kind() == std::io::ErrorKind::NotFound => {
                    // Either the file genuinely does not exist, or a writer
                    // published `target` and removed `pending` between the two
                    // probes above. Re-check `target` before reporting absence.
                    parent.open_read(name)
                }
                Err(pending_error) => Err(pending_error),
            }
        }
        Err(error) => Err(error),
    }
}

fn open_read_source(path: &Path) -> std::io::Result<File> {
    let (parent, name) = ManagedParent::open_for(path)?;
    open_read_source_in(&parent, &name)
}

fn read_bounded_stable_in(
    parent: &ManagedParent,
    name: &OsStr,
    file: &File,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    let opened_metadata = file.metadata()?;
    let length = opened_metadata.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(max_bytes))
            .unwrap_or(0)
            .min(64 * 1024),
    );
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes while being read"),
        ));
    }
    let after_metadata = file.metadata()?;

    // A Windows atomic replacement can legitimately make `target` absent
    // while the opened file is its pending sibling. Reopen with the same
    // source-selection rule, always through this pinned parent capability.
    let current = open_read_source_in(parent, name)?;
    let current_identity = same_file::Handle::from_file(current)?;
    let stable_metadata = opened_metadata.len() == after_metadata.len()
        && opened_metadata.modified().ok() == after_metadata.modified().ok()
        && after_metadata.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if opened != current_identity || !stable_metadata {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed input changed while it was being read",
        ));
    }
    Ok(bytes)
}

pub fn read_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = open_read_source(path)?;
    let length = file.metadata()?.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(max_bytes))
            .unwrap_or(0)
            .min(64 * 1024),
    );
    // Metadata is only a preflight: another writer can grow the already-open
    // file between `metadata()` and EOF. Read at most one byte beyond the
    // contract so the allocation and I/O remain bounded even in that race.
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes while being read"),
        ));
    }
    Ok(bytes)
}

/// Read a workspace-controlled regular file beneath `root` with a hard byte
/// ceiling and without following symlink/reparse components.
///
/// The relative path must contain only normal components. The trusted root and
/// every descendant directory remain open as capabilities while the next
/// component is resolved without following links. The leaf is opened with the
/// platform's no-follow flag (`O_NONBLOCK` is also used on Unix) and re-opened
/// through the same parent capability after the read to verify identity. A
/// concurrent grow is bounded by reading at most one byte beyond `max_bytes`.
pub fn read_bounded_beneath(
    root: &Path,
    relative: &Path,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};

    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed input contains an escaping path component",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed input path is empty",
        ));
    }

    // Resolve the caller-designated boundary once, then keep an open
    // capability for every descent. Namespace changes after the root is
    // opened can no longer redirect a later component lookup outside it.
    let canonical_root = fs::canonicalize(root)?;
    let (_root_file, mut directory) = open_managed_directory(&canonical_root)?;
    for part in &parts[..parts.len() - 1] {
        let metadata = directory.symlink_metadata(part)?;
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed input ancestor is not a real directory",
            ));
        }
        directory = directory.open_dir_nofollow(part)?;
    }

    let leaf = parts.last().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed input path is empty",
        )
    })?;
    let metadata = directory.symlink_metadata(leaf)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed input is not a regular non-link file",
        ));
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = directory.open_with(leaf, &options)?.into_std();
    let opened = same_file::Handle::from_file(file.try_clone()?)?;

    let opened_metadata = file.metadata()?;
    let length = opened_metadata.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(max_bytes))
            .unwrap_or(0)
            .min(64 * 1024),
    );
    (&file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes while being read"),
        ));
    }
    let after_metadata = file.metadata()?;
    let current_file = directory.open_with(leaf, &options)?.into_std();
    let current = same_file::Handle::from_file(current_file)?;
    let stable_metadata = opened_metadata.len() == after_metadata.len()
        && opened_metadata.modified().ok() == after_metadata.modified().ok()
        && after_metadata.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if opened != current || !stable_metadata {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed input changed while it was being read",
        ));
    }
    Ok(bytes)
}

pub fn remove_regular_file(path: &Path) -> std::io::Result<bool> {
    let (parent, name) = ManagedParent::open_for(path)?;
    recover_pending_in(&parent, &name)?;
    match parent.file_state(&name)? {
        Some(true) => {
            parent.remove_file(&name)?;
            Ok(true)
        }
        Some(false) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a non-regular managed path",
        )),
        None => Ok(false),
    }
}

/// Remove a non-directory leaf beneath a trusted root without following it or
/// any descendant directory component. A symbolic link/reparse leaf is removed
/// as an entry; its target is never touched.
pub fn remove_file_entry_beneath(root: &Path, relative: &Path) -> std::io::Result<bool> {
    let (parent, name) = parent_beneath(root, relative, false)?;
    match parent.capability.symlink_metadata(&name) {
        Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a managed directory as a file",
        )),
        Ok(_) => {
            parent.remove_file_or_link(&name)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Remove an empty managed directory without following a link or mount-like
/// reparse point. Windows transient sharing conflicts are retried briefly.
pub fn remove_empty_dir(path: &Path) -> std::io::Result<bool> {
    let (parent, name) = ManagedParent::open_for(path)?;
    match parent.directory_state(&name)? {
        Some(true) => {
            parent.remove_dir(&name)?;
            Ok(true)
        }
        Some(false) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a non-directory managed path",
        )),
        None => Ok(false),
    }
}

/// Create one directory without accepting an existing entry. Windows transient
/// sharing conflicts are retried for a bounded interval.
pub fn create_dir(path: &Path) -> std::io::Result<()> {
    let (parent, name) = ManagedParent::open_for(path)?;
    parent.create_dir(&name)
}

/// Rename a managed path, retrying transient Windows sharing violations so the
/// durable-store family (lock directories, ledger rotation, tombstone
/// publication) shares one Windows robustness level.
///
/// Operands are not validated for links or reparse points; callers that need
/// that must check beforehand, exactly as the internal atomic-write path does.
/// Fail-open: it never widens the outcome of a genuine, non-transient failure.
pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from_parent, from_name) = ManagedParent::open_for(from)?;
    let (to_parent, to_name) = ManagedParent::open_for(to)?;
    from_parent.rename_to(&from_name, &to_parent, &to_name)
}

/// Atomically replace a regular file with another regular file staged in the
/// same real directory. This gives callers outside this module the same safe
/// Windows two-phase replacement and crash recovery used by [`atomic_write`].
pub fn replace_regular_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let from_parent = from.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source file has no parent",
        )
    })?;
    let to_parent = to.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement file has no parent",
        )
    })?;
    if from_parent != to_parent {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed regular-file replacement",
        ));
    }
    let (parent, to_name) = ManagedParent::open_for(to)?;
    let from_name = from.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source file has no name")
    })?;
    validate_child_name(from_name)?;
    if parent.file_state(from_name)? != Some(true) || !parent.safe_file_or_absent(&to_name)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed regular-file replacement",
        ));
    }
    recover_pending_in(&parent, &to_name)?;
    rename_replacing_in(&parent, from_name, &to_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_atomic_writes_replace_the_same_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(read_bounded(&path, 16).unwrap(), b"two");
    }

    #[test]
    fn rooted_private_publication_is_complete_and_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let relative = Path::new(".umadev/events/one.json");
        publish_new_private_beneath(temp.path(), relative, b"complete", true).unwrap();
        assert_eq!(
            read_bounded_beneath(temp.path(), relative, 64).unwrap(),
            b"complete"
        );
        assert_eq!(
            publish_new_private_beneath(temp.path(), relative, b"replacement", true)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            read_bounded_beneath(temp.path(), relative, 64).unwrap(),
            b"complete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rooted_mutations_reject_linked_ancestors() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep.json"), b"keep").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();

        assert!(publish_new_private_beneath(
            root.path(),
            Path::new("linked/new.json"),
            b"outside",
            true,
        )
        .is_err());
        assert!(open_private_lock_beneath(root.path(), Path::new("linked/lock"), true,).is_err());
        assert!(remove_file_entry_beneath(root.path(), Path::new("linked/keep.json")).is_err());
        assert_eq!(fs::read(outside.path().join("keep.json")).unwrap(), b"keep");
        assert!(!outside.path().join("new.json").exists());
        assert!(!outside.path().join("lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_parent_operations_stay_on_the_open_directory_after_path_replacement() {
        use std::io::Write as _;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let managed = root.path().join("managed");
        let moved = root.path().join("moved");
        fs::create_dir(&managed).unwrap();
        let target = managed.join("audit.jsonl");
        let (parent, name) = ManagedParent::open_for(&target).unwrap();

        fs::rename(&managed, &moved).unwrap();
        symlink(outside.path(), &managed).unwrap();

        let mut file = parent
            .open_existing_or_new(&name, false, true, true, 0o600)
            .unwrap();
        file.write_all(b"private\n").unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(fs::read(moved.join("audit.jsonl")).unwrap(), b"private\n");
        assert!(!outside.path().join("audit.jsonl").exists());

        let staged = OsStr::new(".state.tmp");
        let mut file = parent.create_new(staged).unwrap();
        file.write_all(b"new").unwrap();
        file.sync_all().unwrap();
        drop(file);
        rename_replacing_in(&parent, staged, OsStr::new("state.json")).unwrap();
        assert_eq!(fs::read(moved.join("state.json")).unwrap(), b"new");
        assert!(!outside.path().join("state.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rooted_handle_never_mutates_a_replacement_root() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let container = tempfile::tempdir().unwrap();
        let managed = container.path().join("managed");
        let moved = container.path().join("moved");
        let outside = container.path().join("outside");
        fs::create_dir(&managed).unwrap();
        fs::create_dir(&outside).unwrap();
        for directory in [&managed, &outside] {
            fs::write(directory.join("write.txt"), b"keep").unwrap();
            fs::write(directory.join("delete.txt"), b"keep").unwrap();
            fs::write(directory.join("mode.txt"), b"keep").unwrap();
            fs::set_permissions(
                directory.join("mode.txt"),
                fs::Permissions::from_mode(0o640),
            )
            .unwrap();
        }

        let rooted = RootedDir::open(&managed).unwrap();
        fs::rename(&managed, &moved).unwrap();
        symlink(&outside, &managed).unwrap();

        rooted
            .atomic_write(Path::new("write.txt"), b"inside", false)
            .unwrap();
        rooted.remove_regular_file(Path::new("delete.txt")).unwrap();
        rooted
            .atomic_write_with_unix_mode(Path::new("mode.txt"), b"inside-mode", false, 0o700)
            .unwrap();

        assert_eq!(fs::read(outside.join("write.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(outside.join("delete.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(outside.join("mode.txt")).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(outside.join("mode.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(fs::read(moved.join("write.txt")).unwrap(), b"inside");
        assert!(!moved.join("delete.txt").exists());
        assert_eq!(
            fs::metadata(moved.join("mode.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn rooted_read_publish_lock_append_and_listing_survive_root_replacement() {
        use std::io::Write as _;
        use std::os::unix::fs::symlink;

        let container = tempfile::tempdir().unwrap();
        let managed = container.path().join("managed");
        let moved = container.path().join("moved");
        let outside = container.path().join("outside");
        fs::create_dir(&managed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(managed.join("records")).unwrap();
        fs::create_dir(outside.join("records")).unwrap();
        fs::write(managed.join("records/input.txt"), b"inside").unwrap();
        fs::write(outside.join("records/input.txt"), b"outside").unwrap();
        let rooted = RootedDir::open(&managed).unwrap();
        assert!(rooted.matches_path(&managed).unwrap());
        assert!(!format!("{rooted:?}").contains(managed.to_string_lossy().as_ref()));

        fs::rename(&managed, &moved).unwrap();
        symlink(&outside, &managed).unwrap();
        assert!(!rooted.matches_path(&managed).unwrap());
        assert_eq!(
            rooted
                .read_bounded(Path::new("records/input.txt"), 64)
                .unwrap(),
            b"inside"
        );
        let input_proof = rooted
            .read_bounded_verified(Path::new("records/input.txt"), 64)
            .unwrap();
        rooted
            .publish_new_private(Path::new("records/published.txt"), b"published", false)
            .unwrap();
        let _lock = rooted
            .open_private_lock(Path::new("records/lease"), false)
            .unwrap();
        let mut log = rooted
            .open_regular_append(Path::new("records/events.log"), false)
            .unwrap();
        log.write_all(b"row\n").unwrap();
        drop(log);

        let files = rooted.list_regular_files(Path::new("records"), 16).unwrap();
        assert!(files.iter().any(|file| file.name == "published.txt"));
        let tree = rooted
            .list_regular_tree(Path::new("records"), 4, 32, 16, 1_024)
            .unwrap();
        assert!(tree
            .iter()
            .any(|file| file.relative == Path::new("records/published.txt")));
        assert!(rooted
            .remove_regular_file_if_unchanged(Path::new("records/input.txt"), &input_proof)
            .unwrap());
        assert_eq!(
            fs::read(outside.join("records/input.txt")).unwrap(),
            b"outside"
        );
        assert!(!outside.join("records/published.txt").exists());
        assert!(!outside.join("records/lease").exists());
        assert!(!outside.join("records/events.log").exists());
        assert_eq!(
            fs::read(moved.join("records/published.txt")).unwrap(),
            b"published"
        );
        assert_eq!(
            fs::read(moved.join("records/events.log")).unwrap(),
            b"row\n"
        );
        assert!(!moved.join("records/input.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rooted_conditional_unlink_rejects_a_replaced_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("value.bin");
        fs::write(&path, b"original").unwrap();
        let rooted = RootedDir::open(temp.path()).unwrap();
        let proof = rooted
            .read_bounded_verified(Path::new("value.bin"), 64)
            .unwrap();
        fs::remove_file(&path).unwrap();
        // Identical bytes are insufficient: a new filesystem identity must
        // never be unlinked using an older proof.
        fs::write(&path, b"original").unwrap();
        assert_eq!(
            rooted
                .remove_regular_file_if_unchanged(Path::new("value.bin"), &proof)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[cfg(windows)]
    #[test]
    fn managed_parent_handle_prevents_directory_replacement() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("ancestor");
        let managed = ancestor.join("managed");
        fs::create_dir_all(&managed).unwrap();
        let target = managed.join("state.json");
        let (_parent, _name) = ManagedParent::open_for(&target).unwrap();
        assert!(fs::rename(&managed, root.path().join("moved")).is_err());
        assert!(fs::rename(&ancestor, root.path().join("moved-ancestor")).is_err());
    }

    #[test]
    fn staged_regular_file_can_replace_a_regular_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("state.json");
        let staged = temp.path().join("state.next");
        fs::write(&target, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        replace_regular_file(&staged, &target).unwrap();
        assert_eq!(read_bounded(&target, 16).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn bounded_read_rejects_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"oversized").unwrap();
        assert_eq!(
            read_bounded(&path, 3).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rooted_reads_reject_hard_linked_inputs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("value.json"), b"private").unwrap();
        fs::hard_link(
            temp.path().join("value.json"),
            temp.path().join("alias.json"),
        )
        .unwrap();
        let root = RootedDir::open_no_follow(temp.path()).unwrap();
        assert!(root.read_bounded(Path::new("value.json"), 64).is_err());
        assert!(root.read_prefix(Path::new("alias.json"), 4).is_err());
    }

    #[test]
    fn bounded_beneath_read_rejects_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("state")).unwrap();
        fs::write(temp.path().join("state/value.json"), b"oversized").unwrap();
        assert_eq!(
            read_bounded_beneath(temp.path(), Path::new("state/value.json"), 3)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_beneath_read_rejects_linked_ancestors_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("value.json"), b"outside").unwrap();
        symlink(outside.path(), temp.path().join("linked")).unwrap();
        assert!(read_bounded_beneath(temp.path(), Path::new("linked/value.json"), 64).is_err());

        let fifo = temp.path().join("input.fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        assert!(read_bounded_beneath(temp.path(), Path::new("input.fifo"), 64).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a FIFO must be rejected before a blocking read"
        );
    }

    #[test]
    fn private_create_never_replaces_an_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("export.zip");
        write_new_private(&path, b"one").unwrap();
        assert_eq!(
            write_new_private(&path, b"two").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(read_bounded(&path, 16).unwrap(), b"one");
    }

    #[cfg(unix)]
    #[test]
    fn streaming_private_create_uses_private_permissions() {
        use std::io::Write as _;
        use std::os::unix::fs::MetadataExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stream.zip");
        let mut file = create_new_private(&path).unwrap();
        file.write_all(b"private").unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(fs::metadata(path).unwrap().mode() & 0o077, 0);
    }

    #[cfg(unix)]
    #[test]
    fn private_lock_and_append_never_follow_leaf_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"keep").unwrap();

        let lock_link = temp.path().join("lock");
        symlink(&outside, &lock_link).unwrap();
        assert!(open_private_lock(&lock_link).is_err());

        let log_link = temp.path().join("audit.jsonl");
        symlink(&outside, &log_link).unwrap();
        assert!(append_private(&log_link, b"secret\n").is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");

        let hard_link = temp.path().join("hard-linked-audit.jsonl");
        fs::hard_link(&outside, &hard_link).unwrap();
        assert!(append_private(&hard_link, b"secret\n").is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    fn transient_windows_file_conflicts_are_retried_but_other_errors_are_not() {
        let mut transient_attempts = 0;
        retry_transient_windows_fs(|| {
            transient_attempts += 1;
            if transient_attempts < 3 {
                Err(std::io::Error::from_raw_os_error(32))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(transient_attempts, 3);

        let mut permanent_attempts = 0;
        let error = retry_transient_windows_fs(|| {
            permanent_attempts += 1;
            Err::<(), _>(std::io::Error::from_raw_os_error(87))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(87));
        assert_eq!(permanent_attempts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_read_replaced_or_removed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let link = temp.path().join("state.json");
        symlink(&outside, &link).unwrap();
        assert!(atomic_write(&link, b"replace").is_err());
        assert!(read_bounded(&link, 64).is_err());
        assert!(remove_regular_file(&link).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn removals_never_follow_a_linked_parent_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("state.json");
        let outside_dir = outside.path().join("empty");
        fs::write(&outside_file, "keep").unwrap();
        fs::create_dir(&outside_dir).unwrap();
        symlink(outside.path(), root.path().join("state")).unwrap();

        assert!(remove_regular_file(&root.path().join("state/state.json")).is_err());
        assert!(remove_empty_dir(&root.path().join("state/empty")).is_err());
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "keep");
        assert!(outside_dir.is_dir());
    }

    #[test]
    fn read_in_replace_window_reads_pending_without_mutating_the_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"old").unwrap();

        // Reproduce the Windows two-phase replace window by hand: the previous
        // bytes are parked in `pending` while `target` is momentarily absent.
        let pending = pending_path(&path).unwrap();
        fs::rename(&path, &pending).unwrap();
        assert!(!path.exists());
        assert!(real_file(&pending));

        // A reader must return the still-committed previous value from
        // `pending` WITHOUT renaming it into place. The buggy read path used to
        // run the mutating recovery here and recreate `target`, which is what
        // let an unlocked reader clobber a concurrent writer's committed bytes.
        assert_eq!(read_bounded(&path, 64).unwrap(), b"old");
        assert!(!path.exists(), "a read must not recreate target");
        assert!(real_file(&pending), "a read must leave pending untouched");

        // The next writer performs the real recovery under its lock; its new
        // value wins and the earlier read never reverted anything.
        atomic_write(&path, b"new").unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        assert!(!pending.exists());
    }

    #[test]
    fn read_prefers_target_over_a_leftover_pending_sibling() {
        // Both `target` (NEW) and a stale `pending` (OLD) present: a reader must
        // return the authoritative `target`, never the parked previous value,
        // and must not disturb either file.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"new").unwrap();
        let pending = pending_path(&path).unwrap();
        fs::write(&pending, b"old").unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        assert!(real_file(&path));
        assert!(real_file(&pending));
    }

    #[test]
    fn a_read_in_the_replace_window_cannot_revert_a_committed_write() {
        use std::sync::{Arc, Barrier};

        // The reported race, exercised on every platform: a reader arrives while
        // (target absent, pending=OLD), a writer commits NEW to target, and the
        // reader must never restore OLD over the committed NEW. Deterministically
        // correct for the fixed read path; a reintroduced clobber would surface
        // here as an intermittent reverted value.
        for _ in 0..64 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state.json");
            atomic_write(&path, b"old").unwrap();
            let pending = pending_path(&path).unwrap();

            // Enter the mid-replace window by hand (Unix never produces it).
            fs::rename(&path, &pending).unwrap();

            let gate = Arc::new(Barrier::new(2));
            let reader_gate = Arc::clone(&gate);
            let reader_path = path.clone();
            let reader = std::thread::spawn(move || {
                reader_gate.wait();
                if let Ok(bytes) = read_bounded(&reader_path, 64) {
                    assert!(bytes == b"old" || bytes == b"new", "reverted/torn read");
                }
            });

            gate.wait();
            // Commit NEW: publish it at target, then drop the previous sibling.
            let staged = path.with_file_name(".state.json.staged");
            fs::write(&staged, b"new").unwrap();
            fs::rename(&staged, &path).unwrap();
            let _ = fs::remove_file(&pending);
            reader.join().unwrap();

            // The committed write survives; the read never reverted it.
            assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_two_phase_replace_survives_concurrent_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Drive the real Windows `atomic_write` two-phase replace against a
        // hammering reader: every observation is a complete OLD/NEW value and
        // the final committed write is never reverted.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"old").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = read_bounded(&reader_path, 64) {
                    assert!(bytes == b"old" || bytes == b"new", "reverted/torn read");
                }
            }
        });
        for _ in 0..500 {
            atomic_write(&path, b"new").unwrap();
            atomic_write(&path, b"old").unwrap();
        }
        atomic_write(&path, b"new").unwrap();
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
    }
}
