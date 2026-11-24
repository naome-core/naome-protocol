//! Create the seed with a protected DACL before any key bytes are written.
use std::{
    ffi::c_void,
    fs::File,
    io::{self, Write},
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, GetTokenInformation,
        InitializeAcl, InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
    },
    System::{
        SystemServices::{FILE_PERSISTENT_ACLS, SECURITY_DESCRIPTOR_REVISION},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

fn checked(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null in key path",
        ));
    }
    name.push(0);
    // Pointer-aligned storage for the variable-size token and its SID, and the one-entry ACL.
    const SID: usize = SECURITY_MAX_SID_SIZE as usize;
    let mut user = [0usize; (size_of::<TOKEN_USER>() + SID).div_ceil(size_of::<usize>())];
    let mut acl = [0usize;
        (size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() + SID).div_ceil(size_of::<usize>())];
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    // SAFETY: Each buffer remains live through CreateFileW. All pointers returned by
    // GetTokenInformation are read only after success. OwnedHandle and File own the handles.
    unsafe {
        let mut token = null_mut();
        checked(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        let token = OwnedHandle::from_raw_handle(token);
        let mut needed = 0;
        checked(GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            user.as_mut_ptr().cast(),
            size_of_val(&user) as u32,
            &mut needed,
        ))?;
        let sid = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let acl_ptr = acl.as_mut_ptr().cast();
        checked(InitializeAcl(
            acl_ptr,
            size_of_val(&acl) as u32,
            ACL_REVISION,
        ))?;
        checked(AddAccessAllowedAce(
            acl_ptr,
            ACL_REVISION,
            FILE_ALL_ACCESS,
            sid,
        ))?;
        let descriptor_ptr = (&raw mut descriptor).cast::<c_void>();
        checked(InitializeSecurityDescriptor(
            descriptor_ptr,
            SECURITY_DESCRIPTOR_REVISION,
        ))?;
        checked(SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0))?;
        checked(SetSecurityDescriptorControl(
            descriptor_ptr,
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        ))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        let handle = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

pub(super) fn publish_seed(path: &Path, seed: &[u8; 32]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::Builder::new().make_in(parent.canonicalize()?, create_private)?;
    let mut flags = 0;
    // SAFETY: The file owns the handle; flags is the only requested output.
    unsafe {
        checked(GetVolumeInformationByHandleW(
            staged.as_file().as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        ))?;
    }
    anyhow::ensure!(
        flags & FILE_PERSISTENT_ACLS != 0,
        "key storage must support persistent access controls"
    );
    staged.write_all(seed)?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(path)?;
    Ok(())
}
