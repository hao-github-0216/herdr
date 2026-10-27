//! Task-only, process-account private storage. Unknown existing ACLs are not repaired.
use crate::platform::client_state::{private_task_acl, TaskAclEntry};
use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt as _, SecurityDescriptor,
};
use std::{
    fs::File,
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr::{null, null_mut},
};
use widestring::{U16CStr, U16CString};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, ERROR_SUCCESS, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT},
        EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetTokenInformation, IsValidAcl, IsValidSecurityDescriptor, IsValidSid, IsWellKnownSid,
        TokenUser, WinCreatorOwnerRightsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "task storage requires a same-user owner and a recognized protected private DACL; left unchanged")
}

struct UserSid(Vec<usize>);
impl UserSid {
    fn current() -> io::Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0;
        unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed)
        };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }
    fn sid(&self) -> PSID {
        // Aligned TOKEN_USER buffer owns the trailing SID for the entire query.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
    fn descriptor(&self, directory: bool) -> io::Result<SecurityDescriptor> {
        let mut text = null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = unsafe { U16CStr::from_ptr_str(text) }.to_string_lossy();
        unsafe { LocalFree(text.cast()) };
        let flags = if directory { "OICI" } else { "" };
        // Explicit owner avoids an elevated token's Administrators default owner.
        let sddl = U16CString::from_str(format!(
            "O:{sid}D:P(A;{flags};FA;;;SY)(A;{flags};FA;;;{sid})"
        ))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        SecurityDescriptor::deserialize(&sddl)
    }
}

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn verify(handle: &File, directory: bool) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(denied());
    }
    let user = UserSid::current()?;
    let mut owner = null_mut();
    let mut acl: *mut ACL = null_mut();
    let mut sd = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let sd = Descriptor(sd);
    if sd.0.is_null()
        || owner.is_null()
        || unsafe { IsValidSecurityDescriptor(sd.0) } == 0
        || unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(owner, user.sid()) } == 0
    {
        return Err(denied());
    }
    let mut control = 0;
    let mut revision = 0;
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorControl(sd.0, &mut control, &mut revision) } == 0
        || unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
        || unsafe { IsValidAcl(acl) } == 0
    {
        return Err(denied());
    }
    let mut entries = Vec::new();
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = null_mut();
        if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 || ace.is_null() {
            return Err(denied());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Ordinary ACCESS_ALLOWED_ACE_TYPE is zero. Reject every other ACE family
        // before interpreting its layout, including deny and callback entries.
        if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
            return Err(denied());
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let sid = unsafe { ace.cast::<u8>().add(sid_offset) };
        let available = usize::from(header.AceSize) - sid_offset;
        // SID header is 8 bytes, followed by SubAuthorityCount DWORDs. Establish
        // bounds before invoking SID APIs, which assume a complete SID buffer.
        if available < 8 || 8 + usize::from(unsafe { *sid.add(1) }) * 4 > available {
            return Err(denied());
        }
        let sid = sid.cast();
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(denied());
        }
        let same_user = unsafe { EqualSid(sid, user.sid()) } != 0;
        let owner_rights = unsafe { IsWellKnownSid(sid, WinCreatorOwnerRightsSid) } != 0;
        let system = unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0;
        entries.push(TaskAclEntry {
            ordinary_allow: true,
            trusted: same_user || owner_rights || system,
            grants_user: same_user || owner_rights,
            full_control: allowed.Mask == GENERIC_ALL || allowed.Mask == FILE_ALL_ACCESS,
            flags: header.AceFlags,
        });
    }
    if !private_task_acl(true, control & SE_DACL_PROTECTED != 0, directory, &entries) {
        return Err(denied());
    }
    Ok(())
}

fn open_directory(path: &Path) -> io::Result<File> {
    let path = super::extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(crate) fn ensure_private_task_directory(path: &Path) -> io::Result<()> {
    let user = UserSid::current()?;
    let descriptor = user.descriptor(true)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let wide = super::extended_length_path(path)?;
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    // Same read-only check for fresh and existing leaves. No ACL rewrite.
    verify(&open_directory(path)?, true)
}

pub(crate) fn create_private_task_file(path: &Path) -> io::Result<File> {
    let user = UserSid::current()?;
    let descriptor = user.descriptor(false)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let wide = super::extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    // Establish privacy on this exact handle before any task bytes are written.
    verify(&file, false)?;
    Ok(file)
}

pub(crate) fn open_private_task_file(path: &Path, write: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    verify(&file, false)?;
    Ok(file)
}

#[cfg(test)]
mod tests;
