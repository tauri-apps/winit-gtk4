use std::ptr::NonNull;

use dpi::PhysicalPosition;
use gdk4_x11::x11::xlib;
use gtk4::gdk::prelude::SurfaceExt;
use gtk4::prelude::*;

use crate::event_loop::OwnedDisplayHandle;

pub(crate) fn raw_display_handle(
    display: gtk4::gdk::Display,
) -> Result<Option<OwnedDisplayHandle>, rwh_06::HandleError> {
    let Ok(display) = display.downcast::<gdk4_x11::X11Display>() else {
        return Ok(None);
    };

    let xdisplay = unsafe { display.xdisplay() };
    let xdisplay = NonNull::new(xdisplay.cast()).ok_or(rwh_06::HandleError::Unavailable)?;
    let screen = display.screen().screen_number();

    Ok(Some(OwnedDisplayHandle::Xlib { display: xdisplay, screen }))
}

pub(crate) fn raw_window_handle(surface: &gtk4::gdk::Surface) -> Option<rwh_06::RawWindowHandle> {
    let window = XWindow::from_surface(surface)?;
    Some(rwh_06::XlibWindowHandle::new(window.xid as _).into())
}

pub struct XWindow {
    display: *mut xlib::Display,
    xid: xlib::Window,
}

impl XWindow {
    pub fn from_surface(surface: &gtk4::gdk::Surface) -> Option<Self> {
        let surface = surface.clone().downcast::<gdk4_x11::X11Surface>().ok()?;
        let display = surface.display().downcast::<gdk4_x11::X11Display>().ok()?;

        let xid = surface.xid();
        if xid == 0 {
            return None;
        }

        let display = unsafe { display.xdisplay() };
        if display.is_null() {
            return None;
        }

        Some(Self { display, xid })
    }

    pub fn set_position(&self, position: PhysicalPosition<i32>) {
        // GTK/GDK has no cross-backend API for global toplevel placement. On X11
        // the closest match for winit's initial position is WM_NORMAL_HINTS.
        let Ok(xlib) = xlib::Xlib::open() else {
            return;
        };

        unsafe {
            let mut hints = std::mem::zeroed::<xlib::XSizeHints>();
            let mut supplied = 0;
            if (xlib.XGetWMNormalHints)(self.display, self.xid, &mut hints, &mut supplied) == 0 {
                hints = std::mem::zeroed();
            }
            hints.flags |= xlib::USPosition;
            hints.x = position.x;
            hints.y = position.y;
            (xlib.XSetWMNormalHints)(self.display, self.xid, &mut hints);
            (xlib.XMoveWindow)(self.display, self.xid, position.x, position.y);
            (xlib.XFlush)(self.display);
        }
    }
}
