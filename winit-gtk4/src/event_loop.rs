use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use gtk4::prelude::*;
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{CustomCursor as CoreCustomCursor, CustomCursorSource};
use winit_core::error::{EventLoopError, NotSupportedError, OsError, RequestError};
use winit_core::event::{StartCause, SurfaceSizeWriter, WindowEvent};
use winit_core::event_loop::pump_events::PumpStatus;
use winit_core::event_loop::{
    ActiveEventLoop as CoreActiveEventLoop, ControlFlow, DeviceEvents,
    EventLoopProxy as CoreEventLoopProxy, EventLoopProxyProvider,
    OwnedDisplayHandle as CoreOwnedDisplayHandle,
};
use winit_core::monitor::MonitorHandle;
use winit_core::window::{Theme, Window as CoreWindow, WindowAttributes, WindowId};

use crate::cursor::GtkCustomCursor;
use crate::sink::{Command, CommandSink, EventSink};
use crate::window::{UnownedWindow, Window, theme_from_settings};

#[derive(Debug)]
pub(crate) enum Event {
    Window { window_id: WindowId, event: winit_core::event::WindowEvent },
    ScaleFactorChanged { window_id: WindowId, scale_factor: f64, surface_size: PhysicalSize<u32> },
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) app: gtk4::Application,
    pub(crate) commands: Arc<Mutex<CommandSink>>,
    pub(crate) events_sink: EventSink,
    pub(crate) windows: HashMap<WindowId, Weak<UnownedWindow>>,
}

#[derive(Debug)]
pub(crate) struct RunState {
    control_flow: Cell<ControlFlow>,
    exit: Cell<Option<i32>>,
    proxy_wake_up: Arc<AtomicBool>,
}

/// GTK4 event loop implementation.
///
/// The event loop owns a GTK [`Application`](gtk4::Application) and drives
/// winit callbacks from GLib's default [`MainContext`](gtk4::glib::MainContext).
/// Create it on the main thread unless the public winit builder has been
/// configured to allow another thread. GTK objects created by this backend must
/// still be used according to GTK's thread and main-context rules.
///
/// GTK4 does not expose one stable event-loop file descriptor; users that need
/// to wake the loop from another thread should use a winit event-loop proxy.
#[derive(Debug)]
pub struct EventLoop {
    active_event_loop: ActiveEventLoop,
    buffer_command_sink: CommandSink,
    buffer_sink: EventSink,
    loop_running: bool,
    context: gtk4::glib::MainContext,
}

/// GTK4-specific event loop attributes.
///
/// These attributes configure the process-wide GTK application created for the
/// event loop. They are normally set through
/// [`EventLoopBuilderExtGtk4`](crate::EventLoopBuilderExtGtk4) on the public
/// `winit` event-loop builder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlatformSpecificEventLoopAttributes {
    /// GTK application ID used when registering the underlying
    /// [`gtk4::Application`].
    ///
    /// When set, the value must be a valid
    /// [`GApplication` ID](gtk4::gio::Application::id_is_valid), for example
    /// `org.example.MyApplication`. Invalid IDs make event-loop construction
    /// return [`EventLoopError::NotSupported`]. When unset, the GTK application
    /// is registered as non-unique.
    pub application_id: Option<String>,
}

/// Active GTK4 event-loop target.
///
/// This value is passed to application callbacks while the GTK4 event loop is
/// running. It creates GTK-backed windows and cursors, reports monitor and theme
/// information through GDK, and exposes the raw display handle for the runtime
/// GDK backend, currently Wayland or X11.
///
/// The target is tied to GLib's default main context. GTK objects reachable from
/// windows created through this target must be accessed on the thread and main
/// context required by GTK.
#[derive(Clone)]
pub struct ActiveEventLoop {
    pub(crate) display_handle: OwnedDisplayHandle,
    pub(crate) xconn: Option<Arc<crate::x11::XConnection>>,
    pub(crate) shared: Rc<RefCell<SharedState>>,
    pub(crate) run_state: Rc<RunState>,
    event_loop_proxy: CoreEventLoopProxy,
    pub(crate) context: gtk4::glib::MainContext,
}

impl fmt::Debug for ActiveEventLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActiveEventLoop").finish_non_exhaustive()
    }
}

impl ActiveEventLoop {
    pub(crate) fn clear_exit(&self) {
        self.run_state.exit.set(None);
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.run_state.exit.get()
    }
}

impl EventLoop {
    /// Creates a GTK4 event loop from GTK4-specific attributes.
    ///
    /// This registers a [`gtk4::Application`], opens the default GDK display,
    /// captures its raw display handle, and prepares the GLib main context used
    /// by the event loop.
    ///
    /// Only one GTK4 event loop may be created in a process. A second
    /// construction attempt returns [`EventLoopError::RecreationAttempt`].
    /// Invalid application IDs return [`EventLoopError::NotSupported`], and
    /// failures from GTK/GDK display initialization are returned as
    /// [`EventLoopError::Os`].
    pub fn new(attributes: &PlatformSpecificEventLoopAttributes) -> Result<Self, EventLoopError> {
        static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);
        if EVENT_LOOP_CREATED.swap(true, Ordering::Relaxed) {
            return Err(EventLoopError::RecreationAttempt);
        }

        let application_id = attributes.application_id.as_deref();

        if let Some(application_id) = application_id {
            if !gtk4::gio::Application::id_is_valid(application_id) {
                return Err(EventLoopError::NotSupported(NotSupportedError::new(
                    "invalid GTK application ID",
                )));
            }
        }

        let flags = if application_id.is_some() {
            gtk4::gio::ApplicationFlags::empty()
        } else {
            gtk4::gio::ApplicationFlags::NON_UNIQUE
        };

        gtk4::init().map_err(|err| EventLoopError::Os(OsError::new(line!(), file!(), err)))?;

        let app = gtk4::Application::new(application_id, flags);
        app.connect_activate(|_| {});
        app.register(None::<&gtk4::gio::Cancellable>)
            .map_err(|err| EventLoopError::Os(OsError::new(line!(), file!(), err)))?;

        let shared = Rc::new(RefCell::new(SharedState {
            app,
            commands: Arc::new(Mutex::new(CommandSink::new())),
            events_sink: EventSink::new(),
            windows: HashMap::new(),
        }));
        let context = gtk4::glib::MainContext::default();

        let run_state = Rc::new(RunState {
            control_flow: Cell::new(ControlFlow::default()),
            exit: Cell::new(None),
            proxy_wake_up: Arc::new(AtomicBool::new(false)),
        });
        let display_handle = OwnedDisplayHandle::new()?;
        let xconn = gtk4::gdk::Display::default().and_then(crate::x11::x_connection);
        let event_loop_proxy =
            EventLoopProxy::new(run_state.proxy_wake_up.clone(), context.clone()).into();

        Ok(Self {
            active_event_loop: ActiveEventLoop {
                display_handle,
                xconn,
                shared,
                run_state,
                event_loop_proxy,
                context: context.clone(),
            },
            buffer_command_sink: CommandSink::new(),
            buffer_sink: EventSink::new(),
            loop_running: false,
            context,
        })
    }

    /// Returns the active event-loop target used to create windows and proxies.
    pub fn window_target(&self) -> &dyn CoreActiveEventLoop {
        &self.active_event_loop
    }

    /// Runs an [`ApplicationHandler`] until the application exits.
    ///
    /// This method repeatedly pumps GTK/GLib events on the calling thread and
    /// dispatches resulting winit callbacks. Unlike the one-shot `run_app`
    /// entry point exposed by the public `winit` crate, this backend method can
    /// be called again after it returns.
    ///
    /// An exit code of `0` returns `Ok(())`; any other exit code is returned as
    /// [`EventLoopError::ExitFailure`].
    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        self.active_event_loop.clear_exit();
        let exit = loop {
            match self.pump_app_events(None, &mut app) {
                PumpStatus::Exit(0) => {
                    break Ok(());
                },
                PumpStatus::Exit(code) => {
                    break Err(EventLoopError::ExitFailure(code));
                },
                PumpStatus::Continue => {
                    continue;
                },
            }
        };

        self.flush_pending_glib_events();
        exit
    }

    /// Runs one iteration of GTK4 event processing for an [`ApplicationHandler`].
    ///
    /// On the first call this activates the underlying GTK application and emits
    /// the initial winit lifecycle callbacks. Later calls dispatch pending GLib
    /// events, queued window commands, proxy wakeups, and winit window events.
    ///
    /// The optional timeout caps how long this call may block while waiting for
    /// work, after also considering the current [`ControlFlow`]. The returned
    /// [`PumpStatus`] indicates whether the application should continue polling
    /// or has requested exit.
    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        timeout: Option<Duration>,
        mut app: A,
    ) -> PumpStatus {
        if !self.loop_running {
            self.loop_running = true;

            let gtk_app = self.active_event_loop.shared.borrow().app.clone();
            gtk_app.activate();

            // Run the initial loop iteration.
            self.single_iteration(&mut app, StartCause::Init);
        }

        // Consider the possibility that the `StartCause::Init` iteration could
        // request to Exit.
        if !self.exiting() {
            self.poll_events_with_timeout(timeout, &mut app);
        }

        if let Some(code) = self.exit_code() {
            self.loop_running = false;
            self.active_event_loop.run_state.proxy_wake_up.store(false, Ordering::Relaxed);

            PumpStatus::Exit(code)
        } else {
            PumpStatus::Continue
        }
    }

    fn poll_events_with_timeout<A: ApplicationHandler>(
        &mut self,
        mut timeout: Option<Duration>,
        app: &mut A,
    ) {
        let start = Instant::now();
        let has_pending = self.has_pending();

        timeout = if has_pending {
            // If we already have work to do then we don't want to block on the next poll.
            Some(Duration::ZERO)
        } else {
            let control_flow_timeout = match self.control_flow() {
                ControlFlow::Wait => None,
                ControlFlow::Poll => Some(Duration::ZERO),
                ControlFlow::WaitUntil(wait_deadline) => {
                    Some(wait_deadline.saturating_duration_since(start))
                },
            };

            min_timeout(control_flow_timeout, timeout)
        };

        let mut dispatched_glib = false;

        match timeout {
            Some(Duration::ZERO) => {
                dispatched_glib |= self.flush_pending_glib_events();
            },
            Some(timeout) => {
                let expired = Arc::new(AtomicBool::new(false));
                let expired_clone = expired.clone();
                let source = gtk4::glib::timeout_add_once(timeout, move || {
                    expired_clone.store(true, Ordering::Relaxed);
                });

                while !expired.load(Ordering::Relaxed) && !self.exiting() && !self.has_pending() {
                    dispatched_glib |= self.context.iteration(true);
                }

                if !expired.load(Ordering::Relaxed) {
                    source.remove();
                }
                dispatched_glib |= self.flush_pending_glib_events();
            },
            None => {
                while !self.exiting() && !self.has_pending() {
                    dispatched_glib |= self.context.iteration(true);
                }
                dispatched_glib |= self.flush_pending_glib_events();
            },
        }

        // NB: `StartCause::Init` is handled as a special case and doesn't need
        // to be considered here.
        let cause = match self.control_flow() {
            ControlFlow::Poll => StartCause::Poll,
            ControlFlow::Wait => StartCause::WaitCancelled { start, requested_resume: None },
            ControlFlow::WaitUntil(deadline) => {
                if Instant::now() < deadline {
                    StartCause::WaitCancelled { start, requested_resume: Some(deadline) }
                } else {
                    StartCause::ResumeTimeReached { start, requested_resume: deadline }
                }
            },
        };

        // False positive / spurious wake ups could lead to us spamming
        // redundant iterations of the event loop with no new events to
        // dispatch.
        if !dispatched_glib
            && !self.has_pending()
            && !matches!(&cause, StartCause::ResumeTimeReached { .. } | StartCause::Poll)
            && timeout.is_none()
        {
            return;
        }

        self.single_iteration(app, cause);
    }

    fn has_pending(&self) -> bool {
        let shared = self.active_event_loop.shared.borrow();
        self.active_event_loop.run_state.proxy_wake_up.load(Ordering::Relaxed)
            || !shared.commands.lock().unwrap().is_empty()
            || !shared.events_sink.is_empty()
    }

    fn single_iteration<A: ApplicationHandler>(&mut self, app: &mut A, cause: StartCause) {
        app.new_events(&self.active_event_loop, cause);

        if cause == StartCause::Init {
            app.can_create_surfaces(&self.active_event_loop);
        }

        if self.active_event_loop.run_state.proxy_wake_up.swap(false, Ordering::Relaxed) {
            app.proxy_wake_up(&self.active_event_loop);
        }

        {
            let command_sink = self.active_event_loop.shared.borrow().commands.clone();
            self.buffer_command_sink.append(&mut command_sink.lock().unwrap());
        }
        for command in self.buffer_command_sink.drain() {
            match command {
                Command::Window { window_id, command } => {
                    let window = {
                        let shared = self.active_event_loop.shared.borrow();
                        shared.windows.get(&window_id).and_then(Weak::upgrade)
                    };

                    let Some(window) = window else {
                        continue;
                    };

                    command.apply_to(&window);
                },
                Command::CloseWindow(window) => window.close(),
            }
        }

        {
            let mut shared = self.active_event_loop.shared.borrow_mut();
            self.buffer_sink.append(&mut shared.events_sink);
        }
        for event in self.buffer_sink.drain() {
            match event {
                Event::Window { window_id, event } => {
                    app.window_event(&self.active_event_loop, window_id, event);
                },
                Event::ScaleFactorChanged { window_id, scale_factor, surface_size } => {
                    let old_surface_size = surface_size;
                    let surface_size = Arc::new(Mutex::new(surface_size));
                    let event = WindowEvent::ScaleFactorChanged {
                        scale_factor,
                        surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(&surface_size)),
                    };

                    app.window_event(&self.active_event_loop, window_id, event);

                    let surface_size = *surface_size.lock().unwrap();
                    if surface_size != old_surface_size {
                        let window = {
                            let shared = self.active_event_loop.shared.borrow();
                            shared.windows.get(&window_id).and_then(Weak::upgrade)
                        };

                        let Some(window) = window else {
                            continue;
                        };

                        let logical_size = surface_size.to_logical::<i32>(scale_factor);
                        let (width, height) = logical_size.into();
                        window.gtk_window.set_default_size(width, height);
                    }
                },
            }
        }

        app.about_to_wait(&self.active_event_loop);
    }

    fn control_flow(&self) -> ControlFlow {
        self.active_event_loop.control_flow()
    }

    fn exiting(&self) -> bool {
        self.active_event_loop.exiting()
    }

    fn exit_code(&self) -> Option<i32> {
        self.active_event_loop.exit_code()
    }

    fn flush_pending_glib_events(&mut self) -> bool {
        let mut dispatched = false;
        while self.context.pending() {
            dispatched |= self.context.iteration(false);
        }
        dispatched
    }
}

#[derive(Clone, Debug)]
pub struct EventLoopProxy {
    proxy_wake_up: Arc<AtomicBool>,
    context: gtk4::glib::MainContext,
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        self.proxy_wake_up.store(true, Ordering::Relaxed);
        self.context.wakeup();
    }
}

impl EventLoopProxy {
    fn new(proxy_wake_up: Arc<AtomicBool>, context: gtk4::glib::MainContext) -> Self {
        Self { proxy_wake_up, context }
    }
}

impl From<EventLoopProxy> for CoreEventLoopProxy {
    fn from(value: EventLoopProxy) -> Self {
        CoreEventLoopProxy::new(Arc::new(value))
    }
}

impl CoreActiveEventLoop for ActiveEventLoop {
    fn create_proxy(&self) -> CoreEventLoopProxy {
        self.event_loop_proxy.clone()
    }

    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, RequestError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn create_custom_cursor(
        &self,
        custom_cursor: CustomCursorSource,
    ) -> Result<CoreCustomCursor, RequestError> {
        Ok(CoreCustomCursor(Arc::new(GtkCustomCursor::new(custom_cursor)?)))
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = MonitorHandle>> {
        crate::monitor::available_monitors()
    }

    fn primary_monitor(&self) -> Option<MonitorHandle> {
        crate::monitor::primary_monitor()
    }

    fn listen_device_events(&self, _allowed: DeviceEvents) {
        // GTK4/GDK does not expose backend-neutral raw device event capture.
    }

    fn system_theme(&self) -> Option<Theme> {
        gtk4::Settings::default().map(|settings| theme_from_settings(&settings))
    }

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.run_state.control_flow.set(control_flow);
    }

    fn control_flow(&self) -> ControlFlow {
        self.run_state.control_flow.get()
    }

    fn exit(&self) {
        self.run_state.exit.set(Some(0));
        self.context.wakeup();
    }

    fn exiting(&self) -> bool {
        self.run_state.exit.get().is_some()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        CoreOwnedDisplayHandle::new(Arc::new(self.display_handle))
    }

    fn rwh_06_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum OwnedDisplayHandle {
    Wayland { display: NonNull<c_void> },
    Xlib { display: NonNull<c_void>, screen: c_int },
}

unsafe impl Send for OwnedDisplayHandle {}
unsafe impl Sync for OwnedDisplayHandle {}

impl OwnedDisplayHandle {
    fn new() -> Result<Self, EventLoopError> {
        raw_display_handle().map_err(|err| {
            EventLoopError::Os(OsError::new(
                line!(),
                file!(),
                format!("failed to get GTK display handle: {err}"),
            ))
        })
    }
}

impl rwh_06::HasDisplayHandle for ActiveEventLoop {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        self.display_handle.display_handle()
    }
}

impl rwh_06::HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = match *self {
            OwnedDisplayHandle::Wayland { display } => {
                rwh_06::WaylandDisplayHandle::new(display).into()
            },
            OwnedDisplayHandle::Xlib { display, screen } => {
                rwh_06::XlibDisplayHandle::new(Some(display), screen).into()
            },
        };
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

fn raw_display_handle() -> Result<OwnedDisplayHandle, rwh_06::HandleError> {
    let display = gtk4::gdk::Display::default().ok_or(rwh_06::HandleError::Unavailable)?;

    if let Some(handle) = crate::wayland::raw_display_handle(display.clone())? {
        return Ok(handle);
    }

    if let Some(handle) = crate::x11::raw_display_handle(display)? {
        return Ok(handle);
    }

    Err(rwh_06::HandleError::NotSupported)
}

fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (None, None) => None,
        (None, Some(duration)) | (Some(duration), None) => Some(duration),
        (Some(a), Some(b)) => Some(a.min(b)),
    }
}
