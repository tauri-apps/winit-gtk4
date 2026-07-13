use std::ptr::NonNull;
use std::sync::Arc;

use dpi::PhysicalPosition;
use gtk4::prelude::*;
use winit_core::error::{NotSupportedError, OsError, RequestError};
use winit_core::window::{CursorGrabMode, WindowLevel};
use winit_x11::x11_util;
use x11_util::{AtomName, StateOperation};
pub(crate) use x11_util::{FrameExtentsHeuristic, XConnection};
use x11rb::connection::Connection;
use x11rb::properties::{WmHints, WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::x11_utils::Serialize;

use crate::event_loop::OwnedDisplayHandle;

pub(crate) fn x_connection(display: gtk4::gdk::Display) -> Option<Arc<XConnection>> {
    let display = display.downcast::<gdk4_x11::X11Display>().ok()?;
    let root = display.xrootwindow() as xproto::Window;
    let xconn = Arc::new(XConnection::new(None).ok()?);
    xconn.update_cached_wm_info(root);
    Some(xconn)
}

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
    let surface = surface.clone().downcast::<gdk4_x11::X11Surface>().ok()?;
    let xid = surface.xid();
    if xid == 0 {
        return None;
    }

    Some(rwh_06::XlibWindowHandle::new(xid as _).into())
}

pub(crate) fn parent_window(raw: rwh_06::RawWindowHandle) -> Option<xproto::Window> {
    match raw {
        rwh_06::RawWindowHandle::Xlib(handle) => Some(handle.window as xproto::Window),
        rwh_06::RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct GtkXWindow {
    xconn: Arc<XConnection>,
    xid: xproto::Window,
    root: xproto::Window,
}

impl GtkXWindow {
    pub fn from_surface(surface: &gtk4::gdk::Surface, xconn: Arc<XConnection>) -> Option<Self> {
        let display = surface.display().downcast::<gdk4_x11::X11Display>().ok()?;
        let root = display.xrootwindow() as xproto::Window;

        let surface = surface.clone().downcast::<gdk4_x11::X11Surface>().ok()?;

        let xid = surface.xid();
        if xid == 0 {
            return None;
        }

        Some(Self { xconn, xid: xid as xproto::Window, root })
    }

    pub fn set_position(&self, position: PhysicalPosition<i32>) {
        let mut hints = WmSizeHints::get_normal_hints(self.xconn.xcb_connection(), self.xid)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .flatten()
            .unwrap_or_else(WmSizeHints::new);

        hints.position = Some((WmSizeHintsSpecification::UserSpecified, position.x, position.y));
        let _ = hints.set_normal_hints(self.xconn.xcb_connection(), self.xid);

        let configure = xproto::ConfigureWindowAux::new().x(position.x).y(position.y);
        let _ = self.xconn.xcb_connection().configure_window(self.xid, &configure);

        let _ = self.xconn.xcb_connection().flush();
    }

    pub fn set_cursor_position(&self, position: PhysicalPosition<i32>) -> Result<(), RequestError> {
        let x = position.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let y = position.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16;

        self.xconn
            .xcb_connection()
            .warp_pointer(x11rb::NONE, self.xid, 0, 0, 0, 0, x, y)
            .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;
        self.xconn
            .flush_requests()
            .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;

        Ok(())
    }

    pub fn set_cursor_grab(&self, mode: CursorGrabMode) -> Result<(), RequestError> {
        // We don't support the locked cursor yet, so ignore it early on.
        if mode == CursorGrabMode::Locked {
            return Err(NotSupportedError::new("locked cursor is not implemented on X11").into());
        }

        // We ungrab before grabbing to prevent passive grabs from causing `AlreadyGrabbed`.
        // Therefore, this is common to both codepaths.
        self.xconn
            .xcb_connection()
            .ungrab_pointer(x11rb::CURRENT_TIME)
            .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;

        let result = match mode {
            CursorGrabMode::None => self
                .xconn
                .flush_requests()
                .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err))),
            CursorGrabMode::Confined => {
                let result = self
                    .xconn
                    .xcb_connection()
                    .grab_pointer(
                        true as _,
                        self.xid,
                        xproto::EventMask::BUTTON_PRESS
                            | xproto::EventMask::BUTTON_RELEASE
                            | xproto::EventMask::ENTER_WINDOW
                            | xproto::EventMask::LEAVE_WINDOW
                            | xproto::EventMask::POINTER_MOTION
                            | xproto::EventMask::POINTER_MOTION_HINT
                            | xproto::EventMask::BUTTON1_MOTION
                            | xproto::EventMask::BUTTON2_MOTION
                            | xproto::EventMask::BUTTON3_MOTION
                            | xproto::EventMask::BUTTON4_MOTION
                            | xproto::EventMask::BUTTON5_MOTION
                            | xproto::EventMask::KEYMAP_STATE,
                        xproto::GrabMode::ASYNC,
                        xproto::GrabMode::ASYNC,
                        self.xid,
                        0u32,
                        x11rb::CURRENT_TIME,
                    )
                    .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?
                    .reply()
                    .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;

                match result.status {
                    xproto::GrabStatus::SUCCESS => Ok(()),
                    xproto::GrabStatus::ALREADY_GRABBED => {
                        Err("Cursor could not be confined: already confined by another client")
                    },
                    xproto::GrabStatus::INVALID_TIME => {
                        Err("Cursor could not be confined: invalid time")
                    },
                    xproto::GrabStatus::NOT_VIEWABLE => {
                        Err("Cursor could not be confined: confine location not viewable")
                    },
                    xproto::GrabStatus::FROZEN => {
                        Err("Cursor could not be confined: frozen by another client")
                    },
                    status => {
                        return Err(RequestError::Os(OsError::new(
                            line!(),
                            file!(),
                            format!("cursor could not be confined: unexpected status {status:?}"),
                        )));
                    },
                }
                .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))
            },
            CursorGrabMode::Locked => return Ok(()),
        };

        result
    }

    pub fn inner_position(&self) -> Option<PhysicalPosition<i32>> {
        let coords = self.xconn.translate_coords_root(self.xid, self.root).ok()?;
        Some(PhysicalPosition::new(coords.dst_x.into(), coords.dst_y.into()))
    }

    pub fn frame_extents(&self) -> FrameExtentsHeuristic {
        self.xconn.get_frame_extents_heuristic(self.xid, self.root)
    }

    pub fn set_parent(&self, parent: xproto::Window, position: PhysicalPosition<i32>) {
        let x = position.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let y = position.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let _ = self.xconn.xcb_connection().reparent_window(self.xid, parent, x, y);

        let _ = self.xconn.xcb_connection().flush();
    }

    pub fn set_window_level(&self, level: WindowLevel) {
        self.toggle_atom(AtomName::_NET_WM_STATE_ABOVE, level == WindowLevel::AlwaysOnTop);
        self.toggle_atom(AtomName::_NET_WM_STATE_BELOW, level == WindowLevel::AlwaysOnBottom);

        let _ = self.xconn.xcb_connection().flush();
    }

    pub fn request_user_attention(&self, request_attention: bool) {
        let mut hints = WmHints::get(self.xconn.xcb_connection(), self.xid)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .flatten()
            .unwrap_or_else(WmHints::new);

        hints.urgent = request_attention;
        let _ = hints.set(self.xconn.xcb_connection(), self.xid);

        let _ = self.xconn.xcb_connection().flush();
    }

    fn toggle_atom(&self, atom_name: AtomName, enabled: bool) {
        let atoms = self.xconn.atoms();
        let atom = atoms[atom_name];
        self.set_netwm(enabled.into(), (atom, 0, 0, 0));
    }

    fn set_netwm(&self, operation: StateOperation, properties: (u32, u32, u32, u32)) {
        let atoms = self.xconn.atoms();
        let state_atom = atoms[AtomName::_NET_WM_STATE];

        let event = xproto::ClientMessageEvent {
            response_type: xproto::CLIENT_MESSAGE_EVENT,
            window: self.xid,
            format: 32,
            data: [operation as u32, properties.0, properties.1, properties.2, properties.3].into(),
            sequence: 0,
            type_: state_atom,
        };

        let _ = self.xconn.xcb_connection().send_event(
            false,
            self.root,
            xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            event.serialize(),
        );
    }
}
