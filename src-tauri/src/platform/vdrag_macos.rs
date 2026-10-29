//! macOS streamed drag: one `NSFilePromiseProvider` per top-level file or folder of the batch. Once dropped,
//! its delegate writes the promise on its own operation queue from the bytes as they arrive (`Growing`);
//! a folder is created and filled file by file. A failed transfer or vanished sender fails the promise.

use super::Growing;
use crate::network::transfer::Promised;
use block2::DynBlock;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApp, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSImage, NSView,
};
use objc2_foundation::{NSArray, NSData, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL};
use std::{fs::File, io, path::Path};

/// One top-level entry of the drag: its files as (path below it, "" for the entry itself, file).
struct Promise {
    name: String,
    batch: u64,
    files: Vec<(String, Promised)>,
    /// promises are written here, never on the main queue (the default): writing waits for the network
    queue: Retained<NSOperationQueue>,
}

impl Promise {
    fn write(&self, to: &Path) -> io::Result<()> {
        for (below, file) in &self.files {
            let dest = if below.is_empty() { to.to_path_buf() } else { below.split('/').fold(to.to_path_buf(), |d, p| d.join(p)) };
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)?;
            }
            io::copy(&mut Growing::new(self.batch, file), &mut File::create(&dest)?)?;
        }
        Ok(())
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LokaFilePromise"]
    #[ivars = Promise]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _provider: &NSFilePromiseProvider, _type: &NSString) -> Retained<NSString> {
            NSString::from_str(&self.ivars().name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(&self, _provider: &NSFilePromiseProvider, url: &NSURL, done: &DynBlock<dyn Fn(*mut NSError)>) {
            let r = url.to_file_path().ok_or_else(|| io::Error::other("not a file URL")).and_then(|to| self.ivars().write(&to));
            match r {
                Ok(()) => done.call((std::ptr::null_mut(),)),
                Err(e) => {
                    eprintln!("streamed drag: {e}");
                    let error = NSError::new(1, &NSString::from_str("LOKA"));
                    done.call((Retained::as_ptr(&error) as *mut NSError,));
                }
            }
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(promise: Promise) -> Retained<Self> {
        let this = Self::alloc().set_ivars(promise);
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LokaDragSource"]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }
    }
);

impl DragSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

pub fn start(window: &tauri::WebviewWindow, batch: u64, files: Vec<Promised>) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("drags start on the main thread")?;
    let mut roots: Vec<(String, bool, Vec<(String, Promised)>)> = Vec::new();
    for f in files {
        let (top, below) = f.name.split_once('/').map_or((f.name.clone(), String::new()), |(t, b)| (t.to_string(), b.to_string()));
        match roots.iter_mut().find(|r| r.0 == top) {
            Some(r) => r.2.push((below, f)),
            None => roots.push((top, !below.is_empty(), vec![(below, f)])),
        }
    }
    #[allow(unused_unsafe)]
    unsafe {
        let view = &*(window.ns_view().map_err(|e| e.to_string())? as *const NSView);
        let ns_window = view.window().ok_or("no window")?;
        let content = ns_window.contentView().ok_or("no content view")?;
        let at = ns_window.mouseLocationOutsideOfEventStream();
        let icon = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(include_bytes!("../../icons/64x64.png"))).ok_or("no drag image")?;
        let icon: &AnyObject = &icon;
        let frame = NSRect::new(NSPoint::new(at.x - 32.0, at.y - 32.0), NSSize::new(64.0, 64.0));
        let items: Vec<Retained<NSDraggingItem>> = roots
            .into_iter()
            .map(|(name, folder, files)| {
                let file_type = NSString::from_str(if folder { "public.folder" } else { "public.data" });
                let delegate = PromiseDelegate::new(Promise { name, batch, files, queue: NSOperationQueue::new() });
                let provider = NSFilePromiseProvider::initWithFileType_delegate(NSFilePromiseProvider::alloc(), &file_type, ProtocolObject::from_ref(&*delegate));
                let keep: &AnyObject = &delegate;
                provider.setUserInfo(Some(keep)); // the provider holds its delegate weakly
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*provider));
                item.setDraggingFrame_contents(frame, Some(icon));
                item
            })
            .collect();
        let time = NSApp(mtm).currentEvent().map_or(0.0, |e| e.timestamp());
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseDragged,
            at,
            NSEventModifierFlags::empty(),
            time,
            ns_window.windowNumber(),
            None,
            0,
            1,
            1.0,
        )
        .ok_or("no drag event")?;
        content.beginDraggingSessionWithItems_event_source(&NSArray::from_retained_slice(&items), &event, ProtocolObject::from_ref(&*DragSource::new(mtm)));
    }
    Ok(())
}
