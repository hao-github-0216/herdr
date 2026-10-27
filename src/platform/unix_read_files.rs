//! Handle-relative, no-follow reads. O_NONBLOCK prevents a raced-in FIFO from
//! blocking before fstat rejects it. Descendant components never follow symlinks.
use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};

use super::ReadDirectoryEntry;

fn open(root: &Path, path: &Path, directory: bool) -> io::Result<File> {
    let root = root.canonicalize()?;
    let root_name = CString::new("/")?;
    // SAFETY: NUL-terminated name and valid flags; ownership is transferred below.
    let fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a newly owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    // Walk the canonical root too: a concurrently replaced ancestor must not
    // redirect the root open through a symlink between canonicalize and open.
    let components = root
        .components()
        .filter(|part| *part != Component::RootDir)
        .chain(path.components())
        .collect::<Vec<_>>();
    for (i, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relative normal path required",
            ));
        };
        let name = CString::new(name.as_bytes())?;
        if !directory && i + 1 == components.len() {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: live parent/name and writable storage; do not follow links.
            let result = unsafe {
                libc::fstatat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fstatat initialized the structure.
            if unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "only regular files can be previewed",
                ));
            }
        }
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory || i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: parent descriptor remains live and name is NUL-terminated.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a newly owned descriptor.
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

pub(crate) fn read_confined_file(root: &Path, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = open(root, path, false)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only regular files can be previewed",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn list_confined_directory(
    root: &Path,
    path: &Path,
    limit: usize,
) -> io::Result<(Vec<ReadDirectoryEntry>, bool)> {
    let file = open(root, path, true)?;
    // SAFETY: duplicate ownership for fdopendir; file retains its own descriptor.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is owned and refers to a directory.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed and did not take ownership.
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let dir = Directory(dir);
    let mut entries = Vec::new();
    let mut visited = 0;
    loop {
        // SAFETY: the directory stream is live and used by only this thread.
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            return Ok((entries, false));
        }
        // SAFETY: readdir's name is NUL-terminated and valid until its next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if visited == limit {
            return Ok((entries, true));
        }
        visited += 1;
        let name_c = CString::new(name)?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid parent fd/name and writable stat storage. Do not follow
        // symlinks, including on filesystems whose dirent type is DT_UNKNOWN.
        let result = unsafe {
            libc::fstatat(
                file.as_raw_fd(),
                name_c.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            continue;
        } // Entry disappeared while listing.
        let kind = unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT;
        entries.push(ReadDirectoryEntry {
            name: OsString::from_vec(name.to_vec()),
            directory: kind == libc::S_IFDIR,
            symlink: kind == libc::S_IFLNK,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn confined_reads_reject_fifo_directory_symlink_and_permissions() {
        let root = std::env::temp_dir().join(format!("herdr-secure-file-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: valid pathname and mode, isolated fixture directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_confined_file(&root, Path::new("fifo"), 16).is_err());
        assert!(read_confined_file(&root, Path::new(""), 16).is_err());
        std::fs::write(root.join("private"), "fixture").unwrap();
        std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o0))
            .unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(read_confined_file(&root, Path::new("private"), 16).is_err());
        }
        std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::os::unix::fs::symlink("private", root.join("link")).unwrap();
        assert!(read_confined_file(&root, Path::new("link"), 16).is_err());
        let (entries, _) = list_confined_directory(&root, Path::new(""), 16).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.name == "link" && !entry.directory && entry.symlink));
        std::fs::remove_dir_all(root).unwrap();
    }
}
