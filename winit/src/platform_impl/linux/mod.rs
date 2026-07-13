#![cfg(free_unix)]

#[cfg(all(not(x11_platform), not(wayland_platform), not(gtk4_platform)))]
compile_error!("Please select a feature to build for unix: `x11`, `wayland`, `gtk4`");

use std::env;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::Duration;

#[cfg(any(x11_platform, wayland_platform))]
pub(crate) use winit_common::xkb::{physicalkey_to_scancode, scancode_to_physicalkey};
use winit_core::application::ApplicationHandler;
use winit_core::error::EventLoopError;
#[cfg(all(any(x11_platform, wayland_platform), not(gtk4_platform)))]
use winit_core::error::NotSupportedError;
use winit_core::event_loop::ActiveEventLoop;
use winit_core::event_loop::pump_events::PumpStatus;
#[cfg(gtk4_platform)]
pub(crate) use winit_gtk4 as gtk4;
#[cfg(wayland_platform)]
pub(crate) use winit_wayland as wayland;
#[cfg(x11_platform)]
pub(crate) use winit_x11 as x11;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Backend {
    #[cfg(x11_platform)]
    X,
    #[cfg(wayland_platform)]
    Wayland,
    #[cfg(gtk4_platform)]
    Gtk4,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlatformSpecificEventLoopAttributes {
    pub(crate) forced_backend: Option<Backend>,
    pub(crate) any_thread: bool,
    #[cfg(gtk4_platform)]
    pub(crate) gtk4: gtk4::PlatformSpecificEventLoopAttributes,
}

/// `linux_backend!(match expr; Enum(foo) => foo.something())`
/// expands to the equivalent of
/// ```ignore
/// match self {
///    Enum::X(foo) => foo.something(),
///    Enum::Wayland(foo) => foo.something(),
///    Enum::Gtk4(foo) => foo.something(),
/// }
/// ```
/// The result can be converted to another enum by adding `; as AnotherEnum`
macro_rules! linux_backend {
    (match $what:expr; $enum:ident ( $($c1:tt)* ) => $x:expr; as $enum2:ident ) => {
        match $what {
            #[cfg(x11_platform)]
            $enum::X($($c1)*) => $enum2::X($x),
            #[cfg(wayland_platform)]
            $enum::Wayland($($c1)*) => $enum2::Wayland($x),
            #[cfg(gtk4_platform)]
            $enum::Gtk4($($c1)*) => $enum2::Gtk4($x),
        }
    };
    (match $what:expr; $enum:ident ( $($c1:tt)* ) => $x:expr) => {
        match $what {
            #[cfg(x11_platform)]
            $enum::X($($c1)*) => $x,
            #[cfg(wayland_platform)]
            $enum::Wayland($($c1)*) => $x,
            #[cfg(gtk4_platform)]
            $enum::Gtk4($($c1)*) => $x,
        }
    };
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EventLoop {
    #[cfg(wayland_platform)]
    Wayland(Box<wayland::EventLoop>),
    #[cfg(x11_platform)]
    X(x11::EventLoop),
    #[cfg(gtk4_platform)]
    Gtk4(gtk4::EventLoop),
}

impl EventLoop {
    pub(crate) fn new(
        attributes: &PlatformSpecificEventLoopAttributes,
    ) -> Result<Self, EventLoopError> {
        if !attributes.any_thread && !is_main_thread() {
            panic!(
                "Initializing the event loop outside of the main thread is a significant \
                 cross-platform compatibility hazard. If you absolutely need to create an \
                 EventLoop on a different thread, you can use the \
                 `EventLoopBuilderExtX11::with_any_thread`, \
                 `EventLoopBuilderExtWayland::with_any_thread`, or \
                 `EventLoopBuilderExtGtk4::with_any_thread` functions."
            );
        }

        // NOTE: Wayland first because of X11 could be present under Wayland as well. Empty
        // variables are also treated as not set.
        let backend = match (
            attributes.forced_backend,
            env::var("WAYLAND_DISPLAY")
                .ok()
                .filter(|var| !var.is_empty())
                .or_else(|| env::var("WAYLAND_SOCKET").ok())
                .filter(|var| !var.is_empty())
                .is_some(),
            env::var("DISPLAY").map(|var| !var.is_empty()).unwrap_or(false),
        ) {
            // User is forcing a backend.
            (Some(backend), ..) => backend,
            // Wayland is present.
            #[cfg(wayland_platform)]
            (None, true, _) => Backend::Wayland,
            // X11 is present.
            #[cfg(x11_platform)]
            (None, _, true) => Backend::X,
            // GTK4 is present.
            #[cfg(gtk4_platform)]
            (None, ..) => Backend::Gtk4,
            // No backend is present.
            #[cfg(all(any(x11_platform, wayland_platform), not(gtk4_platform)))]
            (_, wayland_display, x11_display) => {
                let msg = if wayland_display && !cfg!(wayland_platform) {
                    "DISPLAY is not set; note: enable the `winit/wayland` feature to support \
                     Wayland"
                } else if x11_display && !cfg!(x11_platform) {
                    "neither WAYLAND_DISPLAY nor WAYLAND_SOCKET is set; note: enable the \
                     `winit/x11` feature to support X11"
                } else {
                    "neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set."
                };
                return Err(NotSupportedError::new(msg).into());
            },
        };

        // Create the display based on the backend.
        match backend {
            #[cfg(wayland_platform)]
            Backend::Wayland => EventLoop::new_wayland_any_thread(),
            #[cfg(x11_platform)]
            Backend::X => EventLoop::new_x11_any_thread(),
            #[cfg(gtk4_platform)]
            Backend::Gtk4 => EventLoop::new_gtk4_any_thread(&attributes.gtk4),
        }
    }

    #[cfg(wayland_platform)]
    fn new_wayland_any_thread() -> Result<EventLoop, EventLoopError> {
        wayland::EventLoop::new().map(|evlp| EventLoop::Wayland(Box::new(evlp)))
    }

    #[cfg(x11_platform)]
    fn new_x11_any_thread() -> Result<EventLoop, EventLoopError> {
        x11::EventLoop::new().map(EventLoop::X)
    }

    #[cfg(gtk4_platform)]
    fn new_gtk4_any_thread(
        attributes: &gtk4::PlatformSpecificEventLoopAttributes,
    ) -> Result<EventLoop, EventLoopError> {
        gtk4::EventLoop::new(attributes).map(EventLoop::Gtk4)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_wayland(&self) -> bool {
        match *self {
            #[cfg(wayland_platform)]
            EventLoop::Wayland(_) => true,
            #[cfg(any(x11_platform, gtk4_platform))]
            _ => false,
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_x11(&self) -> bool {
        match *self {
            #[cfg(x11_platform)]
            EventLoop::X(_) => true,
            #[cfg(any(wayland_platform, gtk4_platform))]
            _ => false,
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_gtk4(&self) -> bool {
        match *self {
            #[cfg(gtk4_platform)]
            EventLoop::Gtk4(_) => true,
            #[cfg(any(x11_platform, wayland_platform))]
            _ => false,
        }
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        app: A,
    ) -> Result<(), EventLoopError> {
        linux_backend!(match self; EventLoop(evlp) => evlp.run_app_on_demand(app))
    }

    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        timeout: Option<Duration>,
        app: A,
    ) -> PumpStatus {
        linux_backend!(match self; EventLoop(evlp) => evlp.pump_app_events(timeout, app))
    }

    pub fn window_target(&self) -> &dyn ActiveEventLoop {
        linux_backend!(match self; EventLoop(evlp) => evlp.window_target())
    }
}

impl AsFd for EventLoop {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            #[cfg(x11_platform)]
            EventLoop::X(evlp) => evlp.as_fd(),
            #[cfg(wayland_platform)]
            EventLoop::Wayland(evlp) => evlp.as_fd(),
            #[cfg(gtk4_platform)]
            EventLoop::Gtk4(_) => {
                panic!(
                    "GTK4 EventLoop does not support AsFd: GLib exposes a dynamic poll set, not \
                     one stable event-loop file descriptor"
                )
            },
        }
    }
}

impl AsRawFd for EventLoop {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            #[cfg(x11_platform)]
            EventLoop::X(evlp) => evlp.as_raw_fd(),
            #[cfg(wayland_platform)]
            EventLoop::Wayland(evlp) => evlp.as_raw_fd(),
            #[cfg(gtk4_platform)]
            EventLoop::Gtk4(_) => {
                panic!(
                    "GTK4 EventLoop does not support AsRawFd: GLib exposes a dynamic poll set, \
                     not one stable event-loop file descriptor"
                )
            },
        }
    }
}

#[cfg(target_os = "linux")]
fn is_main_thread() -> bool {
    rustix::thread::gettid() == rustix::process::getpid()
}

#[cfg(any(target_os = "dragonfly", target_os = "freebsd", target_os = "openbsd"))]
fn is_main_thread() -> bool {
    use libc::pthread_main_np;

    unsafe { pthread_main_np() == 1 }
}

#[cfg(target_os = "netbsd")]
fn is_main_thread() -> bool {
    std::thread::current().name() == Some("main")
}
