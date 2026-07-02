use dpi::PhysicalSize;

#[derive(Debug)]
pub(crate) struct WindowState {
    pub(crate) surface_size: PhysicalSize<u32>,
    pub(crate) last_layout: Option<PhysicalSize<u32>>,
    pub(crate) scale_factor: f64,
    pub(crate) visible: bool,
    pub(crate) title: String,
}
