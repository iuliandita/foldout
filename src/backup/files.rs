use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

use sha2::{Digest, Sha256};

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "backup files must be private regular files and directories",
    )
}

pub(super) fn private_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

pub(super) fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(super) fn open_private(path: &Path) -> io::Result<File> {
    let expected = fs::symlink_metadata(path)?;
    if !expected.is_file() || expected.mode() & 0o777 != 0o600 {
        return Err(invalid());
    }
    let file = File::open(path)?;
    let actual = file.metadata()?;
    if expected.ino() != actual.ino() || expected.dev() != actual.dev() {
        return Err(invalid());
    }
    Ok(file)
}

pub(super) fn copy_private(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = open_private(source)?;
    let mut output = private_file(destination)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()
}

pub(super) fn digest(path: &Path) -> io::Result<String> {
    let mut file = open_private(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
