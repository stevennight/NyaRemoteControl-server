//! The client's shared folders as a drive on the host (FEATURE_FOLDER_MOUNT),
//! through WinFsp's FUSE layer.
//!
//! WinFsp (optional component, a signed file system driver) is loaded at run
//! time from its install directory, so the host works without it. Every file
//! system call is answered by the client over an FS stream
//! (`nya_transport::folders::call`); WinFsp caches file and directory
//! information for a second to keep Explorer responsive over the network.
//!
//! The drive is served by a child process of the service (`nya-server-svc.exe
//! folders`, see [`Mount`]) that relays every call to the service over its
//! stdin / stdout. Run as SYSTEM, the drive letter is global: the user sees
//! it in their session. Files get mode 0777 (Everyone may read and write); the
//! client enforces read-only folders.
//!
//! The types below follow WinFsp's `inc/fuse/*.h` for 64-bit Windows.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use prost::Message;
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
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
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

// ---------------------------------------------------------------- service ↔ mount process

// Frames on the mount process's stdin / stdout: kind (u8), id (u64 LE),
// payload length (u32 LE), payload.
const TO_SERVICE_REQUEST: u8 = 1; // FsRequest; answered under the same id
const TO_SERVICE_MOUNTED: u8 = 2; // the drive letter exists
const TO_MOUNT_REPLY: u8 = 1; // FsReply
const TO_MOUNT_FAILED: u8 = 2; // the client did not answer
const MAX_FRAME: usize = 2 * nya_proto::MAX_MESSAGE_LEN;

fn write_frame(w: &mut impl Write, kind: u8, id: u64, payload: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(13 + payload.len());
    b.push(kind);
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    w.write_all(&b)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, u64, Vec<u8>)> {
    let mut h = [0u8; 13];
    r.read_exact(&mut h)?;
    let id = u64::from_le_bytes(h[1..9].try_into().unwrap());
    let len = u32::from_le_bytes(h[9..13].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut p = vec![0; len];
    r.read_exact(&mut p)?;
    Ok((h[0], id, p))
}

/// Is `point` ("Z:") a drive letter this process sees?
fn drive_exists(point: &str) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let Some(i) = point.bytes().next().map(|c| c.to_ascii_uppercase().wrapping_sub(b'A')) else { return false };
    // SAFETY: no arguments.
    i < 26 && unsafe { GetLogicalDrives() } & (1 << i) != 0
}

// ---------------------------------------------------------------- the file system (mount process)

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

/// `struct fuse *` while WinFsp runs the file system (to stop it).
#[derive(Default)]
struct FuseSlot {
    ptr: usize,
    stopping: bool,
}

/// The mounted file system, reached from WinFsp's threads. Every call goes
/// to the service over stdout and is answered on stdin.
struct PipeFs {
    out: Mutex<io::Stdout>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, mpsc::Sender<Option<pb::FsReply>>>>,
    /// The service closed stdin (unmount) or went away.
    closed: AtomicBool,
    fuse: Mutex<FuseSlot>,
    inited: Mutex<Option<mpsc::Sender<()>>>,
}

static FS: OnceLock<PipeFs> = OnceLock::new();

fn fs() -> &'static PipeFs {
    FS.get().expect("mount process")
}

impl PipeFs {
    fn call(&self, path: &str, op: Op) -> std::result::Result<pb::FsReply, c_int> {
        let req = pb::FsRequest { path: path.to_owned(), op: Some(op) };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        // Checked after registering: closing empties `waiting` after setting it.
        let reply = if self.closed.load(Ordering::SeqCst) {
            None
        } else if write_frame(&mut *self.out.lock().unwrap(), TO_SERVICE_REQUEST, id, &req.encode_to_vec()).is_err() {
            None
        } else {
            // The service gives up after 30 s and says so.
            rx.recv_timeout(Duration::from_secs(40)).ok().flatten()
        };
        self.waiting.lock().unwrap().remove(&id);
        let Some(reply) = reply else {
            tracing::debug!("folder request {path}: no answer");
            return Err(-EIO);
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

    /// Answers from the service, until it closes stdin; then unmount.
    fn read_replies(&self) {
        let mut input = io::stdin().lock();
        while let Ok((kind, id, payload)) = read_frame(&mut input) {
            let reply = (kind == TO_MOUNT_REPLY).then(|| pb::FsReply::decode(&payload[..]).ok()).flatten();
            if let Some(tx) = self.waiting.lock().unwrap().remove(&id) {
                let _ = tx.send(reply);
            }
        }
        tracing::info!("service closed the pipe: unmounting");
        self.closed.store(true, Ordering::SeqCst);
        self.waiting.lock().unwrap().clear(); // waiting calls fail
        self.stop_if_closed();
    }

    /// Stop WinFsp's loop once the service is gone (WinFsp may not have
    /// started it yet; called again from `init`).
    fn stop_if_closed(&self) {
        if !self.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut f = self.fuse.lock().unwrap();
        if f.ptr != 0 && !f.stopping {
            f.stopping = true;
            if let Ok(api) = api() {
                // SAFETY: WinFsp's `struct fuse *`, cleared by `destroy` (under
                // this lock) before WinFsp frees it.
                unsafe { (api.exit)(&ENV, f.ptr as *mut c_void) };
            }
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
    let fs = fs();
    fs.fuse.lock().unwrap().ptr = (*ctx).fuse as usize;
    if let Some(tx) = fs.inited.lock().unwrap().take() {
        let _ = tx.send(());
    }
    // Unmounted before WinFsp got here: it stops right after starting.
    fs.stop_if_closed();
    (*ctx).private_data
}

unsafe extern "C" fn op_destroy(_data: *mut c_void) {
    // WinFsp frees `struct fuse` next.
    fs().fuse.lock().unwrap().ptr = 0;
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
        destroy: Some(op_destroy),
        ..Default::default()
    }
}

/// `nya-server-svc.exe folders`: mount the drive and serve it until the
/// service closes stdin. Returns WinFsp's exit code.
pub fn run_mount_process(point: &str, volname: &str) -> Result<i32> {
    let api = api()?;
    let (inited_tx, inited) = mpsc::channel();
    FS.get_or_init(|| PipeFs {
        out: Mutex::new(io::stdout()),
        next_id: AtomicU64::new(1),
        waiting: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
        fuse: Mutex::new(FuseSlot::default()),
        inited: Mutex::new(Some(inited_tx)),
    });
    std::thread::Builder::new().name("replies".into()).spawn(|| fs().read_replies())?;
    // `init` runs before WinFsp creates the volume and the drive letter:
    // "mounted" only once the letter is really there.
    let watched = point.to_owned();
    std::thread::Builder::new().name("mount watch".into()).spawn(move || {
        if inited.recv().is_err() {
            return;
        }
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while !drive_exists(&watched) {
            if std::time::Instant::now() >= until {
                tracing::warn!("{watched} did not appear in 20 s");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!("{watched} mounted");
        let _ = write_frame(&mut *fs().out.lock().unwrap(), TO_SERVICE_MOUNTED, 0, &[]);
    })?;
    let opts =
        format!("uid=-1,gid=-1,umask=0,FileInfoTimeout=1000,DirInfoTimeout=1000,VolumeInfoTimeout=5000,volname={volname}");
    let args: Vec<CString> = ["nya-remote-folders", "-o", &opts, point].iter().map(|a| CString::new(*a).unwrap()).collect();
    let mut argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
    let ops = operations();
    // SAFETY: argv and ops outlive the call; the file system is the global `FS`.
    let code = unsafe {
        (api.main_real)(&ENV, argv.len() as c_int, argv.as_mut_ptr(), &ops, std::mem::size_of::<FuseOperations>(), std::ptr::null_mut())
    };
    tracing::info!("WinFsp finished ({code})");
    Ok(code)
}

// ---------------------------------------------------------------- mounting (service)

/// First free drive letter from Z: down to D:.
fn free_drive() -> Option<String> {
    (3..26u8).rev().map(|i| format!("{}:", (b'A' + i) as char)).find(|p| !drive_exists(p))
}

/// Volume label: ASCII letters and digits of `name`, at most 32 characters.
fn label(name: &str) -> String {
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(20).collect();
    if clean.is_empty() { "NyaRemote".into() } else { format!("NyaRemote-{clean}") }
}

enum ToMount {
    Frame(u8, u64, Vec<u8>),
    /// Close stdin: the mount process unmounts and exits.
    Close,
}

enum Event {
    Mounted,
    /// The mount process exited (its exit code).
    Ended(Option<i32>),
}

/// A mounted drive, served by a mount process; unmounted by
/// [`Mount::unmount`] (or on drop).
///
/// WinFsp's FUSE loop runs `FspServiceRun` on a thread of its own and ends
/// the file system when that thread does. In a process that already is a
/// service (ours) that thread fails at once (the process's service control
/// dispatcher is taken), so the drive vanished right after it was created.
/// A child process is no service: WinFsp runs it in console mode, as with
/// WinFsp's own launcher. As LocalSystem's child its drive letter is still
/// global (visible in the user's session).
pub struct Mount {
    pub point: String,
    child: Arc<Mutex<Child>>,
    to_mount: mpsc::Sender<ToMount>,
    events: Mutex<mpsc::Receiver<Event>>,
    closing: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
}

impl Mount {
    /// Mount the client's folders (answered over `conn`) on a free drive
    /// letter. `on_end` hears why if the drive goes away later on its own.
    pub fn start(
        conn: Connection,
        rt: tokio::runtime::Handle,
        client_name: &str,
        on_end: impl FnOnce(String) + Send + 'static,
    ) -> Result<Mount> {
        use std::os::windows::process::CommandExt;
        api()?; // installed? (the mount process loads it itself)
        let point = free_drive().ok_or_else(|| anyhow!("被控端没有空闲的盘符"))?;
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args(["folders", "--point", &point, "--label", &label(client_name)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|e| anyhow!("无法启动挂载进程：{e}"))?;
        let (stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let child = Arc::new(Mutex::new(child));

        std::thread::Builder::new().name("folders stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                tracing::warn!("mount process: {line}");
            }
        })?;

        let (to_mount, rx) = mpsc::channel::<ToMount>();
        std::thread::Builder::new().name("folders out".into()).spawn(move || {
            let mut w = stdin;
            for m in rx {
                match m {
                    ToMount::Frame(kind, id, p) => {
                        if write_frame(&mut w, kind, id, &p).is_err() {
                            break;
                        }
                    }
                    ToMount::Close => break,
                }
            }
        })?;

        let (events_tx, events) = mpsc::channel();
        let (closing, ended) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        {
            let (to_mount, child, closing, ended, point) = (to_mount.clone(), child.clone(), closing.clone(), ended.clone(), point.clone());
            std::thread::Builder::new().name("folders in".into()).spawn(move || {
                let mut input = BufReader::new(stdout);
                let mut mounted = false;
                while let Ok((kind, id, payload)) = read_frame(&mut input) {
                    match kind {
                        TO_SERVICE_REQUEST => {
                            let (conn, to_mount) = (conn.clone(), to_mount.clone());
                            rt.spawn(async move {
                                let frame = match answer(&conn, &payload).await {
                                    Some(r) => ToMount::Frame(TO_MOUNT_REPLY, id, r.encode_to_vec()),
                                    None => ToMount::Frame(TO_MOUNT_FAILED, id, Vec::new()),
                                };
                                let _ = to_mount.send(frame);
                            });
                        }
                        TO_SERVICE_MOUNTED => {
                            mounted = true;
                            let _ = events_tx.send(Event::Mounted);
                        }
                        _ => {}
                    }
                }
                drop(to_mount);
                let code = wait_exit(&child);
                ended.store(true, Ordering::SeqCst);
                let _ = events_tx.send(Event::Ended(code));
                if mounted && !closing.load(Ordering::SeqCst) {
                    tracing::warn!("{point} went away (mount process ended: {code:?})");
                    on_end(format!("被控端的 {point} 盘意外卸载了（WinFsp 代码 {}）", code.map_or("?".into(), |c| c.to_string())));
                }
            })?;
        }

        let m = Mount { point, child, to_mount, events: Mutex::new(events), closing, ended };
        let first = m.events.lock().unwrap().recv_timeout(Duration::from_secs(25));
        match first {
            Ok(Event::Mounted) => Ok(m),
            Ok(Event::Ended(code)) => {
                m.closing.store(true, Ordering::SeqCst);
                bail!("WinFsp 挂载 {} 失败（代码 {}）", m.point, code.map_or("?".into(), |c| c.to_string()))
            }
            Err(_) => {
                m.unmount();
                bail!("WinFsp 挂载 {} 超时", m.point)
            }
        }
    }

    /// The mount process is gone (the drive with it).
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// Unmount and wait (briefly) for the mount process to finish.
    pub fn unmount(&self) {
        if self.closing.swap(true, Ordering::SeqCst) || self.ended.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.to_mount.send(ToMount::Close);
        let until = std::time::Instant::now() + Duration::from_secs(10);
        let events = self.events.lock().unwrap();
        loop {
            match events.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
                Ok(Event::Ended(code)) => {
                    tracing::info!("unmounted {} ({code:?})", self.point);
                    return;
                }
                Ok(Event::Mounted) => {}
                Err(_) => break,
            }
        }
        tracing::warn!("unmounting {} is taking long: ending the mount process", self.point);
        let _ = self.child.lock().unwrap().kill();
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// Ask the client (FsRequest bytes from the mount process); `None` = no answer.
async fn answer(conn: &Connection, req: &[u8]) -> Option<pb::FsReply> {
    let req = pb::FsRequest::decode(req).ok()?;
    match tokio::time::timeout(Duration::from_secs(30), nya_transport::folders::call(conn, &req)).await {
        Ok(Ok(r)) => Some(r),
        Ok(Err(e)) => {
            tracing::debug!("folder request {}: {e:#}", req.path);
            None
        }
        Err(_) => {
            tracing::warn!("folder request {}: no answer in 30 s", req.path);
            None
        }
    }
}

/// The exit code of a process that closed its stdout (waited for briefly).
fn wait_exit(child: &Mutex<Child>) -> Option<i32> {
    for _ in 0..100 {
        if let Ok(Some(s)) = child.lock().unwrap().try_wait() {
            return s.code();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
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
    fn frames_and_drives() {
        let mut b = Vec::new();
        write_frame(&mut b, TO_SERVICE_REQUEST, 7, b"abc").unwrap();
        write_frame(&mut b, TO_SERVICE_MOUNTED, 0, &[]).unwrap();
        let mut r = &b[..];
        assert_eq!(read_frame(&mut r).unwrap(), (TO_SERVICE_REQUEST, 7, b"abc".to_vec()));
        assert_eq!(read_frame(&mut r).unwrap(), (TO_SERVICE_MOUNTED, 0, Vec::new()));
        assert!(read_frame(&mut r).is_err(), "end of stream");
        let mut huge = vec![1u8];
        huge.extend_from_slice(&0u64.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame(&mut &huge[..]).is_err());
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(drive_exists(&system) && drive_exists(&system.to_lowercase()));
        assert!(free_drive().is_some_and(|p| !drive_exists(&p)));
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
