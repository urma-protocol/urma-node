use crate::{
    config,
    install::Error,
    platform::{Descriptor, Exposure, Paths, Sddl},
};
use std::{
    ffi::{OsStr, c_void},
    fs, io,
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LUID, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AdjustTokenPrivileges,
        Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT},
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, IsWellKnownSid, LUID_AND_ATTRIBUTES,
        LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
        SE_TAKE_OWNERSHIP_NAME, SetKernelObjectSecurity, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub struct Acl;

impl Acl {
    const OWNER: &'static str = "O:BA";
    const TRUSTED_INSTALLER: &'static str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    const BINARY: &'static str = "urma-node.exe";
    const WRITE_RIGHTS: u32 = 0x0000_0002
        | 0x0000_0004
        | 0x0000_0010
        | 0x0000_0100
        | 0x0001_0000
        | 0x0004_0000
        | 0x0008_0000
        | 0x1000_0000
        | 0x4000_0000;
    const ALLOWED: u32 = 0;
    const INHERIT_ONLY: u32 = 0x08;
    const NOT_ALL_ASSIGNED: i32 = 1300;
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

fn refuse(path: &Path, why: &str) -> Error {
    Error::Failed(format!("refusing {}: {why}", path.display()))
}

fn open(path: &Path, access: u32) -> Result<Handle, Error> {
    let name = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    match handle {
        INVALID_HANDLE_VALUE => Err(io::Error::last_os_error().into()),
        handle => Ok(Handle(handle)),
    }
}

fn information(handle: &Handle) -> Result<BY_HANDLE_FILE_INFORMATION, Error> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    match unsafe { GetFileInformationByHandle(handle.0, &mut info) } {
        0 => Err(io::Error::last_os_error().into()),
        _ => Ok(info),
    }
}

fn plain(path: &Path, info: &BY_HANDLE_FILE_INFORMATION) -> Result<bool, Error> {
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(refuse(path, "it is a reparse point"));
    }
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if !directory && info.nNumberOfLinks != 1 {
        return Err(refuse(path, "it has more than one hard link"));
    }
    Ok(directory)
}

fn security(handle: &Handle, what: u32) -> Result<(PSID, *mut ACL, Local), Error> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            what,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = Local(descriptor);
    match status {
        0 => Ok((owner, dacl, descriptor)),
        code => {
            Err(io::Error::from_raw_os_error(i32::try_from(code).map_err(io::Error::other)?).into())
        }
    }
}

fn administrative(sid: PSID) -> bool {
    unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid) != 0
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
    }
}

fn trusted_installer() -> Result<Local, Error> {
    let text = wide(OsStr::new(Acl::TRUSTED_INSTALLER));
    let mut sid: PSID = ptr::null_mut();
    match unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } {
        0 => Err(io::Error::last_os_error().into()),
        _ => Ok(Local(sid)),
    }
}

fn apply(handle: &Handle, sddl: &str) -> Result<(), Error> {
    let descriptor = Descriptor::parse(&format!("{}{sddl}", Acl::OWNER))?;
    let what = OWNER_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION;
    match unsafe { SetKernelObjectSecurity(handle.0, what, descriptor.raw()) } {
        0 => Err(io::Error::last_os_error().into()),
        _ => Ok(()),
    }
}

fn enable_privileges() -> Result<(), Error> {
    let mut token: HANDLE = ptr::null_mut();
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };
    if opened == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let token = Handle(token);
    for name in [SE_BACKUP_NAME, SE_RESTORE_NAME, SE_TAKE_OWNERSHIP_NAME] {
        let mut luid: LUID = unsafe { zeroed() };
        if unsafe { LookupPrivilegeValueW(ptr::null(), name, &mut luid) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let size = u32::try_from(size_of::<TOKEN_PRIVILEGES>()).map_err(io::Error::other)?;
        let adjusted = unsafe {
            AdjustTokenPrivileges(token.0, 0, &state, size, ptr::null_mut(), ptr::null_mut())
        };
        let error = io::Error::last_os_error();
        let unassigned = error
            .raw_os_error()
            .iter()
            .any(|code| *code == Acl::NOT_ALL_ASSIGNED);
        if adjusted == 0 || unassigned {
            return Err(error.into());
        }
    }
    Ok(())
}

fn template(paths: &Paths, path: &Path, directory: bool, data: bool) -> String {
    if path == paths.token {
        return Sddl::file(Exposure::Readers);
    }
    if path == paths.peer_key {
        return Sddl::file(Exposure::Private);
    }
    if path == paths.settings || path == paths.config {
        return Sddl::file(Exposure::Public);
    }
    match (directory, data) {
        (true, true) => Sddl::data_directory(),
        (false, true) => Sddl::file(Exposure::Private),
        (true, false) => Sddl::settings_directory(),
        (false, false) => Sddl::file(Exposure::Public),
    }
}

fn squatted(paths: &Paths, path: &Path, handle: &Handle) -> Result<bool, Error> {
    let known = [
        &paths.settings,
        &paths.config,
        &paths.token,
        &paths.peer_key,
    ];
    if !known.iter().any(|item| item.as_path() == path) {
        return Ok(false);
    }
    let (owner, _dacl, _descriptor) = security(handle, OWNER_SECURITY_INFORMATION)?;
    Ok(!administrative(owner))
}

fn walk(paths: &Paths, dir: &Path, data: bool) -> Result<(), Error> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let handle = open(
            &path,
            READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        )?;
        let directory = plain(&path, &information(&handle)?)?;
        if squatted(paths, &path, &handle)? {
            drop(handle);
            fs::remove_file(&path)?;
            continue;
        }
        let data = data || path == paths.data;
        apply(&handle, &template(paths, &path, directory, data))?;
        drop(handle);
        if directory {
            walk(paths, &path, data)?;
        }
    }
    Ok(())
}

pub fn reclaim(paths: &Paths) -> Result<(), Error> {
    if !paths.state_dir.try_exists()? {
        return Ok(());
    }
    enable_privileges()?;
    let root = &paths.state_dir;
    let handle = open(
        root,
        READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
    )?;
    if !plain(root, &information(&handle)?)? {
        return Err(refuse(root, "it is not a directory"));
    }
    apply(&handle, &Sddl::settings_directory())?;
    drop(handle);
    walk(paths, root, false)
}

fn writers_are_trusted(dacl: *mut ACL, installer: PSID) -> Result<bool, Error> {
    if dacl.is_null() {
        return Ok(false);
    }
    let count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace: *mut c_void = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let applies = u32::from(header.AceFlags) & Acl::INHERIT_ONLY == 0;
        if u32::from(header.AceType) != Acl::ALLOWED || !applies {
            continue;
        }
        let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
        let mask = unsafe { (*allowed).Mask };
        let sid: PSID = unsafe { ptr::addr_of_mut!((*allowed).SidStart).cast() };
        let trusted = administrative(sid) || unsafe { EqualSid(sid, installer) } != 0;
        if mask & Acl::WRITE_RIGHTS != 0 && !trusted {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn trusted_binary(binary: &Path) -> Result<(), Error> {
    let expected = config::program_files()
        .join(config::WINDOWS_PRODUCT_DIR)
        .join(Acl::BINARY);
    let refusal = || {
        Error::Failed(format!(
            "refusing to register {}: the service binary must be {}, owned by SYSTEM, \
             Administrators or TrustedInstaller and writable by nobody else",
            binary.display(),
            expected.display()
        ))
    };
    let same = match (binary.to_str(), expected.to_str()) {
        (Some(binary), Some(expected)) => binary.eq_ignore_ascii_case(expected),
        (Some(..) | None, Some(..) | None) => return Err(refusal()),
    };
    if !same {
        return Err(refusal());
    }
    let Some(parent) = binary.parent() else {
        return Err(refusal());
    };
    let installer = trusted_installer()?;
    let file = trusted_object(binary, false, installer.0)?;
    let directory = trusted_object(parent, true, installer.0)?;
    match file && directory {
        true => Ok(()),
        false => Err(refusal()),
    }
}

fn trusted_object(path: &Path, directory: bool, installer: PSID) -> Result<bool, Error> {
    let handle = open(path, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    if plain(path, &information(&handle)?)? != directory {
        return Ok(false);
    }
    let (owner, dacl, _descriptor) = security(
        &handle,
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
    )?;
    let owned = administrative(owner) || unsafe { EqualSid(owner, installer) } != 0;
    Ok(owned && writers_are_trusted(dacl, installer)?)
}

pub fn administered(path: &Path) -> Result<(), Error> {
    let handle = open(path, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    if !plain(path, &information(&handle)?)? {
        return Err(refuse(path, "it is not a directory"));
    }
    let (owner, _dacl, _descriptor) = security(&handle, OWNER_SECURITY_INFORMATION)?;
    match administrative(owner) {
        true => Ok(()),
        false => Err(refuse(path, "it is not owned by SYSTEM or Administrators")),
    }
}
