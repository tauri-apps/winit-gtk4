use dpi::LogicalPosition;
use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::prelude::*;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use winit_core::error::{NotSupportedError, OsError, RequestError};
use winit_core::window::CursorGrabMode;

use crate::event_loop::OwnedDisplayHandle;

pub(crate) fn raw_display_handle(
    display: gtk4::gdk::Display,
) -> Result<Option<OwnedDisplayHandle>, rwh_06::HandleError> {
    let Ok(display) = display.downcast::<gdk4_wayland::WaylandDisplay>() else {
        return Ok(None);
    };

    let display = display.wl_display_raw().ok_or(rwh_06::HandleError::Unavailable)?;
    Ok(Some(OwnedDisplayHandle::Wayland { display }))
}

pub(crate) fn raw_window_handle(surface: &gtk4::gdk::Surface) -> Option<rwh_06::RawWindowHandle> {
    let Ok(surface) = surface.clone().downcast::<gdk4_wayland::WaylandSurface>() else {
        return None;
    };

    let surface = surface.wl_surface_raw()?;
    Some(rwh_06::WaylandWindowHandle::new(surface).into())
}

pub(crate) struct GtkWaylandWindow {
    cursor_grab: Option<GtkWaylandCursorGrab>,
}

impl GtkWaylandWindow {
    pub(crate) fn from_surface(surface: &gtk4::gdk::Surface) -> Option<Self> {
        let Ok(_) = surface.display().downcast::<gdk4_wayland::WaylandDisplay>() else {
            return None;
        };

        Some(Self { cursor_grab: None })
    }

    pub(crate) fn set_cursor_grab(
        &mut self,
        surface: &gtk4::gdk::Surface,
        pointer_device: Option<&gtk4::gdk::Device>,
        mode: CursorGrabMode,
    ) -> Result<(), RequestError> {
        if self.cursor_grab.is_none() {
            if mode == CursorGrabMode::None {
                return Ok(());
            }

            self.cursor_grab = GtkWaylandCursorGrab::from_surface(surface)?;
        }

        let Some(cursor_grab) = self.cursor_grab.as_mut() else {
            let e = NotSupportedError::new("cursor grabbing is not supported on this GDK backend");
            return Err(e.into());
        };

        if let Some(pointer_device) = pointer_device {
            cursor_grab.set_pointer_device(pointer_device);
        }

        cursor_grab.set_cursor_grab(surface, mode)
    }

    pub(crate) fn set_cursor_position(
        &mut self,
        position: LogicalPosition<f64>,
    ) -> Result<(), RequestError> {
        let Some(cursor_grab) = self.cursor_grab.as_mut() else {
            let e = NotSupportedError::new("cursor positioning requires a locked pointer");
            return Err(e.into());
        };

        cursor_grab.set_cursor_position(position)
    }
}

pub(crate) struct GtkWaylandCursorGrab {
    conn: Connection,
    queue: EventQueue<WaylandCursorGrabState>,
    #[allow(unused)]
    state: WaylandCursorGrabState,
    pointer_constraints: ZwpPointerConstraintsV1,
    current_pointer: Option<WlPointer>,
    current_mode: CursorGrabMode,
    locked_pointer: Option<ZwpLockedPointerV1>,
    confined_pointer: Option<ZwpConfinedPointerV1>,
}

impl GtkWaylandCursorGrab {
    pub(crate) fn from_surface(surface: &gtk4::gdk::Surface) -> Result<Option<Self>, RequestError> {
        let Ok(display) = surface.display().downcast::<gdk4_wayland::WaylandDisplay>() else {
            return Ok(None);
        };

        if !display.query_registry(ZwpPointerConstraintsV1::interface().name) {
            let e = NotSupportedError::new("zwp_pointer_constraints_v1 is not available");
            return Err(e.into());
        }

        let Some(wl_display) = display.wl_display() else {
            return Err(NotSupportedError::new("Wayland display is not available").into());
        };
        let Some(backend) = wl_display.backend().upgrade() else {
            return Err(NotSupportedError::new("Wayland connection is not available").into());
        };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<WaylandCursorGrabState>(&conn)
            .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;

        let pointer_constraints = globals
            .bind(&queue.handle(), 1..=1, ())
            .map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))?;

        Ok(Some(Self {
            conn,
            queue,
            state: WaylandCursorGrabState,
            pointer_constraints,
            current_pointer: None,
            current_mode: CursorGrabMode::None,
            locked_pointer: None,
            confined_pointer: None,
        }))
    }

    pub(crate) fn set_pointer_device(&mut self, device: &gtk4::gdk::Device) {
        let pointer = device
            .clone()
            .downcast::<gdk4_wayland::WaylandDevice>()
            .ok()
            .and_then(|device| device.wl_pointer());

        let changed = match (&self.current_pointer, &pointer) {
            (Some(current_pointer), Some(pointer)) => current_pointer.id() != pointer.id(),
            (None, Some(_)) => true,
            (Some(_), None) => true,
            _ => false,
        };

        if changed {
            self.current_pointer = pointer;
            self.current_mode = CursorGrabMode::None;
        }
    }

    pub(crate) fn set_cursor_grab(
        &mut self,
        surface: &gtk4::gdk::Surface,
        mode: CursorGrabMode,
    ) -> Result<(), RequestError> {
        if mode == self.current_mode {
            return Ok(());
        }

        self.unset_cursor_grab();

        if mode == CursorGrabMode::None {
            return self.flush();
        }

        let Some(pointer) = self.current_pointer.as_ref() else {
            return Ok(());
        };

        let Some(surface) = surface.clone().downcast::<gdk4_wayland::WaylandSurface>().ok() else {
            return Err(NotSupportedError::new("Wayland surface is not available").into());
        };
        let Some(surface) = surface.wl_surface() else {
            return Err(NotSupportedError::new("Wayland surface is not available").into());
        };

        match mode {
            CursorGrabMode::Confined => {
                self.confined_pointer = Some(self.pointer_constraints.confine_pointer(
                    &surface,
                    pointer,
                    None,
                    Lifetime::Persistent,
                    &self.queue.handle(),
                    (),
                ));
            },
            CursorGrabMode::Locked => {
                self.locked_pointer = Some(self.pointer_constraints.lock_pointer(
                    &surface,
                    pointer,
                    None,
                    Lifetime::Persistent,
                    &self.queue.handle(),
                    (),
                ));
            },
            CursorGrabMode::None => (),
        }

        self.current_mode = mode;
        self.flush()
    }

    pub(crate) fn set_cursor_position(
        &self,
        position: LogicalPosition<f64>,
    ) -> Result<(), RequestError> {
        if self.current_mode != CursorGrabMode::Locked {
            let e = NotSupportedError::new("cursor positioning requires a locked pointer");
            return Err(e.into());
        }

        let Some(locked_pointer) = self.locked_pointer.as_ref() else {
            let e = NotSupportedError::new("cursor positioning requires a locked pointer");
            return Err(e.into());
        };

        locked_pointer.set_cursor_position_hint(position.x, position.y);
        self.flush()
    }

    fn unset_cursor_grab(&mut self) {
        if let Some(locked_pointer) = self.locked_pointer.take() {
            locked_pointer.destroy();
        }
        if let Some(confined_pointer) = self.confined_pointer.take() {
            confined_pointer.destroy();
        }
        self.current_mode = CursorGrabMode::None;
    }

    fn flush(&self) -> Result<(), RequestError> {
        self.conn.flush().map_err(|err| RequestError::Os(OsError::new(line!(), file!(), err)))
    }
}

impl Drop for GtkWaylandCursorGrab {
    fn drop(&mut self) {
        self.unset_cursor_grab();
        self.pointer_constraints.destroy();
        let _ = self.conn.flush();
    }
}

pub(crate) struct WaylandCursorGrabState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandCursorGrabState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for WaylandCursorGrabState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPointerConstraintsV1,
        _event: <ZwpPointerConstraintsV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for WaylandCursorGrabState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLockedPointerV1,
        _event: <ZwpLockedPointerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for WaylandCursorGrabState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpConfinedPointerV1,
        _event: <ZwpConfinedPointerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
