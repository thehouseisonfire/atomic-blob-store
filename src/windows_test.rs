use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_REMOTE_PROTOCOL_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileRemoteProtocolInfo, GetDriveTypeW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OSVERSIONINFOW) -> i32;
}

use crate::filesystem::wide_path;

const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

#[derive(Clone, Debug)]
pub(crate) struct WindowsTestEnvironment {
    pub(crate) test_root: PathBuf,
    pub(crate) volume_path: OsString,
    pub(crate) volume_name: OsString,
    pub(crate) filesystem: OsString,
    pub(crate) drive_type: u32,
    pub(crate) remote_protocol: bool,
    pub(crate) windows_major: u32,
    pub(crate) windows_minor: u32,
    pub(crate) windows_build: u32,
}

impl WindowsTestEnvironment {
    pub(crate) fn inspect(test_root: &Path) -> io::Result<Self> {
        let test_root = std::path::absolute(test_root)?;
        let root_wide = wide_path(&test_root);
        let mut volume_path = vec![0_u16; 32_768];
        // SAFETY: all pointers refer to live, sized buffers and the input is NUL-terminated.
        if unsafe {
            GetVolumePathNameW(
                root_wide.as_ptr(),
                volume_path.as_mut_ptr(),
                u32::try_from(volume_path.len()).expect("the path buffer length fits in u32"),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        truncate_nul(&mut volume_path);
        let mut volume_path_nul = volume_path.clone();
        volume_path_nul.push(0);

        let mut volume_name = vec![0_u16; 64];
        // SAFETY: the mount point and output buffers are valid and NUL-terminated/sized.
        if unsafe {
            GetVolumeNameForVolumeMountPointW(
                volume_path_nul.as_ptr(),
                volume_name.as_mut_ptr(),
                u32::try_from(volume_name.len()).expect("the volume-name buffer fits in u32"),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        truncate_nul(&mut volume_name);

        let mut filesystem = vec![0_u16; 64];
        // SAFETY: the mount point and filesystem output buffers are valid.
        if unsafe {
            GetVolumeInformationW(
                volume_path_nul.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                u32::try_from(filesystem.len()).expect("the filesystem buffer fits in u32"),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        truncate_nul(&mut filesystem);

        // SAFETY: the mount point is a NUL-terminated root path.
        let drive_type = unsafe { GetDriveTypeW(volume_path_nul.as_ptr()) };
        let remote_protocol = path_uses_remote_protocol(&test_root)?;

        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>())
                .expect("OSVERSIONINFOW size fits in u32"),
            ..OSVERSIONINFOW::default()
        };
        // SAFETY: the initialized structure has the required size field and is writable.
        let status = unsafe { RtlGetVersion(&raw mut version) };
        if status < 0 {
            return Err(io::Error::other(format!(
                "RtlGetVersion failed with NTSTATUS {status:#x}"
            )));
        }

        Ok(Self {
            test_root,
            volume_path: OsString::from_wide(&volume_path),
            volume_name: OsString::from_wide(&volume_name),
            filesystem: OsString::from_wide(&filesystem),
            drive_type,
            remote_protocol,
            windows_major: version.dwMajorVersion,
            windows_minor: version.dwMinorVersion,
            windows_build: version.dwBuildNumber,
        })
    }

    pub(crate) fn is_local_ntfs(&self) -> bool {
        self.filesystem
            .to_string_lossy()
            .eq_ignore_ascii_case("NTFS")
            && self.drive_type == DRIVE_FIXED
            && !self.remote_protocol
    }

    pub(crate) fn qualification_reason(&self) -> String {
        if self.is_local_ntfs() {
            "qualified: actual test root is on a local fixed NTFS volume".to_owned()
        } else {
            format!(
                "not contract evidence: filesystem={:?}, drive_type={} (remote={}), remote_protocol={}",
                self.filesystem,
                self.drive_type,
                self.drive_type == DRIVE_REMOTE,
                self.remote_protocol
            )
        }
    }

    pub(crate) fn report(&self) -> String {
        format!(
            concat!(
                "test_root={:?}\nvolume_path={:?}\nvolume_name={:?}\nfilesystem={:?}\n",
                "drive_type={}\nremote_protocol={}\nlocal_ntfs={}\n",
                "windows_version={}.{}.{}\narchitecture={}\nqualification={}\n"
            ),
            self.test_root,
            self.volume_path,
            self.volume_name,
            self.filesystem,
            self.drive_type,
            self.remote_protocol,
            self.is_local_ntfs(),
            self.windows_major,
            self.windows_minor,
            self.windows_build,
            std::env::consts::ARCH,
            self.qualification_reason(),
        )
    }
}

pub(crate) fn qualify_contract_test(test_root: &Path, artifact_name: &str) -> io::Result<bool> {
    let environment = match WindowsTestEnvironment::inspect(test_root) {
        Ok(environment) => environment,
        Err(source) => {
            let reason = format!(
                "not contract evidence: failed to inspect actual test root {test_root:?}: {source}"
            );
            write_report(
                artifact_name,
                &format!("test_root={test_root:?}\nqualification={reason}\n"),
            )?;
            if std::env::var_os("ATOMIC_BLOB_REQUIRE_LOCAL_NTFS").is_some() {
                return Err(io::Error::other(reason));
            }
            eprintln!("skipping local-NTFS contract evidence: {reason}");
            return Ok(false);
        }
    };
    write_report(artifact_name, &environment.report())?;
    if environment.is_local_ntfs() {
        return Ok(true);
    }
    if std::env::var_os("ATOMIC_BLOB_REQUIRE_LOCAL_NTFS").is_some() {
        return Err(io::Error::other(environment.qualification_reason()));
    }
    eprintln!(
        "skipping local-NTFS contract evidence: {}",
        environment.qualification_reason()
    );
    Ok(false)
}

fn write_report(artifact_name: &str, report: &str) -> io::Result<()> {
    if let Some(artifact_root) = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR") {
        let directory = PathBuf::from(artifact_root).join("environments");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join(format!("{artifact_name}.txt")), report)?;
    }
    Ok(())
}

fn path_uses_remote_protocol(path: &Path) -> io::Result<bool> {
    use std::os::windows::io::FromRawHandle;

    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated; ownership is transferred only after validation.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call returned one owned handle.
    let _file = unsafe { std::fs::File::from_raw_handle(handle) };
    let mut protocol = FILE_REMOTE_PROTOCOL_INFO {
        StructureVersion: 2,
        StructureSize: u16::try_from(size_of::<FILE_REMOTE_PROTOCOL_INFO>())
            .expect("FILE_REMOTE_PROTOCOL_INFO size fits in u16"),
        ..FILE_REMOTE_PROTOCOL_INFO::default()
    };
    // SAFETY: the handle is live and the output buffer has the exact advertised size.
    let result = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileRemoteProtocolInfo,
            (&raw mut protocol).cast(),
            u32::try_from(size_of::<FILE_REMOTE_PROTOCOL_INFO>())
                .expect("FILE_REMOTE_PROTOCOL_INFO size fits in u32"),
        )
    };
    if result != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // Local files do not have remote protocol information.
        Some(1 | 50 | 87) => Ok(false),
        _ => Err(error),
    }
}

fn truncate_nul(buffer: &mut Vec<u16>) {
    if let Some(length) = buffer.iter().position(|unit| *unit == 0) {
        buffer.truncate(length);
    }
}
