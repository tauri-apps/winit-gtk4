use std::fmt;
use std::sync::{Arc, Mutex};

use dpi::{LogicalSize, PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::gdk::prelude::{DeviceExt, DisplayExt, SeatExt, SurfaceExt};
use gtk4::prelude::*;
use winit_core::cursor::Cursor;
use winit_core::error::RequestError;
use winit_core::event::WindowEvent;
use winit_core::icon::Icon;
use winit_core::keyboard::ModifiersState;
use winit_core::monitor::{Fullscreen, MonitorHandle};
use winit_core::window::{
    CursorGrabMode, ImeCapabilities, ImeRequest, ImeRequestError, ResizeDirection, Theme,
    UserAttentionType, Window as CoreWindow, WindowAttributes, WindowButtons, WindowId,
    WindowLevel,
};

use crate::event_loop::{ActiveEventLoop, OwnedDisplayHandle};
use crate::sink::CommandSink;

mod keyboards;
mod pointers;
mod state;

pub(crate) use state::WindowState;

pub struct Window {
    window_id: WindowId,
    commands: Arc<Mutex<CommandSink>>,
    context: gtk4::glib::MainContext,
    display_handle: OwnedDisplayHandle,
    window_handle: Option<rwh_06::RawWindowHandle>,
    state: Arc<Mutex<WindowState>>,
}

impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Window").field("window_id", &self.window_id).finish()
    }
}

unsafe impl Send for Window {}
unsafe impl Sync for Window {}

impl Window {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<Self, RequestError> {
        let scale_factor =
            guessed_monitor().map(|monitor| monitor.scale_factor().max(1) as f64).unwrap_or(1.0);

        let logical_size = attributes
            .surface_size
            .map(|size| size.to_logical::<u32>(scale_factor))
            .unwrap_or_else(|| LogicalSize::new(800, 600));

        let gtk_window = gtk4::ApplicationWindow::builder()
            .application(&event_loop.shared.borrow().app)
            .title(&attributes.title)
            .default_width(logical_size.width as i32)
            .default_height(logical_size.height as i32)
            .build();

        let surface_size = logical_size.to_physical::<u32>(scale_factor);
        let window_id = WindowId::from_raw(gtk_window.as_ptr() as usize);

        let title = attributes.title;
        let visible = attributes.visible;

        let state = WindowState {
            surface_size,
            last_layout: None,
            scale_factor,
            visible,
            has_focus: false,
            modifiers: ModifiersState::default(),
            held_key_press: None,
            title,
        };
        let state = Arc::new(Mutex::new(state));

        let commands = event_loop.shared.borrow().commands.clone();
        Self::connect_events(event_loop, &gtk_window, window_id, &state);

        if visible {
            gtk_window.present();
        }

        let window_handle = raw_window_handle(&gtk_window);

        {
            let mut shared = event_loop.shared.borrow_mut();
            shared.windows.insert(window_id, gtk_window.clone());
        }

        Ok(Self {
            window_id,
            context: event_loop.context.clone(),
            commands,
            display_handle: event_loop.display_handle,
            window_handle,
            state,
        })
    }

    fn connect_events(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
        state: &Arc<Mutex<WindowState>>,
    ) {
        Self::connect_close_request(event_loop, gtk_window, window_id);
        Self::connect_destroy(event_loop, gtk_window, window_id);
        Self::connect_focus(event_loop, gtk_window, window_id, state);
        Self::connect_surface_layout(event_loop, gtk_window, window_id, state);
        keyboards::connect(event_loop, gtk_window, window_id, state);
        pointers::connect(event_loop, gtk_window, window_id, state);
    }

    fn connect_close_request(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
    ) {
        let shared = event_loop.shared.clone();
        gtk_window.connect_close_request(move |_| {
            let mut shared = shared.borrow_mut();
            shared.events_sink.push_window_event(WindowEvent::CloseRequested, window_id);
            gtk4::glib::Propagation::Stop
        });
    }

    fn connect_destroy(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
    ) {
        let shared = event_loop.shared.clone();
        gtk_window.connect_destroy(move |_| {
            let mut shared = shared.borrow_mut();
            shared.windows.remove(&window_id);
            shared.events_sink.push_window_event(WindowEvent::Destroyed, window_id);
        });
    }

    fn connect_focus(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
        state: &Arc<Mutex<WindowState>>,
    ) {
        let shared = event_loop.shared.clone();
        let state = state.clone();

        gtk_window.connect_is_active_notify(move |window| {
            let focused = window.is_active();
            let modifiers = if focused {
                let modifiers = WidgetExt::display(window)
                    .default_seat()
                    .and_then(|seat| seat.keyboard())
                    .map(|keyboard| keyboard.modifier_state())
                    .unwrap_or_else(gtk4::gdk::ModifierType::empty);

                keyboards::gdk_mods_to_winit_mods(modifiers)
            } else {
                ModifiersState::empty()
            };

            {
                let mut state = state.lock().unwrap();
                if state.has_focus == focused {
                    return;
                }

                state.has_focus = focused;
                state.modifiers = modifiers;
                if !focused {
                    state.held_key_press = None;
                }
            }

            let mut shared = shared.borrow_mut();
            let event_sink = &mut shared.events_sink;
            if focused {
                let focus_event = WindowEvent::Focused(true);
                let mods_event = WindowEvent::ModifiersChanged(modifiers.into());
                event_sink.push_window_event(focus_event, window_id);
                event_sink.push_window_event(mods_event, window_id);
            } else {
                let mods_event = WindowEvent::ModifiersChanged(ModifiersState::empty().into());
                let focus_event = WindowEvent::Focused(false);
                event_sink.push_window_event(mods_event, window_id);
                event_sink.push_window_event(focus_event, window_id);
            }
        });
    }

    fn connect_surface_layout(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
        state: &Arc<Mutex<WindowState>>,
    ) {
        let shared = event_loop.shared.clone();
        let state = state.clone();

        gtk_window.connect_realize(move |window| {
            if let Some(surface) = window.surface() {
                let shared = shared.clone();
                let state = state.clone();
                surface.connect_layout(move |_, width, height| {
                    let width = width.max(0) as u32;
                    let height = height.max(0) as u32;
                    let scale_factor = state.lock().unwrap().scale_factor;
                    let surface_size = LogicalSize::new(width, height).to_physical(scale_factor);

                    let resized = {
                        let mut state = state.lock().unwrap();
                        let resized = state
                            .last_layout
                            .map(|last_layout| last_layout != surface_size)
                            .unwrap_or(true);

                        if resized {
                            state.last_layout = Some(surface_size);
                            state.surface_size = surface_size;
                            true
                        } else {
                            false
                        }
                    };

                    if resized {
                        let event = WindowEvent::SurfaceResized(surface_size);
                        let mut shared = shared.borrow_mut();
                        shared.events_sink.push_window_event(event, window_id);
                    }
                });
            }
        });
    }

    fn queue_command(&self, command: WindowCommand) {
        self.commands.lock().unwrap().push_window_command(self.window_id, command);
        self.context.wakeup();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.commands.lock().unwrap().push_window_command(self.window_id, WindowCommand::Close);
        self.context.wakeup();
    }
}

impl CoreWindow for Window {
    fn id(&self) -> WindowId {
        self.window_id
    }

    fn scale_factor(&self) -> f64 {
        self.state.lock().unwrap().scale_factor
    }

    fn request_redraw(&self) {
        self.queue_command(WindowCommand::RequestRedraw);
    }

    fn pre_present_notify(&self) {}

    fn reset_dead_keys(&self) {
        todo!("GTK4 dead-key reset is not implemented yet")
    }

    fn surface_position(&self) -> PhysicalPosition<i32> {
        PhysicalPosition::new(0, 0)
    }

    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        todo!("GTK4 outer_position is not implemented yet")
    }

    fn set_outer_position(&self, _position: Position) {
        todo!("GTK4 set_outer_position is not implemented yet")
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        self.state.lock().unwrap().surface_size
    }

    fn request_surface_size(&self, _size: Size) -> Option<PhysicalSize<u32>> {
        todo!("GTK4 request_surface_size is not implemented yet")
    }

    fn outer_size(&self) -> PhysicalSize<u32> {
        todo!("GTK4 outer_size is not implemented yet")
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        PhysicalInsets::new(0, 0, 0, 0)
    }

    fn set_min_surface_size(&self, _min_size: Option<Size>) {
        todo!("GTK4 set_min_surface_size is not implemented yet")
    }

    fn set_max_surface_size(&self, _max_size: Option<Size>) {
        todo!("GTK4 set_max_surface_size is not implemented yet")
    }

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        todo!("GTK4 surface_resize_increments is not implemented yet")
    }

    fn set_surface_resize_increments(&self, _increments: Option<Size>) {
        todo!("GTK4 set_surface_resize_increments is not implemented yet")
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        self.state.lock().unwrap().title = title.clone();
        self.queue_command(WindowCommand::SetTitle(title));
    }

    fn set_transparent(&self, _transparent: bool) {
        todo!("GTK4 set_transparent is not implemented yet")
    }

    fn set_blur(&self, _blur: bool) {
        todo!("GTK4 set_blur is not implemented yet")
    }

    fn set_visible(&self, visible: bool) {
        self.state.lock().unwrap().visible = visible;
        self.queue_command(WindowCommand::SetVisible(visible));
    }

    fn is_visible(&self) -> Option<bool> {
        Some(self.state.lock().unwrap().visible)
    }

    fn set_resizable(&self, _resizable: bool) {
        todo!("GTK4 set_resizable is not implemented yet")
    }

    fn is_resizable(&self) -> bool {
        todo!("GTK4 is_resizable is not implemented yet")
    }

    fn set_enabled_buttons(&self, _buttons: WindowButtons) {
        todo!("GTK4 set_enabled_buttons is not implemented yet")
    }

    fn enabled_buttons(&self) -> WindowButtons {
        todo!("GTK4 enabled_buttons is not implemented yet")
    }

    fn set_minimized(&self, _minimized: bool) {
        todo!("GTK4 set_minimized is not implemented yet")
    }

    fn is_minimized(&self) -> Option<bool> {
        todo!("GTK4 is_minimized is not implemented yet")
    }

    fn set_maximized(&self, _maximized: bool) {
        todo!("GTK4 set_maximized is not implemented yet")
    }

    fn is_maximized(&self) -> bool {
        todo!("GTK4 is_maximized is not implemented yet")
    }

    fn set_fullscreen(&self, _fullscreen: Option<Fullscreen>) {
        todo!("GTK4 set_fullscreen is not implemented yet")
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        todo!("GTK4 fullscreen is not implemented yet")
    }

    fn set_decorations(&self, _decorations: bool) {
        todo!("GTK4 set_decorations is not implemented yet")
    }

    fn is_decorated(&self) -> bool {
        todo!("GTK4 is_decorated is not implemented yet")
    }

    fn set_window_level(&self, _level: WindowLevel) {
        todo!("GTK4 set_window_level is not implemented yet")
    }

    fn set_window_icon(&self, _window_icon: Option<Icon>) {
        todo!("GTK4 set_window_icon is not implemented yet")
    }

    fn request_ime_update(&self, _request: ImeRequest) -> Result<(), ImeRequestError> {
        // TODO: implement IME support.
        Err(ImeRequestError::NotSupported)
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        // TODO: report real IME capabilities
        None
    }

    fn focus_window(&self) {
        todo!("GTK4 focus_window is not implemented yet")
    }

    fn has_focus(&self) -> bool {
        self.state.lock().unwrap().has_focus
    }

    fn request_user_attention(&self, _request_type: Option<UserAttentionType>) {
        todo!("GTK4 request_user_attention is not implemented yet")
    }

    fn set_theme(&self, _theme: Option<Theme>) {
        todo!("GTK4 set_theme is not implemented yet")
    }

    fn theme(&self) -> Option<Theme> {
        None
    }

    fn set_content_protected(&self, _protected: bool) {
        todo!("GTK4 set_content_protected is not implemented yet")
    }

    fn title(&self) -> String {
        self.state.lock().unwrap().title.clone()
    }

    fn set_cursor(&self, _cursor: Cursor) {
        todo!("GTK4 set_cursor is not implemented yet")
    }

    fn set_cursor_position(&self, _position: Position) -> Result<(), RequestError> {
        todo!("GTK4 set_cursor_position is not implemented yet")
    }

    fn set_cursor_grab(&self, _mode: CursorGrabMode) -> Result<(), RequestError> {
        todo!("GTK4 set_cursor_grab is not implemented yet")
    }

    fn set_cursor_visible(&self, _visible: bool) {
        todo!("GTK4 set_cursor_visible is not implemented yet")
    }

    fn drag_window(&self) -> Result<(), RequestError> {
        todo!("GTK4 drag_window is not implemented yet")
    }

    fn drag_resize_window(&self, _direction: ResizeDirection) -> Result<(), RequestError> {
        todo!("GTK4 drag_resize_window is not implemented yet")
    }

    fn show_window_menu(&self, _position: Position) {
        todo!("GTK4 show_window_menu is not implemented yet")
    }

    fn set_cursor_hittest(&self, _hittest: bool) -> Result<(), RequestError> {
        todo!("GTK4 set_cursor_hittest is not implemented yet")
    }

    fn current_monitor(&self) -> Option<MonitorHandle> {
        todo!("GTK4 current_monitor is not implemented yet")
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = MonitorHandle>> {
        todo!("GTK4 available_monitors is not implemented yet")
    }

    fn primary_monitor(&self) -> Option<MonitorHandle> {
        todo!("GTK4 primary_monitor is not implemented yet")
    }

    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }
}

#[derive(Debug)]
pub(crate) enum WindowCommand {
    Close,
    RequestRedraw,
    SetTitle(String),
    SetVisible(bool),
}

impl WindowCommand {
    pub(crate) fn apply_to(self, window: &gtk4::ApplicationWindow) {
        match self {
            WindowCommand::Close => window.close(),
            WindowCommand::RequestRedraw => { /* Handled in event_loop.rs */ },
            WindowCommand::SetTitle(title) => window.set_title(Some(&title)),
            WindowCommand::SetVisible(visible) => window.set_visible(visible),
        }
    }
}

fn guessed_monitor() -> Option<gtk4::gdk::Monitor> {
    let display = gtk4::gdk::Display::default()?;

    monitor_under_pointer(&display).or_else(|| first_monitor(&display))
}

fn monitor_under_pointer(display: &gtk4::gdk::Display) -> Option<gtk4::gdk::Monitor> {
    display
        .default_seat()
        .and_then(|seat| seat.pointer())
        .and_then(|pointer| pointer.surface_at_position().0)
        .and_then(|surface| display.monitor_at_surface(&surface))
}

fn first_monitor(display: &gtk4::gdk::Display) -> Option<gtk4::gdk::Monitor> {
    display.monitors().item(0).and_then(|monitor| monitor.downcast::<gtk4::gdk::Monitor>().ok())
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = self.window_handle.ok_or(rwh_06::HandleError::Unavailable)?;

        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw)) }
    }
}

impl rwh_06::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        self.display_handle.display_handle()
    }
}

fn raw_window_handle(window: &gtk4::ApplicationWindow) -> Option<rwh_06::RawWindowHandle> {
    let surface = window.surface()?;

    if let Ok(surface) = surface.clone().downcast::<gdk4_wayland::WaylandSurface>() {
        let surface = surface.wl_surface_raw()?;
        return Some(rwh_06::WaylandWindowHandle::new(surface).into());
    }

    if let Ok(surface) = surface.downcast::<gdk4_x11::X11Surface>() {
        let window = surface.xid() as _;
        if window != 0 {
            return Some(rwh_06::XlibWindowHandle::new(window).into());
        }
    }

    None
}
