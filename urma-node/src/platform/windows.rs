use super::{Exposure, service_sid::service_sid};
use crate::config;
use std::{
    ffi::OsStr,
    fs::File,
    io,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

struct Sddl;

impl Sddl {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    fn of(template: &str) -> String {
        template.replace("{svc}", &service_sid(config::SERVICE_NAME))
    }

    fn settings_directory() -> String {
        Self::of("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;{svc})(A;OICI;FRFX;;;BU)")
    }

    fn data_directory() -> String {
        Self::of("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{svc})")
    }

    fn file(exposure: Exposure) -> String {
        Self::of(match exposure {
            Exposure::Private => "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{svc})",
            Exposure::Readers => "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;{svc})(A;;FR;;;BU)",
            Exposure::Public => "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;{svc})(A;;FR;;;BU)",
        })
    }
}

struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from the SDDL conversion, which uses LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

impl Descriptor {
    fn parse(sddl: &str) -> Result<Self, io::Error> {
        let text = wide(OsStr::new(sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `text` is NUL-terminated; `Descriptor` frees the result.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        match converted {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(Self(descriptor)),
        }
    }

    fn attributes(&self) -> Result<SECURITY_ATTRIBUTES, io::Error> {
        Ok(SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        })
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

fn create_directory(path: &Path, sddl: &str) -> Result<(), io::Error> {
    let descriptor = Descriptor::parse(sddl)?;
    let attributes = descriptor.attributes()?;
    let path = wide(path.as_os_str());
    // SAFETY: `path` is NUL-terminated and `attributes` outlives the call.
    match unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

pub(super) fn create_settings_directory(path: &Path) -> Result<(), io::Error> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent)?,
        None => return Err(io::Error::other("settings directory has no parent")),
    }
    create_directory(path, &Sddl::settings_directory())
}

pub(super) fn create_state_directory(path: &Path) -> Result<(), io::Error> {
    create_settings_directory(path)
}

pub(super) fn create_data_directory(path: &Path) -> Result<(), io::Error> {
    create_directory(path, &Sddl::data_directory())
}

pub(super) fn create_file(path: &Path, exposure: Exposure) -> Result<File, io::Error> {
    let descriptor = Descriptor::parse(&Sddl::file(exposure))?;
    let attributes = descriptor.attributes()?;
    let path = wide(path.as_os_str());
    // SAFETY: `path` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            Sddl::GENERIC_READ | Sddl::GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    match handle {
        INVALID_HANDLE_VALUE => Err(io::Error::last_os_error()),
        // SAFETY: CreateFileW returned a fresh handle that the File now owns.
        _ => Ok(unsafe { File::from_raw_handle(handle) }),
    }
}
