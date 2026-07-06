use std::sync::{Arc, Mutex};

use dpi::LogicalPosition;
use gtk4::gdk::InputSource;
use gtk4::gdk::prelude::DeviceExt;
use gtk4::prelude::*;
use winit_core::event::{
    DeviceId, Force, PointerKind, PointerSource, TabletToolData, TabletToolKind, TabletToolTilt,
    WindowEvent,
};
use winit_core::window::WindowId;

use super::WindowState;
use crate::event_loop::ActiveEventLoop;

pub(crate) fn connect(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window_id: WindowId,
    state: &Arc<Mutex<WindowState>>,
) {
    let controller = gtk4::EventControllerMotion::new();

    {
        let shared = event_loop.shared.clone();
        let state = state.clone();
        controller.connect_enter(move |controller, x, y| {
            let pointer = PointerMetadata::from_controller(controller);
            let event = WindowEvent::PointerEntered {
                device_id: pointer.device_id,
                position: {
                    let scale_factor = state.lock().unwrap().scale_factor;
                    LogicalPosition::new(x, y).to_physical(scale_factor)
                },
                primary: pointer.primary,
                kind: pointer.kind,
            };
            shared.borrow_mut().events_sink.push_window_event(event, window_id);
        });
    }

    {
        let shared = event_loop.shared.clone();
        let state = state.clone();
        controller.connect_motion(move |controller, x, y| {
            let pointer = PointerMetadata::from_controller(controller);
            let event = WindowEvent::PointerMoved {
                device_id: pointer.device_id,
                position: {
                    let scale_factor = state.lock().unwrap().scale_factor;
                    LogicalPosition::new(x, y).to_physical(scale_factor)
                },
                primary: pointer.primary,
                source: pointer.source,
            };
            shared.borrow_mut().events_sink.push_window_event(event, window_id);
        });
    }

    {
        let shared = event_loop.shared.clone();
        let state = state.clone();
        controller.connect_leave(move |controller| {
            let pointer = PointerMetadata::from_controller(controller);
            let position = controller.current_event().and_then(|event| {
                let (x, y) = event.position()?;
                let scale_factor = state.lock().unwrap().scale_factor;
                Some(LogicalPosition::new(x, y).to_physical(scale_factor))
            });

            let event = WindowEvent::PointerLeft {
                device_id: pointer.device_id,
                position,
                primary: pointer.primary,
                kind: pointer.kind,
            };
            shared.borrow_mut().events_sink.push_window_event(event, window_id);
        });
    }

    gtk_window.add_controller(controller);
}

#[derive(Clone, Debug)]
struct PointerMetadata {
    device_id: Option<DeviceId>,
    primary: bool,
    kind: PointerKind,
    source: PointerSource,
}

impl PointerMetadata {
    fn from_controller(controller: &gtk4::EventControllerMotion) -> Self {
        let Some(device) = controller.current_event_device() else {
            return PointerMetadata::unknown(None);
        };

        let device_id = device_id(&device);
        let event = controller.current_event();

        match device.source() {
            InputSource::Mouse | InputSource::Touchpad | InputSource::Trackpoint => {
                Self::mouse(device_id)
            },

            InputSource::Pen | InputSource::TabletPad => {
                Self::tablet_tool(&device, device_id, event.as_ref())
            },

            // Touch needs its own controller path to emit the full entered/button/moved/left lifecycle
            // with stable finger IDs.
            //
            // Non exhaustive, or unknown input sources, default to `PointerSource::Unknown`
            _ => PointerMetadata::unknown(device_id),
        }
    }

    fn mouse(device_id: Option<DeviceId>) -> Self {
        PointerMetadata {
            device_id,
            primary: true,
            kind: PointerKind::Mouse,
            source: PointerSource::Mouse,
        }
    }

    fn tablet_tool(
        device: &gtk4::gdk::Device,
        device_id: Option<DeviceId>,
        event: Option<&gtk4::gdk::Event>,
    ) -> Self {
        let kind = device
            .device_tool()
            .map(|tool| tablet_tool_kind(tool.tool_type()))
            .unwrap_or(TabletToolKind::Pen);

        let data = tablet_tool_data(event)
            .map(|data| PointerSource::TabletTool { kind, data })
            .unwrap_or(PointerSource::Unknown);

        PointerMetadata {
            device_id,
            primary: false,
            kind: PointerKind::TabletTool(kind),
            source: data,
        }
    }

    fn unknown(device_id: Option<DeviceId>) -> Self {
        PointerMetadata {
            device_id,
            primary: true,
            kind: PointerKind::Unknown,
            source: PointerSource::Unknown,
        }
    }
}

pub(crate) fn device_id(device: &gtk4::gdk::Device) -> Option<DeviceId> {
    // GDK only exposes a backend-native numeric pointer device id for X11/XInput2.
    // Wayland devices fail this downcast and keep winit's `device_id: None` behavior
    // matching the current winit-wayland implementation.
    device
        .clone()
        .downcast::<gdk4_x11::X11DeviceXI2>()
        .ok()
        .map(|device| DeviceId::from_raw(device.device_id() as i64))
}

fn tablet_tool_kind(tool_type: gtk4::gdk::DeviceToolType) -> TabletToolKind {
    match tool_type {
        gtk4::gdk::DeviceToolType::Pen => TabletToolKind::Pen,
        gtk4::gdk::DeviceToolType::Eraser => TabletToolKind::Eraser,
        gtk4::gdk::DeviceToolType::Brush => TabletToolKind::Brush,
        gtk4::gdk::DeviceToolType::Pencil => TabletToolKind::Pencil,
        gtk4::gdk::DeviceToolType::Airbrush => TabletToolKind::Airbrush,
        gtk4::gdk::DeviceToolType::Mouse => TabletToolKind::Mouse,
        gtk4::gdk::DeviceToolType::Lens => TabletToolKind::Lens,

        // Non exhaustive, or unknown device tool types, default to `Pen`
        _ => TabletToolKind::Pen,
    }
}

fn tablet_tool_data(event: Option<&gtk4::gdk::Event>) -> Option<TabletToolData> {
    let event = event?;
    let data = TabletToolData {
        force: event.axis(gtk4::gdk::AxisUse::Pressure).map(Force::Normalized),
        tangential_force: None,
        twist: event.axis(gtk4::gdk::AxisUse::Rotation).map(rotation_to_twist),
        tilt: tablet_tool_tilt(event),
        angle: None,
    };

    (data != TabletToolData::default()).then_some(data)
}

fn tablet_tool_tilt(event: &gtk4::gdk::Event) -> Option<TabletToolTilt> {
    let x = event.axis(gtk4::gdk::AxisUse::Xtilt)?;
    let y = event.axis(gtk4::gdk::AxisUse::Ytilt)?;

    Some(TabletToolTilt { x: tilt_axis_to_degrees(x), y: tilt_axis_to_degrees(y) })
}

fn tilt_axis_to_degrees(value: f64) -> i8 {
    value.round().clamp(-90.0, 90.0) as i8
}

fn rotation_to_twist(value: f64) -> u16 {
    value.round().rem_euclid(360.0) as u16
}
