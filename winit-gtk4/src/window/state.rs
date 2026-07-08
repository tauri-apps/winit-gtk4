use dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit_core::keyboard::{ModifiersState, PhysicalKey};
use winit_core::window::{Theme, WindowLevel};

#[derive(Debug)]
pub(crate) struct WindowState {
    pub(crate) surface_size: LogicalSize<u32>,
    pub(crate) last_layout: Option<PhysicalSize<u32>>,
    pub(crate) last_position: Option<PhysicalPosition<i32>>,
    pub(crate) scale_factor: f64,
    pub(crate) visible: bool,
    pub(crate) resizable: bool,
    pub(crate) has_focus: bool,
    pub(crate) modifiers: ModifiersState,
    pub(crate) held_key_press: Option<PhysicalKey>,
    pub(crate) theme: Option<Theme>,
    pub(crate) title: String,
    pub(crate) window_level: WindowLevel,
}
