//! Winit's GTK4 backend.

mod event_loop;
mod sink;
mod wayland;
mod window;
mod x11;

use winit_core::event_loop::ActiveEventLoop as CoreActiveEventLoop;

pub use self::event_loop::{ActiveEventLoop, EventLoop, PlatformSpecificEventLoopAttributes};
pub use self::window::Window;

/// Additional methods on [`ActiveEventLoop`] that are specific to GTK4.
pub trait ActiveEventLoopExtGtk4 {
    /// True if the [`ActiveEventLoop`] uses GTK4.
    fn is_gtk4(&self) -> bool;
}

impl ActiveEventLoopExtGtk4 for dyn CoreActiveEventLoop + '_ {
    #[inline]
    fn is_gtk4(&self) -> bool {
        self.cast_ref::<ActiveEventLoop>().is_some()
    }
}

/// Additional methods on [`EventLoop`] that are specific to GTK4.
pub trait EventLoopExtGtk4 {
    /// True if the [`EventLoop`] uses GTK4.
    fn is_gtk4(&self) -> bool;
}

/// Additional methods when building event loop that are specific to GTK4.
pub trait EventLoopBuilderExtGtk4 {
    /// Force using GTK4.
    fn with_gtk4(&mut self) -> &mut Self;

    /// Set the GTK application ID.
    fn with_application_id(&mut self, application_id: String) -> &mut Self;
}
