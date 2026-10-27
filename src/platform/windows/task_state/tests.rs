use super::*;
use std::io::{Read, Write};
use windows_sys::Win32::Security::{SetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION};

struct Sandbox(std::path::PathBuf);
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-task-acl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn make_permissive(path: &Path, directory: bool) {
    let flags = if directory { "OICI" } else { "" };
    let sddl = U16CString::from_str(format!("D:P(A;{flags};FA;;;WD)")).unwrap();
    let descriptor = SecurityDescriptor::deserialize(&sddl).unwrap();
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let path = super::super::extended_length_path(path).unwrap();
    assert_ne!(
        unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                attributes.lpSecurityDescriptor,
            )
        },
        0
    );
}

#[test]
fn task_windows_existing_permissive_leaf_is_rejected_without_acl_repair() {
    let sandbox = Sandbox::new();
    let parent = sandbox.0.join("tasks-v1");
    ensure_private_task_directory(&parent).unwrap();
    let leaf = parent.join("board");
    ensure_private_task_directory(&leaf).unwrap();
    make_permissive(&leaf, true);
    assert_eq!(
        ensure_private_task_directory(&leaf).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        ensure_private_task_directory(&leaf).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn task_windows_private_create_replace_and_existing_permissive_file_rejection() {
    let sandbox = Sandbox::new();
    let leaf = sandbox.0.join("board");
    ensure_private_task_directory(&leaf).unwrap();
    ensure_private_task_directory(&leaf).unwrap();
    let destination = leaf.join("store.json");
    let mut file = create_private_task_file(&destination).unwrap();
    file.write_all(b"original").unwrap();
    drop(file);
    let temporary = leaf.join("temporary");
    let mut file = create_private_task_file(&temporary).unwrap();
    file.write_all(b"replacement").unwrap();
    file.sync_all().unwrap();
    drop(file);
    super::super::replace_file(&temporary, &destination).unwrap();
    let mut contents = String::new();
    open_private_task_file(&destination, false)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    assert_eq!(contents, "replacement");
    make_permissive(&destination, false);
    // A private directory does not make a restored permissive child private.
    assert_eq!(
        open_private_task_file(&destination, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        open_private_task_file(&destination, true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"replacement");
}

#[test]
fn task_windows_pipe_endpoint_identity_retains_namespaced_spelling() {
    let first = Path::new(r"C:\synthetic\link\..\api.sock");
    let second = Path::new(r"C:\synthetic\api.sock");
    assert_eq!(super::super::task_endpoint_identity(first).unwrap(), first);
    assert_ne!(
        super::super::task_endpoint_identity(first).unwrap(),
        super::super::task_endpoint_identity(second).unwrap()
    );
    assert!(super::super::task_endpoint_identity(Path::new("")).is_err());
    assert!(super::super::task_endpoint_identity(Path::new("invalid\0name")).is_err());
}
