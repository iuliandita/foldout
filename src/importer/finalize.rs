//! Linux descriptor-relative operations. No source unlink, truncation, or permission change.
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::journal::{ImportError, ImportOperation, ImportPolicy, InternalImportRequest};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub dev: u64,
    pub ino: u64,
}
impl Identity {
    pub(crate) fn of(file: &File) -> Result<Self, ImportError> {
        let m = file.metadata()?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    fn check(&self, file: &File) -> Result<(), ImportError> {
        if *self == Self::of(file)? {
            Ok(())
        } else {
            Err(ImportError::Changed)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct IntentIdentity {
    pub source_root: PathBuf,
    pub destination_root: PathBuf,
    pub source_root_identity: Identity,
    pub destination_root_identity: Identity,
    pub destination_parent_identity: Identity,
    pub source: Identity,
    pub signature: String,
    pub size: i64,
    pub format: String,
}
impl IntentIdentity {
    pub(crate) fn destination_path(&self, request: &InternalImportRequest) -> PathBuf {
        self.destination_root.join(&request.destination_relative)
    }
}

pub(crate) fn relative(value: &str) -> Result<(), ImportError> {
    if value.is_empty()
        || value.contains('\0')
        || value
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c == ".library-imports")
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ImportError::UnsafePath);
    }
    Ok(())
}

fn cstring(value: &str) -> Result<CString, ImportError> {
    CString::new(value).map_err(|_| ImportError::UnsafePath)
}

fn open_at(parent: &File, name: &str, flags: i32, mode: u32) -> Result<File, ImportError> {
    let name = cstring(name)?;
    // The owned descriptor outlives openat; the returned fd is transferred exactly once.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn root(path: &Path) -> Result<File, ImportError> {
    if !path.is_absolute() || std::fs::canonicalize(path)? != path {
        return Err(ImportError::UnsafePath);
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_at(
                    &directory,
                    name.to_str().ok_or(ImportError::UnsafePath)?,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?
            }
            _ => return Err(ImportError::UnsafePath),
        }
    }
    check_location(&directory, path)?;
    Ok(directory)
}

fn check_location(file: &File, expected: &Path) -> Result<(), ImportError> {
    let actual = std::fs::canonicalize(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if actual == expected {
        Ok(())
    } else {
        Err(ImportError::UnsafePath)
    }
}

fn parent(root: &File, value: &str) -> Result<(File, String), ImportError> {
    relative(value)?;
    let mut parts: Vec<_> = value.split('/').collect();
    let name = parts.pop().ok_or(ImportError::UnsafePath)?.to_string();
    let mut directory = root.try_clone()?;
    for part in parts {
        directory = open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok((directory, name))
}

fn regular(parent: &File, name: &str) -> Result<File, ImportError> {
    let file = open_at(parent, name, libc::O_RDONLY, 0)?;
    if !file.metadata()?.is_file() {
        return Err(ImportError::UnsafePath);
    }
    Ok(file)
}

pub(crate) fn hash(file: &mut File) -> Result<(String, i64, i64), ImportError> {
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(ImportError::UnsafePath);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut count = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > before.len() {
            return Err(ImportError::Changed);
        }
        hash.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    if count != before.len()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(ImportError::Changed);
    }
    let size = count.try_into().map_err(|_| ImportError::Changed)?;
    let mtime = after
        .mtime()
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(after.mtime_nsec()))
        .ok_or(ImportError::Changed)?;
    Ok((
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
        size,
        mtime,
    ))
}

pub(crate) fn check_content(file: &mut File, intent: &IntentIdentity) -> Result<i64, ImportError> {
    let (signature, size, mtime) = hash(file)?;
    if signature != intent.signature || size != intent.size {
        return Err(ImportError::Changed);
    }
    Ok(mtime)
}

pub(crate) fn inspect_intent(
    source: PathBuf,
    destination: PathBuf,
    request: &InternalImportRequest,
) -> Result<IntentIdentity, ImportError> {
    let source_root = root(&source)?;
    let destination_root = root(&destination)?;
    let (source_parent, source_name) = parent(&source_root, &request.source_relative)?;
    let (destination_parent, destination_name) =
        parent(&destination_root, &request.destination_relative)?;
    check_location(
        &source_parent,
        source
            .join(&request.source_relative)
            .parent()
            .ok_or(ImportError::UnsafePath)?,
    )?;
    check_location(
        &destination_parent,
        destination
            .join(&request.destination_relative)
            .parent()
            .ok_or(ImportError::UnsafePath)?,
    )?;
    absent(&destination_parent, &destination_name)?;
    let mut source_file = regular(&source_parent, &source_name)?;
    let (signature, size, _) = hash(&mut source_file)?;
    let format = Path::new(&request.source_relative)
        .extension()
        .and_then(|s| s.to_str())
        .ok_or(ImportError::InvalidFormat)?
        .to_ascii_lowercase();
    if !matches!(format.as_str(), "cbz" | "cbr" | "pdf")
        || Path::new(&request.destination_relative)
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
            != Some(&format)
    {
        return Err(ImportError::InvalidFormat);
    }
    Ok(IntentIdentity {
        source_root: source,
        destination_root: destination,
        source_root_identity: Identity::of(&source_root)?,
        destination_root_identity: Identity::of(&destination_root)?,
        destination_parent_identity: Identity::of(&destination_parent)?,
        source: Identity::of(&source_file)?,
        signature,
        size,
        format,
    })
}

fn absent(parent: &File, name: &str) -> Result<(), ImportError> {
    let name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Err(ImportError::Conflict);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error.into())
    }
}

pub(crate) struct Destination {
    pub root: File,
    pub parent: File,
    pub name: String,
    pub staging: File,
    pub stage_name: String,
}

pub(crate) struct ImportLock(pub(crate) File);

impl Drop for ImportLock {
    fn drop(&mut self) {
        // A concurrently spawned decoder can inherit this open file description
        // until exec. Explicit unlock prevents that child from prolonging ownership.
        loop {
            if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                tracing::error!(%error, "import directory unlock failed");
                break;
            }
        }
    }
}

pub(crate) fn lock(operation: &ImportOperation) -> Result<ImportLock, ImportError> {
    let file = root(&operation.identity.destination_root)?;
    operation.identity.destination_root_identity.check(&file)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::WouldBlock {
            Err(ImportError::Busy)
        } else {
            Err(error.into())
        };
    }
    Ok(ImportLock(file))
}

pub(crate) fn destination(operation: &ImportOperation) -> Result<Destination, ImportError> {
    let root = root(&operation.identity.destination_root)?;
    operation.identity.destination_root_identity.check(&root)?;
    let (parent, name) = parent(&root, &operation.request.destination_relative)?;
    operation
        .identity
        .destination_parent_identity
        .check(&parent)?;
    check_location(
        &parent,
        operation
            .identity
            .destination_path(&operation.request)
            .parent()
            .ok_or(ImportError::UnsafePath)?,
    )?;
    let directory = cstring(".library-imports")?;
    if unsafe { libc::mkdirat(root.as_raw_fd(), directory.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let staging = open_at(
        &root,
        ".library-imports",
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let metadata = staging.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
        || metadata.dev() != parent.metadata()?.dev()
    {
        return Err(ImportError::UnsafePath);
    }
    root.sync_all()?;
    Ok(Destination {
        root,
        parent,
        name,
        staging,
        stage_name: format!("{}.tmp", operation.id),
    })
}

impl Destination {
    pub(crate) fn check(&self, operation: &ImportOperation) -> Result<(), ImportError> {
        check_location(&self.root, &operation.identity.destination_root)?;
        check_location(
            &self.parent,
            operation
                .identity
                .destination_path(&operation.request)
                .parent()
                .ok_or(ImportError::UnsafePath)?,
        )?;
        check_location(
            &self.staging,
            &operation.identity.destination_root.join(".library-imports"),
        )
    }
    pub(crate) fn stage_file(&self) -> Result<File, ImportError> {
        regular(&self.staging, &self.stage_name)
    }
    pub(crate) fn final_file(&self) -> Result<File, ImportError> {
        regular(&self.parent, &self.name)
    }

    pub(crate) fn publish(&self, operation: &ImportOperation) -> Result<(), ImportError> {
        self.check(operation)?;
        let mut stage = self.stage_file()?;
        operation
            .staged
            .as_ref()
            .ok_or(ImportError::InvalidJournal)?
            .check(&stage)?;
        check_content(&mut stage, &operation.identity)?;
        match link(&self.staging, &self.stage_name, &self.parent, &self.name) {
            Ok(()) => {}
            Err(ImportError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut destination = self.final_file().map_err(|_| ImportError::Conflict)?;
                if Identity::of(&destination)? != Identity::of(&stage)? {
                    return Err(ImportError::Conflict);
                }
                check_content(&mut destination, &operation.identity)?;
            }
            Err(error) => return Err(error),
        }
        stage.sync_all()?;
        self.parent.sync_all()?;
        self.staging.sync_all()?;
        self.check(operation)
    }

    pub(crate) fn verify_final(
        &self,
        operation: &ImportOperation,
    ) -> Result<(File, i64), ImportError> {
        self.check(operation)?;
        let mut file = self.final_file()?;
        operation
            .staged
            .as_ref()
            .ok_or(ImportError::InvalidJournal)?
            .check(&file)?;
        let mtime = check_content(&mut file, &operation.identity)?;
        Ok((file, mtime))
    }

    pub(crate) fn cleanup(&self, operation: &ImportOperation) -> Result<(), ImportError> {
        self.verify_final(operation)?;
        match self.stage_file() {
            Ok(stage) => {
                operation
                    .staged
                    .as_ref()
                    .ok_or(ImportError::InvalidJournal)?
                    .check(&stage)?;
                unlink(&self.staging, &self.stage_name)?;
            }
            Err(ImportError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.staging.sync_all()?;
        Ok(())
    }
}

fn link(from: &File, source: &str, to: &File, destination: &str) -> Result<(), ImportError> {
    let source = cstring(source)?;
    let destination = cstring(destination)?;
    if unsafe {
        libc::linkat(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            destination.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn unlink(parent: &File, name: &str) -> Result<(), ImportError> {
    let name = cstring(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(crate) fn stage(operation: &ImportOperation) -> Result<(Identity, &'static str), ImportError> {
    let dest = destination(operation)?;
    dest.check(operation)?;
    absent(&dest.parent, &dest.name)?;
    let source_root = root(&operation.identity.source_root)?;
    operation
        .identity
        .source_root_identity
        .check(&source_root)?;
    let (source_parent, source_name) = parent(&source_root, &operation.request.source_relative)?;
    check_location(
        &source_parent,
        operation
            .identity
            .source_root
            .join(&operation.request.source_relative)
            .parent()
            .ok_or(ImportError::UnsafePath)?,
    )?;
    let mut source = regular(&source_parent, &source_name)?;
    operation.identity.source.check(&source)?;
    check_content(&mut source, &operation.identity)?;
    let same_device = source.metadata()?.dev() == dest.staging.metadata()?.dev();
    let allow_fallback = matches!(
        operation.request.policy,
        ImportPolicy::Hardlink {
            fallback_to_copy: true
        }
    );
    let mut policy = match operation.request.policy {
        ImportPolicy::Copy => "copy",
        ImportPolicy::Hardlink { .. } if same_device => "hardlink",
        ImportPolicy::Hardlink {
            fallback_to_copy: true,
        } => "copy",
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        } => return Err(ImportError::CrossFilesystem),
    };
    // A crash can leave a complete or partial stage before the phase commit. Only our
    // UUID in the owner-only staging directory may be reconciled; never a final path.
    match dest.stage_file() {
        Ok(mut file) => {
            if policy == "hardlink" && operation.identity.source == Identity::of(&file)? {
                check_content(&mut file, &operation.identity)?;
                file.sync_all()?;
                dest.staging.sync_all()?;
                return Ok((Identity::of(&file)?, policy));
            }
            if policy == "hardlink" {
                if !allow_fallback {
                    return Err(ImportError::Changed);
                }
                // EXDEV can occur across bind mounts even when st_dev matches.
                // A prior fallback copy may have crashed before its phase commit.
                policy = "copy";
            }
            let metadata = file.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o600
                || metadata.nlink() != 1
            {
                return Err(ImportError::Conflict);
            }
            if check_content(&mut file, &operation.identity).is_ok() {
                file.sync_all()?;
                dest.staging.sync_all()?;
                return Ok((Identity::of(&file)?, policy));
            }
            unlink(&dest.staging, &dest.stage_name)?;
            dest.staging.sync_all()?;
        }
        Err(ImportError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if policy == "hardlink" {
        match link(
            &source_parent,
            &source_name,
            &dest.staging,
            &dest.stage_name,
        ) {
            Ok(()) => {}
            Err(ImportError::Io(error)) if error.raw_os_error() == Some(libc::EXDEV) => {
                if !allow_fallback {
                    return Err(ImportError::CrossFilesystem);
                }
                policy = "copy";
            }
            Err(error) => return Err(error),
        }
    }
    let mut staged = if policy == "hardlink" {
        let file = dest.stage_file()?;
        operation.identity.source.check(&file)?;
        file
    } else {
        let mut file = open_at(
            &dest.staging,
            &dest.stage_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        source.seek(SeekFrom::Start(0))?;
        // Bound a concurrently growing source to the size authorized by the intent.
        let copied = std::io::copy(
            &mut (&mut source).take(operation.identity.size as u64 + 1),
            &mut file,
        )?;
        if copied != operation.identity.size as u64 {
            return Err(ImportError::Changed);
        }
        file
    };
    check_content(&mut source, &operation.identity)?;
    let current = regular(&source_parent, &source_name)?;
    operation.identity.source.check(&current)?;
    check_content(&mut staged, &operation.identity)?;
    staged.sync_all()?;
    dest.staging.sync_all()?;
    dest.check(operation)?;
    Ok((Identity::of(&staged)?, policy))
}

pub(crate) fn verify_stage(operation: &ImportOperation) -> Result<File, ImportError> {
    let dest = destination(operation)?;
    dest.check(operation)?;
    let mut file = dest.stage_file()?;
    operation
        .staged
        .as_ref()
        .ok_or(ImportError::InvalidJournal)?
        .check(&file)?;
    check_content(&mut file, &operation.identity)?;
    Ok(file)
}
