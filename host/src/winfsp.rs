//! The client's shared folders as a drive on the host (FEATURE_FOLDER_MOUNT),
//! through WinFsp's FUSE layer.
//!
//! WinFsp (optional component, a signed file system driver) is loaded at run
//! time from its install directory, so the host works without it. Every file
//! system call is answered by the client over an FS stream
//! (`nya_transport::folders::call`); WinFsp caches file and directory
//! information for a second to keep Explorer responsive over the network.
//!
//! Run as SYSTEM (the service), the drive letter is global: the user sees it
//! in their session. Files get mode 0777 (Everyone may read and write); the
//! client enforces read-only folders.
//!
//! The types below follow WinFsp's `inc/fuse/*.h` for 64-bit Windows.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use nya_proto::pb::{self, fs_request::Op, FsError};
use nya_transport::folders::MAX_IO;
use nya_transport::quinn::Connection;

// ---------------------------------------------------------------- FFI types

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FuseTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Default)]
struct FuseStat {
    st_dev: u32,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u16,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u32,
    st_size: i64,
    st_atim: FuseTimespec,
    st_mtim: FuseTimespec,
    st_ctim: FuseTimespec,
    st_blksize: i32,
    st_blocks: i64,
    st_birthtim: FuseTimespec,
}

#[repr(C)]
struct FuseStatvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
}

#[repr(C)]
struct FuseContext {
    fuse: *mut c_void,
    uid: u32,
    gid: u32,
    pid: i32,
    private_data: *mut c_void,
    umask: u32,
}

#[repr(C)]
struct FspFuseEnv {
    environment: u32,
    memalloc: unsafe extern "C" fn(usize) -> *mut c_void,
    memfree: unsafe extern "C" fn(*mut c_void),
    daemonize: unsafe extern "C" fn(c_int) -> c_int,
    set_signal_handlers: unsafe extern "C" fn(*mut c_void) -> c_int,
    conv_to_win_path: *const c_void,
    winpid_to_pid: *const c_void,
    reserved: [*const c_void; 2],
}

// SAFETY: plain function pointers and nulls, never written after creation.
unsafe impl Sync for FspFuseEnv {}

type Path = *const c_char;
type Fi = *mut c_void; // struct fuse_file_info (unused)
type Filler = unsafe extern "C" fn(*mut c_void, *const c_char, *const FuseStat, i64) -> c_int;
type Unused = Option<unsafe extern "C" fn()>;

/// `struct fuse_operations` (FUSE 2.9 + WinFsp/OSXFUSE extensions), in order.
#[repr(C)]
#[derive(Default)]
struct FuseOperations {
    getattr: Option<unsafe extern "C" fn(Path, *mut FuseStat) -> c_int>,
    getdir: Unused,
    readlink: Unused,
    mknod: Unused,
    mkdir: Option<unsafe extern "C" fn(Path, u32) -> c_int>,
    unlink: Option<unsafe extern "C" fn(Path) -> c_int>,
    rmdir: Option<unsafe extern "C" fn(Path) -> c_int>,
    symlink: Unused,
    rename: Option<unsafe extern "C" fn(Path, Path) -> c_int>,
    link: Unused,
    chmod: Option<unsafe extern "C" fn(Path, u32) -> c_int>,
    chown: Option<unsafe extern "C" fn(Path, u32, u32) -> c_int>,
    truncate: Option<unsafe extern "C" fn(Path, i64) -> c_int>,
    utime: Unused,
    open: Option<unsafe extern "C" fn(Path, Fi) -> c_int>,
    read: Option<unsafe extern "C" fn(Path, *mut c_char, usize, i64, Fi) -> c_int>,
    write: Option<unsafe extern "C" fn(Path, *const c_char, usize, i64, Fi) -> c_int>,
    statfs: Option<unsafe extern "C" fn(Path, *mut FuseStatvfs) -> c_int>,
    flush: Unused,
    release: Unused,
    fsync: Unused,
    setxattr: Unused,
    getxattr: Unused,
    listxattr: Unused,
    removexattr: Unused,
    opendir: Unused,
    readdir: Option<unsafe extern "C" fn(Path, *mut c_void, Filler, i64, Fi) -> c_int>,
    releasedir: Unused,
    fsyncdir: Unused,
    init: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    destroy: Unused,
    access: Unused,
    create: Option<unsafe extern "C" fn(Path, u32, Fi) -> c_int>,
    ftruncate: Option<unsafe extern "C" fn(Path, i64, Fi) -> c_int>,
    fgetattr: Option<unsafe extern "C" fn(Path, *mut FuseStat, Fi) -> c_int>,
    lock: Unused,
    utimens: Option<unsafe extern "C" fn(Path, *const FuseTimespec) -> c_int>,
    bmap: Unused,
    flags: u32,
    ioctl: Unused,
    poll: Unused,
    write_buf: Unused,
    read_buf: Unused,
    flock: Unused,
    fallocate: Unused,
    getpath: Unused,
    reserved01: Unused,
    reserved02: Unused,
    statfs_x: Unused,
    setvolname: Unused,
    exchange: Unused,
    getxtimes: Unused,
    setbkuptime: Unused,
    setchgtime: Unused,
    setcrtime: Unused,
    chflags: Unused,
    setattr_x: Unused,
    fsetattr_x: Unused,
}

type MainReal = unsafe extern "C" fn(*const FspFuseEnv, c_int, *mut *mut c_char, *const FuseOperations, usize, *mut c_void) -> c_int;
type Exit = unsafe extern "C" fn(*const FspFuseEnv, *mut c_void);
type GetContext = unsafe extern "C" fn(*const FspFuseEnv) -> *mut FuseContext;

struct Api {
    main_real: MainReal,
    exit: Exit,
    get_context: GetContext,
}

extern "C" {
    fn malloc(n: usize) -> *mut c_void;
    fn free(p: *mut c_void);
}

unsafe extern "C" fn no_daemonize(_: c_int) -> c_int {
    0
}

unsafe extern "C" fn no_signals(_: *mut c_void) -> c_int {
    0
}

static ENV: FspFuseEnv = FspFuseEnv {
    environment: b'W' as u32,
    memalloc: malloc,
    memfree: free,
    daemonize: no_daemonize,
    set_signal_handlers: no_signals,
    conv_to_win_path: std::ptr::null(),
    winpid_to_pid: std::ptr::null(),
    reserved: [std::ptr::null(); 2],
};

// ---------------------------------------------------------------- loading

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
}

fn load() -> Result<Api> {
    use windows::core::{s, HSTRING};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let path = nya_server_core::components::winfsp_dll().ok_or_else(|| anyhow!("被控端没有安装 WinFsp（“本机 → 可选组件”里可一键安装）"))?;
    // SAFETY: loading WinFsp's own signed DLL and looking up its documented exports.
    unsafe {
        let lib = LoadLibraryW(&HSTRING::from(path.as_os_str()))?;
        let main_real = GetProcAddress(lib, s!("fsp_fuse_main_real")).ok_or_else(|| anyhow!("fsp_fuse_main_real missing"))?;
        let exit = GetProcAddress(lib, s!("fsp_fuse_exit")).ok_or_else(|| anyhow!("fsp_fuse_exit missing"))?;
        let get_context = GetProcAddress(lib, s!("fsp_fuse_get_context")).ok_or_else(|| anyhow!("fsp_fuse_get_context missing"))?;
        Ok(Api {
            main_real: std::mem::transmute::<_, MainReal>(main_real),
            exit: std::mem::transmute::<_, Exit>(exit),
            get_context: std::mem::transmute::<_, GetContext>(get_context),
        })
    }
}

// ---------------------------------------------------------------- the file system

const ENOENT: c_int = 2;
const EIO: c_int = 5;
const EACCES: c_int = 13;
const EEXIST: c_int = 17;
const ENOTDIR: c_int = 20;
const EISDIR: c_int = 21;
const EINVAL: c_int = 22;
const ENOSPC: c_int = 28;
const ENOTEMPTY: c_int = 41;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// The mounted file system's state, reached from WinFsp's threads.
struct RemoteFs {
    conn: Connection,
    rt: tokio::runtime::Handle,
    /// `struct fuse *`, known once mounted (to unmount).
    fuse: AtomicPtr<c_void>,
    mounted: Mutex<Option<mpsc::Sender<()>>>,
}

impl RemoteFs {
    fn call(&self, path: &str, op: Op) -> std::result::Result<pb::FsReply, c_int> {
        let req = pb::FsRequest { path: path.to_owned(), op: Some(op) };
        let conn = self.conn.clone();
        let r = self.rt.block_on(async move { tokio::time::timeout(Duration::from_secs(30), nya_transport::folders::call(&conn, &req)).await });
        let reply = match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::debug!("folder request {path}: {e:#}");
                return Err(-EIO);
            }
            Err(_) => {
                tracing::warn!("folder request {path}: no answer in 30 s");
                return Err(-EIO);
            }
        };
        match FsError::try_from(reply.error).unwrap_or(FsError::FsIo) {
            FsError::FsOk => Ok(reply),
            FsError::FsNotFound => Err(-ENOENT),
            FsError::FsExists => Err(-EEXIST),
            FsError::FsAccess => Err(-EACCES),
            FsError::FsNotEmpty => Err(-ENOTEMPTY),
            FsError::FsIsDir => Err(-EISDIR),
            FsError::FsNotDir => Err(-ENOTDIR),
            FsError::FsInvalid => Err(-EINVAL),
            FsError::FsNoSpace => Err(-ENOSPC),
            FsError::FsIo => Err(-EIO),
        }
    }
}

fn ts(us: i64) -> FuseTimespec {
    FuseTimespec { tv_sec: us.div_euclid(1_000_000), tv_nsec: us.rem_euclid(1_000_000) * 1000 }
}

fn to_us(t: &FuseTimespec) -> i64 {
    t.tv_sec * 1_000_000 + t.tv_nsec / 1000
}

fn fill_stat(a: &pb::FsAttr, st: &mut FuseStat) {
    *st = FuseStat::default();
    st.st_mode = if a.dir { S_IFDIR | 0o777 } else if a.read_only { S_IFREG | 0o555 } else { S_IFREG | 0o777 };
    st.st_nlink = 1;
    st.st_size = a.size as i64;
    st.st_blksize = 4096;
    st.st_blocks = a.size.div_ceil(512) as i64;
    st.st_mtim = ts(a.mtime_us);
    st.st_atim = ts(if a.atime_us != 0 { a.atime_us } else { a.mtime_us });
    st.st_ctim = ts(a.mtime_us);
    st.st_birthtim = ts(if a.ctime_us != 0 { a.ctime_us } else { a.mtime_us });
}

/// The file system of the current WinFsp call.
unsafe fn fs<'a>() -> &'a RemoteFs {
    let api = api().expect("WinFsp loaded");
    let ctx = (api.get_context)(&ENV);
    &*((*ctx).private_data as *const RemoteFs)
}

unsafe fn path<'a>(p: Path) -> std::borrow::Cow<'a, str> {
    CStr::from_ptr(p).to_string_lossy()
}

fn status(r: std::result::Result<pb::FsReply, c_int>) -> c_int {
    match r {
        Ok(_) => 0,
        Err(e) => e,
    }
}

unsafe extern "C" fn op_getattr(p: Path, st: *mut FuseStat) -> c_int {
    match fs().call(&path(p), Op::Stat(pb::FsStat {})) {
        Ok(r) => {
            fill_stat(&r.attr.unwrap_or_default(), &mut *st);
            0
        }
        Err(e) => e,
    }
}

unsafe extern "C" fn op_fgetattr(p: Path, st: *mut FuseStat, _: Fi) -> c_int {
    op_getattr(p, st)
}

unsafe extern "C" fn op_readdir(p: Path, buf: *mut c_void, filler: Filler, _off: i64, _: Fi) -> c_int {
    let r = match fs().call(&path(p), Op::List(pb::FsList {})) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let mut st = FuseStat::default();
    fill_stat(&pb::FsAttr { dir: true, ..Default::default() }, &mut st);
    for dot in [c".", c".."] {
        if filler(buf, dot.as_ptr(), &st, 0) != 0 {
            return 0;
        }
    }
    for e in r.entries {
        let Ok(name) = CString::new(e.name) else { continue };
        fill_stat(&e.attr.unwrap_or_default(), &mut st);
        if filler(buf, name.as_ptr(), &st, 0) != 0 {
            break;
        }
    }
    0
}

unsafe extern "C" fn op_open(_: Path, _: Fi) -> c_int {
    0 // stateless: every read / write names the file
}

unsafe extern "C" fn op_create(p: Path, _mode: u32, _: Fi) -> c_int {
    status(fs().call(&path(p), Op::Create(pb::FsCreate { dir: false, exclusive: false })))
}

unsafe extern "C" fn op_mkdir(p: Path, _mode: u32) -> c_int {
    status(fs().call(&path(p), Op::Create(pb::FsCreate { dir: true, exclusive: true })))
}

unsafe extern "C" fn op_unlink(p: Path) -> c_int {
    status(fs().call(&path(p), Op::Remove(pb::FsRemove { dir: false })))
}

unsafe extern "C" fn op_rmdir(p: Path) -> c_int {
    status(fs().call(&path(p), Op::Remove(pb::FsRemove { dir: true })))
}

unsafe extern "C" fn op_rename(from: Path, to: Path) -> c_int {
    // WinFsp checks "replace if exists" itself before calling.
    status(fs().call(&path(from), Op::Rename(pb::FsRename { to: path(to).into_owned(), replace: true })))
}

unsafe extern "C" fn op_ignore_mode(_: Path, _: u32) -> c_int {
    0
}

unsafe extern "C" fn op_ignore_owner(_: Path, _: u32, _: u32) -> c_int {
    0
}

unsafe extern "C" fn op_truncate(p: Path, size: i64) -> c_int {
    status(fs().call(&path(p), Op::Truncate(pb::FsTruncate { size: size.max(0) as u64 })))
}

unsafe extern "C" fn op_ftruncate(p: Path, size: i64, _: Fi) -> c_int {
    op_truncate(p, size)
}

unsafe extern "C" fn op_read(p: Path, buf: *mut c_char, size: usize, off: i64, _: Fi) -> c_int {
    let fs = fs();
    let p = path(p);
    let mut done = 0usize;
    while done < size {
        let len = (size - done).min(MAX_IO);
        let r = match fs.call(&p, Op::Read(pb::FsRead { offset: off as u64 + done as u64, len: len as u32 })) {
            Ok(r) => r,
            Err(e) if done == 0 => return e,
            Err(_) => break,
        };
        let n = r.data.len().min(len);
        std::ptr::copy_nonoverlapping(r.data.as_ptr(), buf.add(done).cast(), n);
        done += n;
        if n < len {
            break; // end of file
        }
    }
    done as c_int
}

unsafe extern "C" fn op_write(p: Path, buf: *const c_char, size: usize, off: i64, _: Fi) -> c_int {
    let fs = fs();
    let p = path(p);
    let mut done = 0usize;
    while done < size {
        let len = (size - done).min(MAX_IO);
        let data = std::slice::from_raw_parts(buf.add(done).cast::<u8>(), len).to_vec();
        match fs.call(&p, Op::Write(pb::FsWrite { offset: off as u64 + done as u64, data })) {
            Ok(_) => done += len,
            Err(e) if done == 0 => return e,
            Err(_) => break,
        }
    }
    done as c_int
}

unsafe extern "C" fn op_statfs(p: Path, st: *mut FuseStatvfs) -> c_int {
    let r = match fs().call(&path(p), Op::Volume(pb::FsVolume {})) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let block = 4096u64;
    *st = FuseStatvfs {
        f_bsize: block,
        f_frsize: block,
        f_blocks: r.total_bytes / block,
        f_bfree: r.free_bytes / block,
        f_bavail: r.free_bytes / block,
        f_files: 0,
        f_ffree: 0,
        f_favail: 0,
        f_fsid: 0,
        f_flag: 0,
        f_namemax: 255,
    };
    0
}

unsafe extern "C" fn op_utimens(p: Path, tv: *const FuseTimespec) -> c_int {
    let (atime, mtime) = (&*tv, &*tv.add(1));
    status(fs().call(&path(p), Op::SetTimes(pb::FsSetTimes { mtime_us: to_us(mtime), atime_us: to_us(atime) })))
}

unsafe extern "C" fn op_init(_conn: *mut c_void) -> *mut c_void {
    let api = api().expect("WinFsp loaded");
    let ctx = (api.get_context)(&ENV);
    let fs = &*((*ctx).private_data as *const RemoteFs);
    fs.fuse.store((*ctx).fuse, Ordering::SeqCst);
    if let Some(tx) = fs.mounted.lock().unwrap().take() {
        let _ = tx.send(());
    }
    (*ctx).private_data
}

fn operations() -> FuseOperations {
    FuseOperations {
        getattr: Some(op_getattr),
        fgetattr: Some(op_fgetattr),
        readdir: Some(op_readdir),
        open: Some(op_open),
        create: Some(op_create),
        mkdir: Some(op_mkdir),
        unlink: Some(op_unlink),
        rmdir: Some(op_rmdir),
        rename: Some(op_rename),
        chmod: Some(op_ignore_mode),
        chown: Some(op_ignore_owner),
        truncate: Some(op_truncate),
        ftruncate: Some(op_ftruncate),
        read: Some(op_read),
        write: Some(op_write),
        statfs: Some(op_statfs),
        utimens: Some(op_utimens),
        init: Some(op_init),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- mounting

/// A mounted drive; unmounted by [`Mount::unmount`] (or on drop).
pub struct Mount {
    fs: Arc<RemoteFs>,
    done: mpsc::Receiver<c_int>,
    pub point: String,
}

/// First free drive letter from Z: down to D:.
fn free_drive() -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    // SAFETY: no arguments.
    let used = unsafe { GetLogicalDrives() };
    (3..26u32).rev().find(|i| used & (1 << i) == 0).map(|i| format!("{}:", (b'A' + i as u8) as char))
}

/// Volume label: ASCII letters and digits of `name`, at most 32 characters.
fn label(name: &str) -> String {
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(20).collect();
    if clean.is_empty() { "NyaRemote".into() } else { format!("NyaRemote-{clean}") }
}

impl Mount {
    /// Mount the client's folders (answered over `conn`) on a free drive letter.
    pub fn start(conn: Connection, rt: tokio::runtime::Handle, client_name: &str) -> Result<Mount> {
        let api = api()?;
        let point = free_drive().ok_or_else(|| anyhow!("被控端没有空闲的盘符"))?;
        let (mounted_tx, mounted_rx) = mpsc::channel();
        let fs = Arc::new(RemoteFs { conn, rt, fuse: AtomicPtr::new(std::ptr::null_mut()), mounted: Mutex::new(Some(mounted_tx)) });
        let opts = format!(
            "uid=-1,gid=-1,umask=0,FileInfoTimeout=1000,DirInfoTimeout=1000,VolumeInfoTimeout=5000,volname={}",
            label(client_name)
        );
        let args: Vec<CString> = ["nya-remote-folders", "-o", &opts, &point].iter().map(|a| CString::new(*a).unwrap()).collect();
        let (done_tx, done) = mpsc::channel();
        let data = Arc::into_raw(fs.clone()) as usize;
        std::thread::Builder::new().name("nya-folders".into()).spawn(move || {
            let ops = operations();
            let mut argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
            // SAFETY: argv and ops outlive the call; `data` is an Arc<RemoteFs>
            // reference released below, after WinFsp has stopped calling us.
            let code = unsafe {
                (api.main_real)(&ENV, argv.len() as c_int, argv.as_mut_ptr(), &ops, std::mem::size_of::<FuseOperations>(), data as *mut c_void)
            };
            unsafe { drop(Arc::from_raw(data as *const RemoteFs)) };
            let _ = done_tx.send(code);
        })?;
        match mounted_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(()) => Ok(Mount { fs, done, point }),
            Err(_) => match done.try_recv() {
                Ok(code) => bail!("WinFsp 挂载 {point} 失败（代码 {code}）"),
                Err(_) => {
                    let m = Mount { fs, done, point: point.clone() };
                    m.unmount();
                    bail!("WinFsp 挂载 {point} 超时")
                }
            },
        }
    }

    /// Unmount and wait (briefly) for WinFsp to finish.
    pub fn unmount(&self) {
        let fuse = self.fs.fuse.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if fuse.is_null() {
            return;
        }
        if let Ok(api) = api() {
            // SAFETY: the `struct fuse *` WinFsp handed to init, still mounted.
            unsafe { (api.exit)(&ENV, fuse) };
        }
        match self.done.recv_timeout(Duration::from_secs(10)) {
            Ok(code) => tracing::info!("unmounted {} ({code})", self.point),
            Err(_) => tracing::warn!("unmounting {} is taking long", self.point),
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.unmount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_winfsp_x64() {
        // Offsets from WinFsp's headers (64-bit Windows).
        assert_eq!(std::mem::size_of::<FuseStat>(), 128);
        assert_eq!(std::mem::offset_of!(FuseStat, st_size), 40);
        assert_eq!(std::mem::offset_of!(FuseStat, st_blocks), 104);
        assert_eq!(std::mem::size_of::<FuseStatvfs>(), 88);
        assert_eq!(std::mem::offset_of!(FuseContext, private_data), 24);
        assert_eq!(std::mem::size_of::<FspFuseEnv>(), 72);
        // 38 pointers, the flags word (padded), then 19 more pointers.
        assert_eq!(std::mem::offset_of!(FuseOperations, flags), 38 * 8);
        assert_eq!(std::mem::offset_of!(FuseOperations, ioctl), 39 * 8);
        assert_eq!(std::mem::size_of::<FuseOperations>(), 58 * 8);
    }

    #[test]
    fn stat_and_names() {
        let mut st = FuseStat::default();
        fill_stat(&pb::FsAttr { size: 1000, mtime_us: 1_500_000, ..Default::default() }, &mut st);
        assert_eq!(st.st_mode, S_IFREG | 0o777);
        assert_eq!((st.st_mtim.tv_sec, st.st_mtim.tv_nsec), (1, 500_000_000));
        assert_eq!(st.st_blocks, 2);
        assert_eq!(to_us(&ts(-1_500_000)), -1_500_000);
        assert_eq!(label("办公室 PC"), "NyaRemote-PC");
        assert_eq!(label("小明"), "NyaRemote");
    }
}
