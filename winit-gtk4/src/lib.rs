//! Winit's GTK4 backend.

mod cursor;
mod event_loop;
mod monitor;
mod sink;
mod wayland;
mod window;
mod x11;

use winit_core::event_loop::ActiveEventLoop as CoreActiveEventLoop;
use winit_core::window::{PlatformWindowAttributes, Window as CoreWindow};

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

    /// Whether to allow the event loop to be created off of the main thread.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;

    /// Set the GTK application ID.
    fn with_application_id(&mut self, application_id: String) -> &mut Self;
}

/// Additional methods on [`Window`] that are specific to GTK4.
///
/// [`Window`]: winit_core::window::Window
pub trait WindowExtGtk4 {
    /// Returns the underlying GTK application window.
    ///
    /// GTK objects must be used according to GTK's thread and main-context rules.
    fn gtk_window(&self) -> Option<gtk4::ApplicationWindow>;

    /// Returns the underlying GDK surface, if the GTK window has been realized.
    fn gdk_surface(&self) -> Option<gtk4::gdk::Surface>;
}

impl WindowExtGtk4 for dyn CoreWindow + '_ {
    #[inline]
    fn gtk_window(&self) -> Option<gtk4::ApplicationWindow> {
        Some(self.cast_ref::<Window>()?.gtk_window())
    }

    #[inline]
    fn gdk_surface(&self) -> Option<gtk4::gdk::Surface> {
        self.cast_ref::<Window>()?.gdk_surface()
    }
}

/// Window attributes specific to GTK4.
///
/// GTK application identity is configured on
/// [`EventLoopBuilderExtGtk4::with_application_id`] because it is a process/application property,
/// not a per-window setting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct WindowAttributesGtk4 {}

impl PlatformWindowAttributes for WindowAttributesGtk4 {
    fn box_clone(&self) -> Box<dyn PlatformWindowAttributes> {
        Box::new(self.clone())
    }
}
