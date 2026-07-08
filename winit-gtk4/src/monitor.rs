use std::borrow::Cow;
use std::num::NonZeroU32;
use std::sync::Arc;

use dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use gtk4::gdk::prelude::{DisplayExt, MonitorExt};
use gtk4::prelude::*;
use winit_core::monitor::{MonitorHandle as CoreMonitorHandle, MonitorHandleProvider, VideoMode};

pub(crate) fn available_monitors() -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
    let monitors = gtk4::gdk::Display::default()
        .map(|display| monitors_for_display(&display))
        .unwrap_or_default();

    Box::new(monitors.into_iter())
}

pub(crate) fn current_monitor(surface: &gtk4::gdk::Surface) -> Option<CoreMonitorHandle> {
    let monitor = surface.display().monitor_at_surface(surface)?;
    MonitorHandle::new(&monitor).map(|monitor| CoreMonitorHandle(Arc::new(monitor)))
}

pub(crate) fn primary_monitor() -> Option<CoreMonitorHandle> {
    let display = gtk4::gdk::Display::default()?;
    // wayland doesn't have a concept of primary monitor, so this will fail on wayland and return
    // None.
    let x11_display = display.downcast::<gdk4_x11::X11Display>().ok()?;
    let monitor = x11_display.primary_monitor();
    MonitorHandle::new(&monitor).map(|monitor| CoreMonitorHandle(Arc::new(monitor)))
}

fn monitors_for_display(display: &gtk4::gdk::Display) -> Vec<CoreMonitorHandle> {
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| {
            let monitor = monitors.item(index)?.downcast::<gtk4::gdk::Monitor>().ok()?;
            if !monitor.is_valid() {
                return None;
            }

            MonitorHandle::new(&monitor).map(|monitor| CoreMonitorHandle(Arc::new(monitor)))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct MonitorHandle {
    native_id: u64,
    name: Option<String>,
    position: PhysicalPosition<i32>,
    scale_factor: f64,
    current_video_mode: Option<VideoMode>,
}

impl MonitorHandle {
    fn new(monitor: &gtk4::gdk::Monitor) -> Option<Self> {
        let geometry = monitor.geometry();
        let scale_factor = monitor.scale();

        let position = LogicalPosition::new(geometry.x(), geometry.y()).to_physical(scale_factor);
        let size = LogicalSize::new(geometry.width(), geometry.height()).to_physical(scale_factor);
        let refresh_rate = monitor.refresh_rate();
        let refresh_rate = NonZeroU32::new(refresh_rate as u32);
        let current_video_mode = Some(VideoMode::new(size, None, refresh_rate));

        let connector = monitor.connector().map(|name| name.to_string());
        let manufacturer = monitor.manufacturer().map(|name| name.to_string());
        let model = monitor.model().map(|name| name.to_string());
        let name = monitor_name(connector.as_deref(), manufacturer.as_deref(), model.as_deref());

        let native_id = native_monitor_id(monitor)?;

        Some(Self { native_id, name, position, scale_factor, current_video_mode })
    }
}

impl MonitorHandleProvider for MonitorHandle {
    fn id(&self) -> u128 {
        self.native_id as u128
    }

    fn native_id(&self) -> u64 {
        self.native_id
    }

    fn name(&self) -> Option<Cow<'_, str>> {
        self.name.as_deref().map(Cow::Borrowed)
    }

    fn position(&self) -> Option<PhysicalPosition<i32>> {
        Some(self.position)
    }

    fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    fn current_video_mode(&self) -> Option<VideoMode> {
        self.current_video_mode
    }

    fn video_modes(&self) -> Box<dyn Iterator<Item = VideoMode>> {
        Box::new(self.current_video_mode.into_iter())
    }
}

fn native_monitor_id(monitor: &gtk4::gdk::Monitor) -> Option<u64> {
    if let Ok(monitor) = monitor.clone().downcast::<gdk4_x11::X11Monitor>() {
        return Some(monitor.output() as u64);
    }

    if let Ok(monitor) = monitor.clone().downcast::<gdk4_wayland::WaylandMonitor>() {
        return monitor.wl_output_raw().map(|output| output.as_ptr() as u64);
    }

    None
}

fn monitor_name(
    connector: Option<&str>,
    manufacturer: Option<&str>,
    model: Option<&str>,
) -> Option<String> {
    match (connector, manufacturer, model) {
        (Some(connector), Some(manufacturer), Some(model)) => {
            Some(format!("{manufacturer} {model} ({connector})"))
        },
        (Some(connector), None, Some(model)) => Some(format!("{model} ({connector})")),
        (Some(connector), Some(manufacturer), None) => {
            Some(format!("{manufacturer} ({connector})"))
        },
        (Some(connector), None, None) => Some(connector.to_owned()),
        (None, Some(manufacturer), Some(model)) => Some(format!("{manufacturer} {model}")),
        (None, None, Some(model)) => Some(model.to_owned()),
        (None, Some(manufacturer), None) => Some(manufacturer.to_owned()),
        (None, None, None) => None,
    }
}
