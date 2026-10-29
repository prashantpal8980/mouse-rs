//! Files still arriving from the peer, offered on the Windows clipboard as *virtual files*.
//!
//! The Windows counterpart of the Linux streaming folder. A file list on the Windows
//! clipboard (`CF_HDROP`) must name files that exist, and a file still on the peer does not:
//! Explorer failed the paste at once. So the clipboard instead carries an OLE data object
//! offering `FileGroupDescriptorW` (names, sizes, folders) and `FileContents` (one
//! `IStream` per file). This is how Explorer pastes an attachment out of an email. Explorer
//! reads each stream and writes the bytes straight where the user pasted, with its own
//! progress dialog. Nothing is spooled here for large files; see [`crate::pipe`].
//!
//! OLE requires the object to live on a single-threaded apartment that pumps messages, so
//! it has a thread of its own. Calls from Explorer are delivered to that thread. A read
//! waiting for the network keeps pumping messages while it waits, so the clipboard stays
//! responsive to other programs throughout.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{implement, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED, STG_E_INUSE,
    S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_READFAULT,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl,
    IStream, IStream_Impl, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM,
    STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL,
    TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    OleInitialize, OleIsCurrentClipboard, OleSetClipboard, OleUninitialize, ReleaseStgMedium, DROPEFFECT_COPY,
};
use windows::Win32::UI::Shell::{
    IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc, CFSTR_FILECONTENTS,
    CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT,
};

use crate::cache::{self, BlobCache};
use crate::files::PasteSignal;
use crate::pipe::StreamPipe;
use crate::remote_read::{Got, Reader, RemoteSource};
use crate::FileSet;

/// How long one wait inside a read lasts before messages are pumped again.
const SLICE: Duration = Duration::from_millis(50);

/// One entry in the descriptor list: a folder, or a file with where to read it from.
#[derive(Clone)]
struct Item {
    /// Relative path with backslashes, as Explorer expects: "Album\\Disc 2\\01.flac".
    rel: String,
    dir: bool,
    size: u64,
    src: Option<RemoteSource>,
}

enum Cmd {
    Offer(Vec<Item>, Sender<bool>),
    Quit,
}

/// The thread that owns the OLE clipboard object. Clears the offer and stops when dropped.
pub struct VirtualClipboard {
    tx: Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VirtualClipboard {
    /// Start the owner thread. `None` (logged) if OLE could not be initialised; the caller
    /// then falls back to publishing paths, as before.
    pub fn start(cache: Arc<BlobCache>, stall: Duration) -> Option<VirtualClipboard> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let thread = std::thread::Builder::new()
            .name("mouse-rs virtual clipboard".into())
            .spawn(move || owner_thread(rx, ready_tx, cache, stall))
            .ok()?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => {
                tracing::info!("virtual clipboard files ready — large pastes start at once in Explorer");
                Some(VirtualClipboard { tx, thread: Some(thread) })
            }
            _ => {
                tracing::warn!("virtual clipboard files unavailable; a paste will need the whole file first");
                None
            }
        }
    }

    /// Put `set` on the clipboard as virtual files. Returns a key identifying the offer.
    pub fn offer(
        &self,
        entry: u64,
        set: &FileSet,
        signal: Arc<PasteSignal>,
        pipes: &HashMap<String, Arc<StreamPipe>>,
    ) -> Option<u64> {
        let items = items_for(set, &signal, pipes);
        if items.is_empty() {
            return None;
        }
        let (reply_tx, reply_rx) = channel();
        self.tx.send(Cmd::Offer(items, reply_tx)).ok()?;
        match reply_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => Some(entry ^ 0x5649_5254_5541_4c00), // "VIRTUAL": distinct from list keys
            _ => None,
        }
    }
}

impl Drop for VirtualClipboard {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The descriptor list for a set: folders first (so each exists before what goes in it),
/// then files, all as relative paths.
fn items_for(set: &FileSet, signal: &Arc<PasteSignal>, pipes: &HashMap<String, Arc<StreamPipe>>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut dirs: Vec<String> = set
        .dirs
        .iter()
        .map(|d| cache::safe_components(d).join("\\"))
        .filter(|d| !d.is_empty())
        .collect();
    dirs.sort();
    dirs.dedup();
    for d in dirs {
        items.push(Item { rel: d, dir: true, size: 0, src: None });
    }
    for f in &set.files {
        let rel = if f.rel.is_empty() { f.name.as_str() } else { f.rel.as_str() };
        let rel = cache::safe_components(rel).join("\\");
        if rel.is_empty() {
            continue;
        }
        items.push(Item {
            rel,
            dir: false,
            size: f.size,
            src: Some(RemoteSource {
                hash: f.hash.clone(),
                size: f.size,
                signal: signal.clone(),
                pipe: pipes.get(&f.hash).cloned(),
            }),
        });
    }
    items
}

fn owner_thread(rx: Receiver<Cmd>, ready: Sender<bool>, cache: Arc<BlobCache>, stall: Duration) {
    // SAFETY: OLE is initialised once on this thread and uninitialised before it ends.
    if unsafe { OleInitialize(None) }.is_err() {
        let _ = ready.send(false);
        return;
    }
    let formats = Formats::register();
    let _ = ready.send(true);
    let mut current: Option<IDataObject> = None;
    loop {
        // Wake for window messages (OLE's clipboard calls arrive that way) or every slice.
        // SAFETY: plain message-queue calls on this thread's own queue.
        unsafe {
            let _ = MsgWaitForMultipleObjects(None, false, SLICE.as_millis() as u32, QS_ALLINPUT);
        }
        pump_messages();
        match rx.try_recv() {
            Ok(Cmd::Offer(items, reply)) => {
                let count = items.iter().filter(|i| !i.dir).count();
                let obj: IDataObject = DataObj {
                    items,
                    cache: cache.clone(),
                    stall,
                    formats,
                    async_mode: AtomicBool::new(true),
                    in_op: AtomicBool::new(false),
                }
                .into();
                // SAFETY: called on the OLE-initialised thread that will keep pumping.
                let ok = unsafe { OleSetClipboard(&obj) }.is_ok();
                if ok {
                    crate::files::mark_own_write();
                    tracing::info!(files = count, "PUBLISHED virtual files to the Windows clipboard — Explorer's Paste pulls them as they arrive");
                    current = Some(obj);
                } else {
                    tracing::warn!("could not put virtual files on the clipboard");
                }
                let _ = reply.send(ok);
            }
            Ok(Cmd::Quit) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
    // A virtual file cannot outlive this process: leave the clipboard empty rather than
    // naming something nothing can produce.
    if let Some(obj) = current {
        // SAFETY: as above.
        unsafe {
            if OleIsCurrentClipboard(&obj).is_ok() {
                let _ = OleSetClipboard(None);
            }
        }
    }
    // SAFETY: pairs the OleInitialize above.
    unsafe { OleUninitialize() };
}

/// Dispatch whatever is queued for this thread.
fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop over this thread's own queue.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[derive(Clone, Copy)]
struct Formats {
    desc: u16,
    contents: u16,
    effect: u16,
}

impl Formats {
    fn register() -> Formats {
        // SAFETY: registering a named clipboard format has no preconditions.
        let reg = |name: PCWSTR| unsafe { RegisterClipboardFormatW(name) } as u16;
        Formats {
            desc: reg(CFSTR_FILEDESCRIPTORW),
            contents: reg(CFSTR_FILECONTENTS),
            effect: reg(CFSTR_PREFERREDDROPEFFECT),
        }
    }

    fn list(&self) -> [FORMATETC; 3] {
        let f = |cf: u16, tymed: i32, lindex: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed as u32,
        };
        [f(self.desc, TYMED_HGLOBAL.0, -1), f(self.contents, TYMED_ISTREAM.0, -1), f(self.effect, TYMED_HGLOBAL.0, -1)]
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObj {
    items: Vec<Item>,
    cache: Arc<BlobCache>,
    stall: Duration,
    formats: Formats,
    async_mode: AtomicBool,
    in_op: AtomicBool,
}

impl DataObj {
    /// `FILEGROUPDESCRIPTORW`: a count, then one `FILEDESCRIPTORW` per item.
    fn descriptor(&self) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        let one = std::mem::size_of::<FILEDESCRIPTORW>();
        let size = 4 + one * self.items.len();
        // SAFETY: the block is allocated, locked, filled within its size, and unlocked.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, size)?;
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                return Err(windows::core::Error::from(STG_E_READFAULT));
            }
            std::ptr::write_unaligned(p as *mut u32, self.items.len() as u32);
            for (i, item) in self.items.iter().enumerate() {
                let mut d = FILEDESCRIPTORW {
                    dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
                    dwFileAttributes: if item.dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
                    nFileSizeHigh: (item.size >> 32) as u32,
                    nFileSizeLow: item.size as u32,
                    ..Default::default()
                };
                for (j, c) in item.rel.encode_utf16().take(259).enumerate() {
                    d.cFileName[j] = c;
                }
                std::ptr::write_unaligned(p.add(4 + i * one) as *mut FILEDESCRIPTORW, d);
            }
            let _ = GlobalUnlock(h);
            Ok(h)
        }
    }

    fn effect(&self) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        // SAFETY: as in `descriptor`.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, 4)?;
            let p = GlobalLock(h) as *mut u32;
            if p.is_null() {
                return Err(windows::core::Error::from(STG_E_READFAULT));
            }
            std::ptr::write_unaligned(p, DROPEFFECT_COPY.0);
            let _ = GlobalUnlock(h);
            Ok(h)
        }
    }

    fn supports(&self, f: &FORMATETC) -> bool {
        let hg = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        let st = f.tymed & TYMED_ISTREAM.0 as u32 != 0;
        (f.cfFormat == self.formats.desc && hg)
            || (f.cfFormat == self.formats.contents && st)
            || (f.cfFormat == self.formats.effect && hg)
    }
}

fn hglobal_medium(h: windows::Win32::Foundation::HGLOBAL) -> STGMEDIUM {
    STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: ManuallyDrop::new(None) }
}

impl IDataObject_Impl for DataObj_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: COM guarantees a valid FORMATETC for the duration of the call.
        let f = unsafe { pformatetcin.as_ref() }.ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
        let hg = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        if f.cfFormat == self.formats.desc && hg {
            return Ok(hglobal_medium(self.descriptor()?));
        }
        if f.cfFormat == self.formats.effect && hg {
            return Ok(hglobal_medium(self.effect()?));
        }
        if f.cfFormat == self.formats.contents && f.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            let item = usize::try_from(f.lindex)
                .ok()
                .and_then(|i| self.items.get(i))
                .filter(|i| !i.dir)
                .ok_or_else(|| windows::core::Error::from(DV_E_LINDEX))?;
            let src = item.src.clone().ok_or_else(|| windows::core::Error::from(DV_E_LINDEX))?;
            if let Some(pipe) = &src.pipe {
                pipe.reopen();
            }
            // Asking for the contents is the paste: start the fetch now, unless it streams
            // through a pipe (its first read asks) or is already here.
            if src.pipe.is_none() && !self.cache.contains(&src.hash) {
                src.signal.failed.store(false, Ordering::SeqCst);
                src.signal.wanted.store(true, Ordering::SeqCst);
            }
            tracing::info!(name = %item.rel, bytes = item.size, "Explorer is pasting a virtual file; streaming it");
            let stream: IStream = VStream {
                size: src.size,
                cache: self.cache.clone(),
                stall: self.stall,
                pos: AtomicU64::new(0),
                tail: AtomicBool::new(false),
                reader: Mutex::new(Reader::new(src)),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(stream)) },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: as in GetData.
        match unsafe { pformatetc.as_ref() } {
            Some(f) if self.supports(f) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        // SAFETY: the out pointer is provided by the caller for us to fill.
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, _pformatetc: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> windows::core::Result<()> {
        // Explorer reports back ("Performed DropEffect", "Paste Succeeded"). Nothing to do
        // with it, but a medium handed over must be freed.
        if frelease.as_bool() && !pmedium.is_null() {
            // SAFETY: ownership was transferred to us by `frelease`.
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: the shell copies the array it is given.
        unsafe { SHCreateStdEnumFmtEtc(&self.formats.list()) }
    }

    fn DAdvise(&self, _f: *const FORMATETC, _advf: u32, _sink: windows::core::Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Lets Explorer copy on a thread of its own, keeping its window responsive while the
/// bytes arrive.
impl IDataObjectAsyncCapability_Impl for DataObj_Impl {
    fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
        self.async_mode.store(fdoopasync.as_bool(), Ordering::SeqCst);
        Ok(())
    }
    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.async_mode.load(Ordering::SeqCst).into())
    }
    fn StartOperation(&self, _pbcreserved: windows::core::Ref<IBindCtx>) -> windows::core::Result<()> {
        self.in_op.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_op.load(Ordering::SeqCst).into())
    }
    fn EndOperation(&self, _hresult: HRESULT, _pbcreserved: windows::core::Ref<IBindCtx>, _dweffects: u32) -> windows::core::Result<()> {
        self.in_op.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// One virtual file being read by Explorer.
#[implement(IStream)]
struct VStream {
    size: u64,
    cache: Arc<BlobCache>,
    stall: Duration,
    pos: AtomicU64,
    /// The last byte has been handed over: the paste has the whole file.
    tail: AtomicBool,
    reader: Mutex<Reader>,
}

impl Drop for VStream {
    fn drop(&mut self) {
        let Ok(reader) = self.reader.lock() else { return };
        let src = reader.source();
        if let Some(pipe) = &src.pipe {
            pipe.release();
            tracing::info!(hash = %src.hash, "Explorer closed the streamed file; stream stopped and its memory freed");
        } else if self.tail.load(Ordering::SeqCst) {
            // The paste has it all: our copy would be the "middle cache" nobody wants.
            self.cache.discard_pasted(&src.hash);
            tracing::info!(hash = %src.hash, "pasted copy complete; mouse-rs's own copy removed");
        }
    }
}

impl ISequentialStream_Impl for VStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        let pos = self.pos.load(Ordering::SeqCst);
        let set_read = |n: u32| {
            if !pcbread.is_null() {
                // SAFETY: the caller's out parameter.
                unsafe { *pcbread = n };
            }
        };
        if pos >= self.size || cb == 0 {
            set_read(0);
            return S_FALSE;
        }
        // A second call arriving while this one waits (messages are pumped meanwhile) is
        // told to come back, rather than deadlocking on the reader.
        let Ok(mut reader) = self.reader.try_lock() else { return STG_E_INUSE };
        loop {
            match reader.read(&self.cache, pos, u64::from(cb), self.stall, SLICE) {
                Got::Data(bytes) => {
                    let n = bytes.len();
                    // SAFETY: `pv` has room for `cb` bytes, and `n <= cb`.
                    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pv as *mut u8, n) };
                    let new = pos + n as u64;
                    self.pos.store(new, Ordering::SeqCst);
                    if new >= self.size {
                        self.tail.store(true, Ordering::SeqCst);
                    }
                    set_read(n as u32);
                    return if n as u32 == cb { S_OK } else { S_FALSE };
                }
                Got::Wait => pump_messages(),
                Got::Fail => {
                    set_read(0);
                    tracing::warn!(pos, size = self.size, "virtual file: no data, or it failed verification; failing the paste");
                    return STG_E_READFAULT;
                }
            }
        }
    }

    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for VStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows::core::Result<()> {
        let base = match dworigin {
            STREAM_SEEK_SET => 0i128,
            STREAM_SEEK_CUR => self.pos.load(Ordering::SeqCst) as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let new = base + dlibmove as i128;
        if new < 0 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        self.pos.store(new as u64, Ordering::SeqCst);
        if !plibnewposition.is_null() {
            // SAFETY: the caller's out parameter.
            unsafe { *plibnewposition = new as u64 };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _pstm: windows::core::Ref<IStream>, _cb: u64, _pcbread: *mut u64, _pcbwritten: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        // SAFETY: the caller's out parameter.
        if let Some(st) = unsafe { pstatstg.as_mut() } {
            *st = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.size, ..Default::default() };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mouse_core::protocol::FileRef;

    #[test]
    fn folders_come_first_as_backslash_paths_and_pipes_attach_to_their_files() {
        let f = |name: &str, rel: &str, size| FileRef {
            name: name.into(),
            size,
            hash: cache::hash_bytes(name.as_bytes()),
            content_hashed: false,
            source_path: None,
            rel: rel.into(),
        };
        let set = FileSet {
            files: vec![f("1.flac", "Album/Disc 2/1.flac", 5), f("video.mkv", "", 9)],
            dirs: vec!["Album/Disc 2".into(), "Album".into()],
            origin: "kali".into(),
        };
        let mut pipes = HashMap::new();
        let big = cache::hash_bytes(b"video.mkv");
        pipes.insert(big.clone(), Arc::new(StreamPipe::new(9, None)));
        let items = items_for(&set, &Arc::new(PasteSignal::default()), &pipes);
        let names: Vec<_> = items.iter().map(|i| (i.rel.as_str(), i.dir)).collect();
        assert_eq!(
            names,
            vec![("Album", true), ("Album\\Disc 2", true), ("Album\\Disc 2\\1.flac", false), ("video.mkv", false)]
        );
        assert!(items[3].src.as_ref().unwrap().pipe.is_some());
        assert!(items[2].src.as_ref().unwrap().pipe.is_none());
    }
}
