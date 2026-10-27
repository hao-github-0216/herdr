use std::path::Path;

#[cfg(not(windows))]
pub(crate) fn create_private_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::create_remote_ssh_config_file(path)
}

#[cfg(windows)]
pub(crate) fn create_private_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::windows::create_remote_ssh_config_file(path)
}

#[cfg(not(windows))]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    super::windows::replace_file(source, destination)
}

#[cfg(not(windows))]
pub(crate) fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    // replace_file uses MOVEFILE_WRITE_THROUGH on Windows.
    Ok(())
}

/// Task state is private even when the enclosing configuration directory is not.
pub(crate) fn ensure_private_state_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task directory must be a private, same-user directory (0700), not a symlink",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        super::windows::ensure_private_task_directory(path)
    }
}

pub(crate) fn create_private_task_file(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        super::windows::create_private_task_file(path)
    }
    #[cfg(not(windows))]
    {
        create_private_state_file(path)
    }
}

pub(crate) fn open_private_task_file(path: &Path, write: bool) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        super::windows::open_private_task_file(path, write)
    }
    #[cfg(not(windows))]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
    }
}

// A deliberately narrow recognized ACL family, not an effective-access evaluator.
// Unknown ACLs are rejected, never repaired. SYSTEM is trusted, other accounts/groups are not.
#[cfg(any(windows, test))]
#[derive(Clone, Copy)]
pub(super) struct TaskAclEntry {
    pub ordinary_allow: bool,
    pub trusted: bool,
    pub grants_user: bool,
    pub full_control: bool,
    pub flags: u8,
}

#[cfg(any(windows, test))]
pub(super) fn private_task_acl(
    owner_is_user: bool,
    protected: bool,
    directory: bool,
    entries: &[TaskAclEntry],
) -> bool {
    owner_is_user
        && protected
        && !entries.is_empty()
        && entries.iter().all(|ace| {
            ace.ordinary_allow
                && ace.trusted
                && ace.full_control
                && ace.flags == if directory { 0x03 } else { 0 }
        })
        && entries.iter().any(|ace| ace.grants_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_acl_policy_rejects_unproven_owner_acl_trustees_masks_and_inheritance() {
        let user = TaskAclEntry {
            ordinary_allow: true,
            trusted: true,
            grants_user: true,
            full_control: true,
            flags: 3,
        };
        assert!(private_task_acl(true, true, true, &[user]));
        assert!(!private_task_acl(false, true, true, &[user]));
        assert!(!private_task_acl(true, false, true, &[user]));
        assert!(!private_task_acl(true, true, true, &[]));
        for unsafe_ace in [
            TaskAclEntry {
                trusted: false,
                ..user
            },
            TaskAclEntry {
                ordinary_allow: false,
                ..user
            },
            TaskAclEntry {
                full_control: false,
                ..user
            },
            TaskAclEntry {
                flags: 0x0b,
                ..user
            }, // inherit-only is not effective owner access
            TaskAclEntry {
                flags: 0x13,
                ..user
            }, // unknown/inherited directory template
        ] {
            assert!(!private_task_acl(true, true, true, &[user, unsafe_ace]));
        }
        assert!(!private_task_acl(
            true,
            true,
            true,
            &[TaskAclEntry {
                grants_user: false,
                ..user
            }]
        ));
        let file = TaskAclEntry { flags: 0, ..user };
        assert!(private_task_acl(true, true, false, &[file]));
        assert!(!private_task_acl(true, true, false, &[user]));
        assert!(!private_task_acl(true, true, true, &[file]));
    }
}
