//! Menu bar pen icon shown while the screen is shared, so the sharer can turn
//! drawing on and off without opening Hopp's window first.
//!
//! It lives in the core process rather than in the Hopp app: the click then
//! reaches the process that owns the drawing window, which may make itself the
//! active app while handling it. macOS shows the pen cursor only for the
//! active app.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSImage, NSStatusBar, NSStatusItem};
use objc2_foundation::{ns_string, NSObject, NSObjectProtocol, NSString};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

/// `NSSquareStatusItemLength`: a square item sized to the menu bar height.
const SQUARE_STATUS_ITEM_LENGTH: f64 = -2.0;

struct TargetIvars {
    event_loop_proxy: EventLoopProxy<UserEvent>,
    drawing: Cell<bool>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct DrawingStatusItemTarget;

    unsafe impl NSObjectProtocol for DrawingStatusItemTarget {}

    impl DrawingStatusItemTarget {
        #[unsafe(method(toggleDrawing:))]
        fn toggle_drawing(&self, _sender: Option<&AnyObject>) {
            log::info!("DrawingStatusItem: clicked (drawing: {})", self.ivars().drawing.get());
            if !self.ivars().drawing.get() {
                // Activate now, while handling the user's click: macOS allows it
                // here, and the drawing window then gets the mouse and the pen.
                activate_app(self.mtm());
            }
            let _ = self
                .ivars()
                .event_loop_proxy
                .send_event(UserEvent::DrawingStatusItemClicked);
        }
    }
);

/// Activates this app. `activate` (macOS 14+) where available, else the
/// older call that forces it.
pub(crate) fn activate_app(main_thread: MainThreadMarker) {
    let app = NSApplication::sharedApplication(main_thread);
    if app.respondsToSelector(sel!(activate)) {
        let _: () = unsafe { msg_send![&app, activate] };
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

pub struct DrawingStatusItem {
    item: Retained<NSStatusItem>,
    target: Retained<DrawingStatusItemTarget>,
}

impl DrawingStatusItem {
    pub fn new(event_loop_proxy: EventLoopProxy<UserEvent>) -> Option<Self> {
        let Some(main_thread) = MainThreadMarker::new() else {
            log::error!("DrawingStatusItem: not on the main thread");
            return None;
        };
        let target = DrawingStatusItemTarget::alloc(main_thread).set_ivars(TargetIvars {
            event_loop_proxy,
            drawing: Cell::new(false),
        });
        let target: Retained<DrawingStatusItemTarget> = unsafe { msg_send![super(target), init] };

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(SQUARE_STATUS_ITEM_LENGTH);
        let Some(button) = item.button(main_thread) else {
            log::error!("DrawingStatusItem: status item has no button");
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
            return None;
        };
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(toggleDrawing:)));
        }
        button.setToolTip(Some(ns_string!("Draw on your screen")));

        let status_item = Self { item, target };
        status_item.set_drawing(false);
        log::info!("DrawingStatusItem: added");
        Some(status_item)
    }

    /// Shows whether drawing is on: an outlined pen when off, a filled one when on.
    pub fn set_drawing(&self, drawing: bool) {
        self.target.ivars().drawing.set(drawing);
        let Some(button) = self.item.button(self.target.mtm()) else {
            return;
        };
        let (symbol, description) = if drawing {
            ("pencil.tip.crop.circle.fill", "Stop drawing")
        } else {
            ("pencil.tip.crop.circle", "Draw on your screen")
        };
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(description)),
        );
        match image {
            Some(image) => {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            None => button.setTitle(&NSString::from_str(if drawing { "✎•" } else { "✎" })),
        }
        button.setToolTip(Some(&NSString::from_str(description)));
    }
}

impl Drop for DrawingStatusItem {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        log::info!("DrawingStatusItem: removed");
    }
}
