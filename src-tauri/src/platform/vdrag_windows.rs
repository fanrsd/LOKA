//! Windows streamed drag: an OLE data object of virtual files (`CFSTR_FILEDESCRIPTORW`, folders first, plus
//! one `CFSTR_FILECONTENTS` IStream per file) whose reads wait for bytes still being received. It is
//! async-capable, so Explorer copies on its own thread after the drop instead of inside `DoDragDrop`.

use super::Growing;
use crate::network::transfer::Promised;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    ffi::c_void,
    mem::ManuallyDrop,
};
use windows::{
    core::{implement, Ref, BOOL, HRESULT},
    Win32::{
        Foundation::*,
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::{Com::*, Memory::*, Ole::*},
        UI::Shell::*,
    },
};

type WinResult<T> = windows::core::Result<T>;

pub fn start(window: &tauri::WebviewWindow, batch: u64, files: Vec<Promised>) -> Result<(), String> {
    let hwnd = HWND(window.hwnd().map_err(|e| e.to_string())?.0 as _);
    unsafe {
        let _ = OleInitialize(None);
        let data: IDataObject = Files::new(batch, files).map_err(|e| e.message())?.into();
        // the shell's own drop source (button up drops, Esc cancels) and generic drag image
        SHDoDragDrop(Some(hwnd), &data, None::<&IDropSource>, DROPEFFECT_COPY).map(|_| ()).map_err(|e| e.message())
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct Files {
    batch: u64,
    /// descriptor order: '\'-separated relative name, `None` for a folder (listed before its contents)
    items: Vec<(String, Option<Promised>)>,
    /// keeps whatever else targets and the drag image helper set (`SetData`)
    inner: IDataObject,
    descriptor: u16,
    contents: u16,
    busy: Cell<bool>,
}

impl Files {
    fn new(batch: u64, files: Vec<Promised>) -> WinResult<Self> {
        let (mut items, mut dirs) = (Vec::new(), HashSet::new());
        for f in files {
            let parts: Vec<&str> = f.name.split('/').collect();
            for i in 1..parts.len() {
                let dir = parts[..i].join("\\");
                if dirs.insert(dir.clone()) {
                    items.push((dir, None));
                }
            }
            items.push((f.name.replace('/', "\\"), Some(f)));
        }
        let cf = |name: windows::core::PCWSTR| unsafe { windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(name.0) as u16 };
        Ok(Self {
            batch,
            items,
            inner: unsafe { SHCreateDataObject(None, None, None)? },
            descriptor: cf(CFSTR_FILEDESCRIPTORW),
            contents: cf(CFSTR_FILECONTENTS),
            busy: Cell::new(false),
        })
    }

    fn is(f: &FORMATETC, cf: u16, tymed: TYMED) -> bool {
        f.cfFormat == cf && f.tymed & tymed.0 as u32 != 0
    }

    /// FILEGROUPDESCRIPTORW: item count, then one descriptor per item (packed).
    fn descriptors(&self) -> WinResult<HGLOBAL> {
        let one = std::mem::size_of::<FILEDESCRIPTORW>();
        unsafe {
            let h = GlobalAlloc(GHND, 4 + self.items.len() * one)?;
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            (p as *mut u32).write_unaligned(self.items.len() as u32);
            for (i, (name, file)) in self.items.iter().enumerate() {
                let mut cfilename = [0u16; 260]; // transfer bounds names to 259 units: the NUL stays
                for (d, c) in cfilename.iter_mut().zip(name.encode_utf16()) {
                    *d = c;
                }
                let size = file.as_ref().map_or(0, |f| f.size);
                let d = FILEDESCRIPTORW {
                    dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
                    dwFileAttributes: if file.is_some() { FILE_ATTRIBUTE_NORMAL.0 } else { FILE_ATTRIBUTE_DIRECTORY.0 },
                    nFileSizeHigh: (size >> 32) as u32,
                    nFileSizeLow: size as u32,
                    cFileName: cfilename,
                    ..Default::default()
                };
                (p.add(4 + i * one) as *mut FILEDESCRIPTORW).write_unaligned(d);
            }
            let _ = GlobalUnlock(h);
            Ok(h)
        }
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for Files_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let f = unsafe { pformatetcin.as_ref() }.ok_or(E_INVALIDARG)?;
        if Files::is(f, self.descriptor, TYMED_HGLOBAL) {
            return Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: self.descriptors()? }, pUnkForRelease: ManuallyDrop::new(None) });
        }
        if Files::is(f, self.contents, TYMED_ISTREAM) {
            let file = usize::try_from(f.lindex).ok().and_then(|i| self.items.get(i)?.1.as_ref()).ok_or(DV_E_LINDEX)?;
            let stream: IStream = Stream(RefCell::new(Growing::new(self.batch, file))).into();
            return Ok(STGMEDIUM { tymed: TYMED_ISTREAM.0 as u32, u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(stream)) }, pUnkForRelease: ManuallyDrop::new(None) });
        }
        unsafe { self.inner.GetData(pformatetcin) }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> {
        Err(DV_E_FORMATETC.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        let Some(f) = (unsafe { pformatetc.as_ref() }) else { return E_INVALIDARG };
        if Files::is(f, self.descriptor, TYMED_HGLOBAL) || Files::is(f, self.contents, TYMED_ISTREAM) {
            S_OK
        } else {
            unsafe { self.inner.QueryGetData(pformatetc) }
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(&self, pformatetc: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> WinResult<()> {
        unsafe { self.inner.SetData(pformatetc, pmedium, frelease.as_bool()) }
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let fmt = |cf, tymed: TYMED| FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: tymed.0 as u32 };
        unsafe { SHCreateStdEnumFmtEtc(&[fmt(self.descriptor, TYMED_HGLOBAL), fmt(self.contents, TYMED_ISTREAM)]) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[allow(non_snake_case)]
impl IDataObjectAsyncCapability_Impl for Files_Impl {
    fn SetAsyncMode(&self, _: BOOL) -> WinResult<()> {
        Ok(())
    }
    fn GetAsyncMode(&self) -> WinResult<BOOL> {
        Ok(true.into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> WinResult<()> {
        self.busy.set(true);
        Ok(())
    }
    fn InOperation(&self) -> WinResult<BOOL> {
        Ok(self.busy.get().into())
    }
    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> WinResult<()> {
        self.busy.set(false);
        Ok(())
    }
}

/// Read-only stream of one file still arriving.
#[implement(IStream)]
struct Stream(RefCell<Growing>);

/// Wait ~15 ms while still serving window messages and COM calls: Explorer's reads arrive on our UI
/// (STA) thread, which must not freeze for the length of a transfer.
fn pump() {
    // our own process handle is never signaled while we run: a pure timed wait
    let me = HANDLE(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() });
    let r = unsafe { CoWaitForMultipleHandles((COWAIT_DISPATCH_WINDOW_MESSAGES.0 | COWAIT_DISPATCH_CALLS.0) as u32, 15, &[me]) };
    if r.is_err_and(|e| e.code() != RPC_S_CALLPENDING) {
        std::thread::sleep(std::time::Duration::from_millis(15)); // not a COM thread
    }
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        // pumping inside a read can re-enter it
        let Ok(mut g) = self.0.try_borrow_mut() else { return E_UNEXPECTED };
        if pv.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let mut done = 0;
        // fill the whole buffer: some readers take a short read for the end of the file
        while done < buf.len() {
            match g.try_read(&mut buf[done..]) {
                Some(Ok(0)) => break,
                Some(Ok(n)) => done += n,
                Some(Err(e)) => {
                    eprintln!("streamed drag: {e}");
                    return STG_E_READFAULT;
                }
                None => pump(),
            }
        }
        if let Some(read) = unsafe { pcbread.as_mut() } {
            *read = done as u32;
        }
        if done < buf.len() {
            S_FALSE
        } else {
            S_OK
        }
    }

    fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for Stream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> WinResult<()> {
        let mut g = self.0.try_borrow_mut().map_err(|_| E_UNEXPECTED)?;
        let from = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => g.pos,
            STREAM_SEEK_END => g.size,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        g.pos = from.checked_add_signed(dlibmove).ok_or(STG_E_INVALIDFUNCTION)?;
        if let Some(out) = unsafe { plibnewposition.as_mut() } {
            *out = g.pos;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> WinResult<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> WinResult<()> {
        Ok(())
    }

    fn Revert(&self) -> WinResult<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _: &STATFLAG) -> WinResult<()> {
        let out = unsafe { pstatstg.as_mut() }.ok_or(STG_E_INVALIDPOINTER)?;
        // no name even if asked: callers then use the file descriptor's
        *out = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.0.try_borrow().map_err(|_| E_UNEXPECTED)?.size, ..Default::default() };
        Ok(())
    }

    fn Clone(&self) -> WinResult<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transfer;
    use std::io::Write;

    fn read(s: &IStream, buf: &mut [u8]) -> (HRESULT, u32) {
        let mut n = 0u32;
        let hr = unsafe { s.Read(buf.as_mut_ptr() as _, buf.len() as u32, Some(&mut n as *mut u32)) };
        (hr, n)
    }

    #[test]
    fn stream_read_waits_for_arriving_bytes_and_ends_at_the_size() {
        let path = std::env::temp_dir().join(format!("loka-vdrag-{}", uuid::Uuid::new_v4()));
        let batch = uuid::Uuid::new_v4().as_u128() as u64;
        transfer::announce(batch);
        let file = Promised { name: "a.bin".into(), path: path.clone(), size: 6 };
        let stream: IStream = Stream(RefCell::new(Growing::new(batch, &file))).into();

        let writer = std::thread::spawn(move || {
            let mut f = std::fs::File::create(&file.path).unwrap();
            for part in [&b"ab"[..], b"cd", b"ef"] {
                std::thread::sleep(std::time::Duration::from_millis(150));
                f.write_all(part).unwrap();
            }
        });
        let started = std::time::Instant::now();
        let mut buf = [0u8; 4];
        assert_eq!(read(&stream, &mut buf), (S_OK, 4), "blocks until the whole buffer arrived");
        assert_eq!(&buf, b"abcd");
        assert!(started.elapsed() >= std::time::Duration::from_millis(250));
        assert_eq!(read(&stream, &mut buf), (S_FALSE, 2), "ends at the announced size");
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(read(&stream, &mut buf), (S_FALSE, 0));
        writer.join().unwrap();

        // a failed transfer turns a waiting read into an error
        let gone = Promised { name: "b.bin".into(), path: path.with_extension("missing"), size: 1 };
        let stream: IStream = Stream(RefCell::new(Growing::new(batch, &gone))).into();
        let failer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            transfer::fail(batch);
        });
        assert_eq!(read(&stream, &mut buf[..1]).0, STG_E_READFAULT);
        failer.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
