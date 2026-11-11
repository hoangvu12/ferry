//! Win32 implementations behind `crate::sys`.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, FileTimes, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_STANDARD_INFO, FILE_WRITE_ATTRIBUTES, FileBasicInfo, FileIdInfo, FileStandardInfo,
    GetDiskFreeSpaceExW, GetFileInformationByHandle, GetFileInformationByHandleEx,
};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicProcessIdList, OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use super::{FileId, Stat};

/// `as_bytes`/`from_bytes` over the OS string's encoded bytes. On Windows
/// that is WTF-8, which equals UTF-8 for every valid Unicode path.
pub trait OsStrExt {
    fn as_bytes(&self) -> &[u8];
    fn from_bytes(bytes: &[u8]) -> &Self;
}

impl OsStrExt for OsStr {
    fn as_bytes(&self) -> &[u8] {
        self.as_encoded_bytes()
    }

    fn from_bytes(bytes: &[u8]) -> &Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => OsStr::new(s),
            // SAFETY: Ferry only passes bytes that are UTF-8 (git output,
            // files it wrote) or that it took from an OS string with
            // `as_bytes` and split at ASCII bytes, which keeps WTF-8 valid.
            Err(_) => unsafe { OsStr::from_encoded_bytes_unchecked(bytes) },
        }
    }
}

pub trait OsStringExt {
    fn from_vec(vec: Vec<u8>) -> Self;
    fn into_vec(self) -> Vec<u8>;
}

impl OsStringExt for OsString {
    fn from_vec(vec: Vec<u8>) -> Self {
        match String::from_utf8(vec) {
            Ok(s) => OsString::from(s),
            // SAFETY: as for `OsStrExt::from_bytes`.
            Err(e) => unsafe { OsString::from_encoded_bytes_unchecked(e.into_bytes()) },
        }
    }

    fn into_vec(self) -> Vec<u8> {
        self.into_encoded_bytes()
    }
}

/// 100 ns ticks between 1601-01-01 (FILETIME's epoch) and 1970-01-01.
const EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// `(seconds, nanoseconds)` since the Unix epoch of a FILETIME.
pub(super) fn from_filetime(ticks: i64) -> (i64, i64) {
    let unix = ticks - EPOCH_TICKS;
    (
        unix.div_euclid(10_000_000),
        unix.rem_euclid(10_000_000) * 100,
    )
}

fn system_time((secs, nsec): (i64, i64)) -> SystemTime {
    let nsec = u32::try_from(nsec).unwrap_or(0);
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.cast_unsigned(), nsec)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + Duration::from_nanos(nsec.into())
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// Opens `path` for its attributes only; directories too.
fn open_attributes(path: &Path, follow: bool, access: u32) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    OpenOptions::new()
        .access_mode(access)
        .custom_flags(flags)
        .open(path)
}

/// A buffer length as the `u32` Win32 takes; Ferry's buffers are small.
fn len32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn handle(file: &File) -> HANDLE {
    file.as_raw_handle() as HANDLE
}

/// `GetFileInformationByHandleEx` into a `T`.
fn info<T: Default>(file: &File, class: i32) -> io::Result<T> {
    let mut out = T::default();
    // SAFETY: the handle is open for the call, `out` is a valid, writable
    // `T` of the size passed, and `class` names a struct of that layout.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle(file),
            class,
            (&raw mut out).cast::<c_void>(),
            len32(size_of::<T>()),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out)
}

fn file_id(file: &File) -> io::Result<FileId> {
    if let Ok(id) = info::<FILE_ID_INFO>(file, FileIdInfo) {
        return Ok(FileId {
            dev: id.VolumeSerialNumber,
            ino: u128::from_le_bytes(id.FileId.Identifier),
        });
    }
    // FAT and some network filesystems only have the 64-bit index.
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open and `info` is a valid out pointer.
    if unsafe { GetFileInformationByHandle(handle(file), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileId {
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow),
    })
}

pub(super) fn stat_handle(file: &File) -> io::Result<Stat> {
    let basic: FILE_BASIC_INFO = info(file, FileBasicInfo)?;
    let standard: FILE_STANDARD_INFO = info(file, FileStandardInfo)?;
    Ok(Stat {
        id: file_id(file)?,
        len: standard.EndOfFile.cast_unsigned(),
        mtime: from_filetime(basic.LastWriteTime),
        ctime: from_filetime(basic.ChangeTime),
        nlink: u64::from(standard.NumberOfLinks),
    })
}

pub(super) fn stat_path(path: &Path, follow: bool) -> io::Result<Stat> {
    stat_handle(&open_attributes(path, follow, FILE_READ_ATTRIBUTES)?)
}

pub(super) fn set_times(
    path: &Path,
    atime: (i64, i64),
    mtime: (i64, i64),
    nofollow: bool,
) -> io::Result<()> {
    let file = open_attributes(path, !nofollow, FILE_WRITE_ATTRIBUTES)?;
    file.set_times(
        FileTimes::new()
            .set_accessed(system_time(atime))
            .set_modified(system_time(mtime)),
    )
}

pub(super) fn open_nofollow(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    Ok(Some(file))
}

unsafe extern "system" fn ignore_ctrl(_: u32) -> BOOL {
    1
}

pub(super) fn exec(command: &mut Command) -> io::Error {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return e,
    };
    // A handler routine, not `SetConsoleCtrlHandler(None, TRUE)`: the latter
    // is inherited by processes the child starts.
    // SAFETY: `ignore_ctrl` is a valid handler for the life of the process.
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) };
    match child.wait() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

pub(super) fn free_space(path: &Path) -> io::Result<u64> {
    let path = wide(path.as_os_str());
    let mut free = 0u64;
    // SAFETY: `path` is NUL-terminated and outlives the call; the other out
    // pointers may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}

/// The name of the job object of the agent whose busy lock is `busy_lock`.
/// Session-local, so agents of another logged-in user never collide.
fn job_name(busy_lock: &Path) -> Vec<u16> {
    let key = busy_lock.as_os_str().to_string_lossy().to_lowercase();
    let name = format!(
        "Local\\ferry-agent-{:016x}",
        crate::store::fnv1a64(key.as_bytes())
    );
    wide(OsStr::new(&name))
}

/// Puts this process, and so every process it starts from now on, into the
/// agent's job object, creating it if needed. Windows has no inheritable
/// `flock`: the job is how `rm`, `list` and `gc` see processes that outlive
/// the one holding the busy lock.
///
/// A job's name lasts only while a handle to it is open, so the handle is
/// leaked and made inheritable: every process started from here on holds a
/// copy (std always lets children inherit handles), and the name stays
/// findable exactly as long as one of them runs, as the inherited `flock`
/// descriptor does on Unix.
pub fn join_agent_job(busy_lock: &Path) -> io::Result<()> {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let name = job_name(busy_lock);
    let inherit = SECURITY_ATTRIBUTES {
        nLength: len32(size_of::<SECURITY_ATTRIBUTES>()),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: `name` is NUL-terminated and `inherit` outlives the call.
    let job = unsafe { CreateJobObjectW(&raw const inherit, name.as_ptr()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: no preconditions; the pseudo handle needs no closing.
    let me = unsafe { GetCurrentProcess() };
    // SAFETY: both handles are valid.
    if unsafe { AssignProcessToJobObject(job, me) } == 0 {
        let err = io::Error::last_os_error();
        // SAFETY: `job` is a handle this function owns.
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(())
}

/// The processes in the agent's job object; empty when there is none.
pub fn agent_job_pids(busy_lock: &Path) -> io::Result<Vec<u32>> {
    const MAX: usize = 4096;
    let name = job_name(busy_lock);
    // SAFETY: `name` is NUL-terminated.
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    if job.is_null() {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(code) if code == ERROR_FILE_NOT_FOUND.cast_signed() => Ok(Vec::new()),
            _ => Err(err),
        };
    }
    // The list header followed by room for `MAX` ids, as `usize`s so the
    // buffer is aligned for the header.
    let header = size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() / size_of::<usize>();
    let mut buf = vec![0usize; header + MAX];
    // SAFETY: `job` is valid and `buf` is writable for the length passed.
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr().cast::<c_void>(),
            len32(buf.len() * size_of::<usize>()),
            std::ptr::null_mut(),
        )
    };
    let err = io::Error::last_os_error();
    // SAFETY: `job` is a handle this function owns.
    unsafe { CloseHandle(job) };
    if ok == 0 {
        return Err(err);
    }
    // SAFETY: the call filled a header at the start of the aligned buffer.
    let list = unsafe { &*buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
    let count = (list.NumberOfProcessIdsInList as usize).min(MAX);
    let ids =
        std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / size_of::<usize>();
    Ok(buf[ids..ids + count]
        .iter()
        .filter_map(|&pid| u32::try_from(pid).ok())
        .collect())
}

/// The filesystem name of the volume holding `path` (`NTFS`, `ReFS`, ...).
pub fn fs_name(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    let file = open_attributes(path, true, FILE_READ_ATTRIBUTES)?;
    let mut name = [0u16; 64];
    // SAFETY: the handle is open; `name` is writable for the length passed
    // and the other buffers are null with zero length.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle(&file),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            len32(name.len()),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..len]))
}

/// `Windows <build>.<revision>`, e.g. `Windows 26200.7171`, from the
/// registry (the version APIs lie to programs without a manifest).
pub fn windows_version() -> String {
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    let key = wide(OsStr::new(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"));
    let mut build = [0u16; 32];
    let mut len = len32(build.len() * 2);
    // SAFETY: the strings are NUL-terminated; `build` is writable for `len`.
    let build_ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            wide(OsStr::new("CurrentBuild")).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            build.as_mut_ptr().cast::<c_void>(),
            &raw mut len,
        )
    } == 0;
    let mut revision = 0u32;
    let mut len = 4u32;
    // SAFETY: as above, with a DWORD out value.
    let revision_ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            wide(OsStr::new("UBR")).as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut revision).cast::<c_void>(),
            &raw mut len,
        )
    } == 0;
    if !build_ok {
        return "Windows".into();
    }
    let end = build.iter().position(|&c| c == 0).unwrap_or(build.len());
    let build = String::from_utf16_lossy(&build[..end]);
    if revision_ok {
        format!("Windows {build}.{revision}")
    } else {
        format!("Windows {build}")
    }
}
