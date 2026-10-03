//! Client side of file transfer: uploads (drag & drop / "发送文件"),
//! downloads of files offered by the host, clipboard images, and files copied
//! on one side and pasted on the other ([`ClipFiles`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nya_proto::pb;
use nya_transport::files::{self, FileLink};
use nya_transport::quinn::RecvStream;

use crate::events::{TransferUpdate, Ui, UiEvent};

/// Rate-limits progress events to the UI.
struct Progress {
    ui: Ui,
    update: TransferUpdate,
    last: Instant,
}

impl Progress {
    fn new(ui: Ui, id: u64, upload: bool, total: u64) -> Self {
        Self {
            ui,
            update: TransferUpdate { id, upload, name: String::new(), done: 0, total, finished: None, folder: None },
            last: Instant::now() - Duration::from_secs(1),
        }
    }

    fn add(&mut self, n: u64) {
        self.set(self.update.done + n);
    }

    /// Bytes done so far (of the whole batch).
    fn set(&mut self, done: u64) {
        self.update.done = done;
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            self.ui.send(UiEvent::Transfer(self.update.clone()));
        }
    }

    fn finish(mut self, r: Result<String, String>, folder: Option<PathBuf>) {
        self.update.finished = Some(r);
        self.update.folder = folder;
        self.ui.send(UiEvent::Transfer(self.update));
    }
}

pub fn new_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    nya_proto::now_us() ^ (N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) << 48)
}

/// Upload files; the host confirms with a FileResult when the batch is saved.
pub async fn upload(link: FileLink, paths: Vec<PathBuf>, ui: Ui) {
    let files: Vec<(PathBuf, u64)> = paths
        .into_iter()
        .filter_map(|p| match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => Some((p, m.len())),
            _ => {
                tracing::info!("skipping {} (folder or unreadable)", p.display());
                None
            }
        })
        .collect();
    let id = new_id();
    let total: u64 = files.iter().map(|f| f.1).sum();
    let mut prog = Progress::new(ui.clone(), id, true, total);
    if files.is_empty() {
        prog.finish(Err("没有可发送的文件（暂不支持文件夹）".into()), None);
        return;
    }
    let count = files.len() as u32;
    for (i, (p, size)) in files.iter().enumerate() {
        prog.update.name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let h = pb::FileHeader {
            transfer_id: id,
            name: prog.update.name.clone(),
            size: *size,
            purpose: pb::FilePurpose::Save as i32,
            index: i as u32,
            count,
            path: String::new(),
        };
        if let Err(e) = link.send_file(h, p, None, |n| prog.add(n)).await {
            prog.finish(Err(format!("发送 {} 失败：{e:#}", p.display())), None);
            return;
        }
    }
    // Sent; the host's FileResult reports where it was saved.
    prog.update.name = format!("{count} 个文件，等待被控端确认…");
    ui.send(UiEvent::Transfer(prog.update));
}

pub async fn send_image(link: FileLink, dib: Vec<u8>) {
    let h = pb::FileHeader {
        transfer_id: new_id(),
        name: "clipboard.dib".into(),
        size: dib.len() as u64,
        purpose: pb::FilePurpose::ClipboardImage as i32,
        index: 0,
        count: 1,
        path: String::new(),
    };
    if let Err(e) = link.send_bytes(h, &dib).await {
        tracing::debug!("clipboard image: {e:#}");
    }
}

pub type PasteReply = std::sync::mpsc::Sender<Result<Vec<PathBuf>, String>>;

/// Copied files between the two computers (FEATURE_CLIPBOARD_FILES): what the
/// network tasks share. Lives as long as the session (across reconnects).
pub struct ClipFiles {
    /// Host files on our clipboard; fetched into `cache` when pasted.
    pub incoming: nya_transport::clipfiles::Incoming,
    /// Files copied here, offered to the host.
    pub outgoing: Mutex<nya_transport::clipfiles::Outgoing>,
    cache: PathBuf,
    /// Pastes waiting for their files.
    waiters: Mutex<HashMap<u64, Vec<PasteReply>>>,
    /// Offer id -> (total bytes, bytes received so far).
    progress: Mutex<HashMap<u64, (u64, u64)>>,
}

impl ClipFiles {
    pub fn new() -> Self {
        let cache = nya_win::shell::clipboard_cache_dir(None).unwrap_or_else(|| std::env::temp_dir().join("NyaRemoteControl").join("clipboard"));
        let c = cache.clone();
        std::thread::spawn(move || files::prune_cache(&c));
        Self { incoming: Default::default(), outgoing: Default::default(), cache, waiters: Default::default(), progress: Default::default() }
    }

    /// The host copied files.
    pub fn register(&self, o: &pb::FileOffer) {
        self.incoming.register(o, &self.cache);
        let total = o.files.iter().map(|f| f.size).sum();
        self.progress.lock().unwrap().insert(o.transfer_id, (total, 0));
    }

    /// A paste asks for `id` (again): its progress starts over.
    pub fn restart(&self, id: u64) {
        if let Some(p) = self.progress.lock().unwrap().get_mut(&id) {
            p.1 = 0;
        }
    }

    pub fn wait(&self, id: u64, reply: PasteReply) {
        self.waiters.lock().unwrap().entry(id).or_default().push(reply);
    }

    /// Tell every paste waiting for `id`.
    pub fn finish(&self, id: u64, r: Result<Vec<PathBuf>, String>) {
        for w in self.waiters.lock().unwrap().remove(&id).unwrap_or_default() {
            let _ = w.send(r.clone());
        }
    }

    /// The connection ended: nothing that was requested will arrive.
    pub fn fail_all(&self, msg: &str) {
        for id in self.incoming.fail_all(msg) {
            self.finish(id, Err(msg.to_owned()));
        }
    }
}

impl Default for ClipFiles {
    fn default() -> Self {
        Self::new()
    }
}

/// Send files the host asked for because they are being pasted there.
/// A failure is reported to the host through `fail`.
pub async fn send_clipboard_files(
    link: FileLink,
    id: u64,
    items: Vec<files::Item>,
    ui: Ui,
    fail: tokio::sync::mpsc::UnboundedSender<pb::ControlMsg>,
) {
    let total = items.iter().map(|i| i.size).sum();
    let count = items.iter().filter(|i| !i.is_dir).count();
    let mut prog = Progress::new(ui, id, true, total);
    let r = nya_transport::clipfiles::send_items_with(&link, id, &items, pb::FilePurpose::Clipboard, None, |name, n| {
        if prog.update.name != name {
            prog.update.name = name.to_owned();
        }
        prog.add(n);
    })
    .await;
    match r {
        Ok(()) => prog.finish(Ok(format!("已发送 {count} 个文件（在被控端粘贴）")), None),
        Err(e) => {
            let msg = format!("客户端发送文件失败：{e:#}");
            let _ = fail.send(pb::ControlMsg {
                msg: Some(pb::control_msg::Msg::FileResult(pb::FileResult { transfer_id: id, ok: false, message: msg.clone(), saved_to: String::new() })),
            });
            prog.finish(Err(msg), None);
        }
    }
}

/// Files of a download batch received so far.
#[derive(Default)]
pub struct Downloads {
    batches: Mutex<HashMap<u64, (Vec<PathBuf>, u64)>>,
}

/// A FILE stream from the host (after the type varint).
pub async fn receive(
    mut r: RecvStream,
    ui: Ui,
    downloads: Arc<Downloads>,
    clip: Arc<ClipFiles>,
    cancels: Arc<files::Cancels>,
    (files_on, images_on, clip_on, print_on): (bool, bool, bool, bool),
) {
    let h = match files::read_header(&mut r).await {
        Ok(h) => h,
        Err(e) => return tracing::warn!("file header: {e:#}"),
    };
    receive_body(h, &mut r, ui, downloads, clip, cancels, (files_on, images_on, clip_on, print_on)).await
}

/// A file from the host (QUIC FILE stream or the TCP file channel).
/// Returning without reading it refuses it.
pub async fn receive_body<R: tokio::io::AsyncRead + Unpin>(
    h: pb::FileHeader,
    r: &mut R,
    ui: Ui,
    downloads: Arc<Downloads>,
    clip: Arc<ClipFiles>,
    cancels: Arc<files::Cancels>,
    (files_on, images_on, clip_on, print_on): (bool, bool, bool, bool),
) {
    // Cancelled (here or by the host): reads fail, partial files are removed.
    let r = &mut files::Cancellable::new(r, cancels.flag(h.transfer_id));
    match pb::FilePurpose::try_from(h.purpose).unwrap_or(pb::FilePurpose::Unspecified) {
        pb::FilePurpose::ClipboardImage if images_on => match files::receive_to_vec(r, &h, files::MAX_IMAGE_BYTES).await {
            Ok(dib) => ui.send(UiEvent::ClipboardImage(dib)),
            Err(e) => tracing::debug!("clipboard image: {e:#}"),
        },
        pb::FilePurpose::Clipboard if clip_on => {
            let id = h.transfer_id;
            let Some(root) = clip.incoming.root(id) else {
                return;
            };
            let total = clip.progress.lock().unwrap().get(&id).map_or(0, |p| p.0);
            let mut prog = Progress::new(ui.clone(), id, false, total);
            prog.update.name = if h.path.is_empty() { h.name.clone() } else { h.path.clone() };
            // Several files arrive at once: one count for the whole batch.
            let res = files::receive_to_tree(r, &h, &root, |n| {
                let done = clip.progress.lock().unwrap().get_mut(&id).map_or(0, |p| {
                    p.1 += n;
                    p.1
                });
                prog.set(done);
            })
            .await;
            prog.update.done = clip.progress.lock().unwrap().get(&id).map_or(0, |p| p.1);
            let res = res.map(|_| ()).map_err(|e| format!("接收 {} 失败：{e:#}", prog.update.name));
            if let Some(done) = clip.incoming.file_done(id, res) {
                match &done {
                    Ok(items) => prog.finish(Ok(format!("已接收 {} 项，正在粘贴", items.len())), Some(root)),
                    Err(e) => prog.finish(Err(e.clone()), None),
                }
                clip.finish(id, done);
            }
        }
        pb::FilePurpose::Print if print_on => {
            let dir = nya_win::shell::receive_dir(None).unwrap_or_else(|| std::env::temp_dir().join("NyaRemoteControl")).join("打印");
            match files::receive_to_dir(r, &h, &dir, |_| {}).await {
                Ok(p) => {
                    tracing::info!("print job from the host: {}", p.display());
                    ui.send(UiEvent::PrintJob(p));
                }
                Err(e) => tracing::warn!("receiving print job {}: {e:#}", h.name),
            }
        }
        pb::FilePurpose::Save if files_on => {
            let dir = nya_win::shell::receive_dir(None).unwrap_or_else(|| std::env::temp_dir().join("NyaRemoteControl"));
            let already = downloads.batches.lock().unwrap().get(&h.transfer_id).map(|b| b.1).unwrap_or(0);
            let mut prog = Progress::new(ui.clone(), h.transfer_id, false, 0);
            prog.update.name = h.name.clone();
            prog.update.done = already;
            let res = files::receive_to_dir(r, &h, &dir, |n| prog.add(n)).await;
            match res {
                Ok(p) => {
                    let done = {
                        let mut b = downloads.batches.lock().unwrap();
                        let e = b.entry(h.transfer_id).or_default();
                        e.0.push(p);
                        e.1 += h.size;
                        if h.index + 1 >= h.count {
                            b.remove(&h.transfer_id).map(|x| x.0)
                        } else {
                            None
                        }
                    };
                    if let Some(paths) = done {
                        let n = paths.len();
                        let clip = tokio::task::spawn_blocking(move || nya_win::clipboard::set_files(&paths)).await;
                        let note = if matches!(clip, Ok(Ok(()))) { "，已放入剪贴板，可直接粘贴" } else { "" };
                        prog.finish(Ok(format!("已下载 {n} 个文件{note}")), Some(dir));
                    }
                }
                Err(e) => {
                    downloads.batches.lock().unwrap().remove(&h.transfer_id);
                    prog.finish(Err(format!("下载 {} 失败：{e:#}", h.name)), None);
                }
            }
        }
        _ => {}
    }
}
