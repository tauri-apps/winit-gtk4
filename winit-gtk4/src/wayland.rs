use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::prelude::*;

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
