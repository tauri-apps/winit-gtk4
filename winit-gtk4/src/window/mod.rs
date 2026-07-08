use std::cell::RefCell;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};

use dpi::{LogicalSize, PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use gtk4::gdk::prelude::{DeviceExt, DisplayExt, SeatExt, SurfaceExt};
use gtk4::prelude::*;
use winit_core::cursor::{Cursor, CursorIcon};
use winit_core::error::RequestError;
use winit_core::event::WindowEvent;
use winit_core::icon::{Icon, RgbaIcon};
use winit_core::keyboard::ModifiersState;
use winit_core::monitor::{Fullscreen, MonitorHandle};
use winit_core::window::{
    CursorGrabMode, ImeCapabilities, ImeRequest, ImeRequestError, ResizeDirection, Theme,
    UserAttentionType, Window as CoreWindow, WindowAttributes, WindowButtons, WindowId,
    WindowLevel,
};

use crate::event_loop::{ActiveEventLoop, OwnedDisplayHandle, SharedState};
use crate::sink::CommandSink;

mod dnd;
mod keyboards;
mod pointers;
mod state;
mod touches;

pub(crate) use state::WindowState;

#[derive(Debug)]
pub struct Window(Arc<UnownedWindow>);

impl Deref for Window {
    type Target = UnownedWindow;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct UnownedWindow {
    window_id: WindowId,

    pub(crate) gtk_window: gtk4::ApplicationWindow,
    xwindow: Mutex<Option<crate::x11::XWindow>>,

    display_handle: OwnedDisplayHandle,
    window_handle: Mutex<Option<rwh_06::RawWindowHandle>>,

    context: gtk4::glib::MainContext,
    commands: Arc<Mutex<CommandSink>>,
    state: Arc<Mutex<WindowState>>,
}

impl fmt::Debug for UnownedWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnownedWindow").field("window_id", &self.window_id).finish()
    }
}

unsafe impl Send for UnownedWindow {}
unsafe impl Sync for UnownedWindow {}

impl Window {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<Self, RequestError> {
        Ok(Self(UnownedWindow::new(event_loop, attributes)?))
    }
}

impl UnownedWindow {
    fn new(
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<Arc<Self>, RequestError> {
        // Clone the app out of `SharedState` before `present()`, which can
        // synchronously realize the widget and re-enter callbacks that mutate it.
        let app = event_loop.shared.borrow().app.clone();

        let scale_factor = guessed_monitor().map(|monitor| monitor.scale()).unwrap_or(1.0);

        let surface_size = attributes
            .surface_size
            .map(|size| size.to_logical::<u32>(scale_factor))
            .unwrap_or_else(|| LogicalSize::new(800, 600));

        let position =
            attributes.position.map(|position| position.to_physical::<i32>(scale_factor));

        let fullscreen = attributes.fullscreen.is_some();

        let mut builder = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title(attributes.title.as_str())
            .default_width(surface_size.width as i32)
            .default_height(surface_size.height as i32)
            .resizable(attributes.resizable)
            .decorated(attributes.decorations)
            // TODO: Support minimizable/maximizable button hints
            .deletable(attributes.enabled_buttons.contains(WindowButtons::CLOSE))
            .maximized(attributes.maximized && !fullscreen)
            .fullscreened(fullscreen);

        // TODO: support max_surface_size
        if let Some(min_surface_size) = attributes.min_surface_size {
            let (width, height): (i32, i32) =
                min_surface_size.to_logical::<i32>(scale_factor).into();
            builder = builder.width_request(width).height_request(height);
        }

        let initial_cursor = match attributes.cursor {
            Cursor::Icon(cursor_icon) => gdk_cursor_from_icon(cursor_icon),
            // TODO: Support GTK-backed custom cursors
            Cursor::Custom(_) => None,
        };
        if let Some(cursor) = initial_cursor.as_ref() {
            builder = builder.cursor(cursor);
        }

        let gtk_window = builder.build();
        let window_id = WindowId::from_raw(gtk_window.as_ptr() as usize);

        let title = attributes.title;
        let visible = attributes.visible;
        let window_level = attributes.window_level;
        let window_icon = attributes.window_icon;

        let preferred_theme = attributes.preferred_theme;
        let settings = WidgetExt::settings(&gtk_window);
        if preferred_theme.is_some() {
            settings.set_gtk_application_prefer_dark_theme(matches!(
                preferred_theme,
                Some(Theme::Dark)
            ));
        }
        let theme = Some(theme_from_settings(&settings));

        let state = WindowState {
            surface_size,
            last_layout: None,
            last_position: None,
            scale_factor,
            visible,
            has_focus: false,
            modifiers: ModifiersState::default(),
            held_key_press: None,
            theme,
            title,
            window_level,
        };
        let state = Arc::new(Mutex::new(state));

        let commands = event_loop.shared.borrow().commands.clone();
        let window = Arc::new(Self {
            window_id,
            context: event_loop.context.clone(),
            commands,
            gtk_window,
            display_handle: event_loop.display_handle,
            window_handle: Mutex::new(None),
            xwindow: Mutex::new(None),
            state,
        });

        event_loop.shared.borrow_mut().windows.insert(window.id(), Arc::downgrade(&window));

        Self::connect_events(
            event_loop,
            &window.gtk_window,
            window.id(),
            Arc::downgrade(&window),
            position,
        );

        // Realize before `present()` so that X11-only initial state, such as
        // position and window level, can be applied before the window is shown.
        WidgetExt::realize(&window.gtk_window);

        let window_handle = raw_window_handle(&window.gtk_window);
        *window.window_handle.lock().unwrap() = window_handle;

        window.set_window_icon(window_icon.as_ref().and_then(|icon| icon.cast_ref()));

        if visible {
            window.gtk_window.present();
        }

        Ok(window)
    }

    pub(crate) fn id(&self) -> WindowId {
        self.window_id
    }

    fn set_window_icon(&self, icon: Option<&RgbaIcon>) {
        let Some(surface) = self.gtk_window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gtk4::gdk::Toplevel>() else {
            return;
        };

        if let Some(texture) = icon.map(gdk_texture_from_icon) {
            toplevel.set_icon_list(&[texture]);
        } else {
            toplevel.set_icon_list(&[]);
        }
    }

    fn connect_events(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
        window: Weak<UnownedWindow>,
        position: Option<PhysicalPosition<i32>>,
    ) {
        Self::connect_close_request(event_loop, gtk_window, window.clone());
        Self::connect_destroy(event_loop, gtk_window, window_id);
        Self::connect_focus(event_loop, gtk_window, window.clone());
        Self::connect_surface_events(event_loop, gtk_window, window.clone(), position);
        Self::connect_theme(event_loop, gtk_window, window.clone());
        dnd::connect(event_loop, gtk_window, window.clone());
        keyboards::connect(event_loop, gtk_window, window.clone());
        pointers::connect(event_loop, gtk_window, window.clone());
        touches::connect(event_loop, gtk_window, window);
    }

    fn connect_close_request(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window: Weak<UnownedWindow>,
    ) {
        let shared = event_loop.shared.clone();
        gtk_window.connect_close_request(move |_| {
            let Some(window) = window.upgrade() else {
                return gtk4::glib::Propagation::Stop;
            };

            let mut shared = shared.borrow_mut();
            shared.events_sink.push_window_event(WindowEvent::CloseRequested, window.id());

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
        window: Weak<UnownedWindow>,
    ) {
        let shared = event_loop.shared.clone();

        gtk_window.connect_is_active_notify(move |gtk_window| {
            let focused = gtk_window.is_active();
            let modifiers = if focused {
                let modifiers = WidgetExt::display(gtk_window)
                    .default_seat()
                    .and_then(|seat| seat.keyboard())
                    .map(|keyboard| keyboard.modifier_state())
                    .unwrap_or_else(gtk4::gdk::ModifierType::empty);

                keyboards::gdk_mods_to_winit_mods(modifiers)
            } else {
                ModifiersState::empty()
            };

            let Some(window) = window.upgrade() else {
                return;
            };
            let window_id = window.id();

            {
                let mut state = window.state.lock().unwrap();
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

    fn connect_surface_events(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window: Weak<UnownedWindow>,
        position: Option<PhysicalPosition<i32>>,
    ) {
        let window_on_map = window.clone();

        let shared = event_loop.shared.clone();
        let xconn = event_loop.xconn.clone();
        gtk_window.connect_realize(move |gtk_window| {
            let Some(surface) = gtk_window.surface() else {
                return;
            };

            Self::connect_scale_factor(&shared, &surface, window.clone());
            Self::connect_surface_layout(&shared, &surface, window.clone());
            Self::connect_moved(&shared, &surface, window.clone());

            let Some(window) = window.upgrade() else {
                return;
            };

            // Store the X11 window handle for later use.
            let xconn = xconn.clone();
            let xwindow = xconn.and_then(|x| crate::x11::XWindow::from_surface(&surface, x));

            if let Some(xwindow) = &xwindow {
                if let Some(position) = position {
                    xwindow.set_position(position);
                }
            }

            *window.xwindow.lock().unwrap() = xwindow;
        });

        gtk_window.connect_map(move |_| {
            let Some(window) = window_on_map.upgrade() else {
                return;
            };

            let window_level = window.state.lock().unwrap().window_level;
            if let Some(xwindow) = window.xwindow.lock().unwrap().as_ref() {
                xwindow.set_window_level(window_level);
            }
        });
    }

    fn connect_scale_factor(
        shared: &Rc<RefCell<SharedState>>,
        surface: &gtk4::gdk::Surface,
        window: Weak<UnownedWindow>,
    ) {
        let Some(winit_window) = window.upgrade() else {
            return;
        };

        let scale_factor = surface.scale();
        let surface_size = {
            let mut state = winit_window.state.lock().unwrap();
            let logical_size = state.surface_size;

            // Window creation only has a guessed monitor scale. Once the GDK surface exists,
            // seed the state from the actual surface scale before listening for later changes.
            state.scale_factor = scale_factor;

            logical_size.to_physical(scale_factor)
        };

        // Push inital scale factor event
        {
            let mut shared = shared.borrow_mut();
            shared.events_sink.push_scale_factor_changed(
                scale_factor,
                surface_size,
                winit_window.id(),
            );
        }

        let shared = shared.clone();
        surface.connect_scale_notify(move |surface| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let scale_factor = surface.scale();
            let surface_size = {
                let mut state = window.state.lock().unwrap();
                if state.scale_factor == scale_factor {
                    return;
                }

                let logical_size = state.surface_size;
                state.scale_factor = scale_factor;
                logical_size.to_physical(scale_factor)
            };

            let mut shared = shared.borrow_mut();
            shared.events_sink.push_scale_factor_changed(scale_factor, surface_size, window.id());
        });
    }

    fn connect_surface_layout(
        shared: &Rc<RefCell<SharedState>>,
        surface: &gtk4::gdk::Surface,
        window: Weak<UnownedWindow>,
    ) {
        let shared = shared.clone();
        surface.connect_layout(move |_, width, height| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let width = width.max(0) as u32;
            let height = height.max(0) as u32;
            let logical_size = LogicalSize::new(width, height);

            let surface_size = {
                let mut state = window.state.lock().unwrap();
                let surface_size = logical_size.to_physical(state.scale_factor);
                let resized = state
                    .last_layout
                    .map(|last_layout| last_layout != surface_size)
                    .unwrap_or(true);

                if resized {
                    state.surface_size = logical_size;
                    state.last_layout = Some(surface_size);
                    Some(surface_size)
                } else {
                    None
                }
            };

            if let Some(surface_size) = surface_size {
                let event = WindowEvent::SurfaceResized(surface_size);
                let mut shared = shared.borrow_mut();
                shared.events_sink.push_window_event(event, window.id());
            }
        });
    }

    fn connect_moved(
        shared: &Rc<RefCell<SharedState>>,
        surface: &gtk4::gdk::Surface,
        window: Weak<UnownedWindow>,
    ) {
        let Ok(surface) = surface.clone().downcast::<gdk4_x11::X11Surface>() else {
            return;
        };
        let Ok(display) = surface.display().downcast::<gdk4_x11::X11Display>() else {
            return;
        };

        let xwindow = surface.xid();
        let shared = shared.clone();

        unsafe {
            display.connect_xevent(move |_, xevent| {
                let Some(window) = window.upgrade() else {
                    return gtk4::glib::Propagation::Proceed;
                };

                let xevent = &*xevent;
                if xevent.get_type() != gdk4_x11::x11::xlib::ConfigureNotify {
                    return gtk4::glib::Propagation::Proceed;
                }

                let configure = xevent.configure;
                if configure.window != xwindow {
                    return gtk4::glib::Propagation::Proceed;
                }

                let position = PhysicalPosition::new(configure.x, configure.y);
                let moved = {
                    let mut state = window.state.lock().unwrap();
                    let moved = state.last_position.is_some_and(|last| last != position);
                    state.last_position = Some(position);
                    moved
                };

                if moved {
                    let mut shared = shared.borrow_mut();
                    let events_sink = &mut shared.events_sink;
                    events_sink.push_window_event(WindowEvent::Moved(position), window.id());
                }

                gtk4::glib::Propagation::Proceed
            });
        }
    }

    fn connect_theme(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window: Weak<UnownedWindow>,
    ) {
        let shared = event_loop.shared.clone();
        let settings = WidgetExt::settings(gtk_window);

        settings.connect_gtk_application_prefer_dark_theme_notify(move |settings| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let theme = theme_from_settings(settings);
            let changed = {
                let mut state = window.state.lock().unwrap();
                let changed = state.theme != Some(theme);
                state.theme = Some(theme);
                changed
            };

            if changed {
                let mut shared = shared.borrow_mut();
                shared.events_sink.push_window_event(WindowEvent::ThemeChanged(theme), window.id());
            }
        });
    }
}

impl Window {
    fn queue_command(&self, command: WindowCommand) {
        self.commands.lock().unwrap().push_window_command(self.window_id, command);
        self.context.wakeup();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.commands.lock().unwrap().push_close_window(self.gtk_window.clone());
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

    fn set_outer_position(&self, position: Position) {
        let scale_factor = self.state.lock().unwrap().scale_factor;
        self.queue_command(WindowCommand::SetOuterPosition { position, scale_factor });
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        let state = self.state.lock().unwrap();
        state.surface_size.to_physical(state.scale_factor)
    }

    fn request_surface_size(&self, size: Size) -> Option<PhysicalSize<u32>> {
        let scale_factor = self.state.lock().unwrap().scale_factor;
        self.queue_command(WindowCommand::SetSurfaceSize { size, scale_factor });
        None
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

    fn set_window_level(&self, level: WindowLevel) {
        self.state.lock().unwrap().window_level = level;
        self.queue_command(WindowCommand::SetWindowLevel(level));
    }

    fn set_window_icon(&self, window_icon: Option<Icon>) {
        self.queue_command(WindowCommand::SetWindowIcon(window_icon));
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

    fn set_theme(&self, theme: Option<Theme>) {
        let is_dark = matches!(theme, Some(Theme::Dark));
        let effective_theme = if is_dark { Theme::Dark } else { Theme::Light };
        self.state.lock().unwrap().theme = Some(effective_theme);
        self.queue_command(WindowCommand::SetTheme(theme));
    }

    fn theme(&self) -> Option<Theme> {
        self.state.lock().unwrap().theme
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
    RequestRedraw,
    SetSurfaceSize { size: Size, scale_factor: f64 },
    SetOuterPosition { position: Position, scale_factor: f64 },
    SetTheme(Option<Theme>),
    SetTitle(String),
    SetVisible(bool),
    SetWindowLevel(WindowLevel),
    SetWindowIcon(Option<Icon>),
}

impl WindowCommand {
    pub(crate) fn apply_to(self, window: &UnownedWindow) {
        match self {
            WindowCommand::RequestRedraw => { /* Handled in event_loop.rs */ },
            WindowCommand::SetSurfaceSize { size, scale_factor } => {
                let (width, height): (i32, i32) = size.to_logical::<i32>(scale_factor).into();
                window.gtk_window.set_default_size(width, height);
            },
            WindowCommand::SetOuterPosition { position, scale_factor } => {
                if let Some(xwindow) = window.xwindow.lock().unwrap().as_ref() {
                    let position = position.to_physical::<i32>(scale_factor);
                    xwindow.set_position(position);
                }
            },
            WindowCommand::SetTheme(theme) => {
                let is_dark = matches!(theme, Some(Theme::Dark));
                let settings = WidgetExt::settings(&window.gtk_window);
                settings.set_gtk_application_prefer_dark_theme(is_dark)
            },
            WindowCommand::SetTitle(title) => window.gtk_window.set_title(Some(&title)),
            WindowCommand::SetVisible(visible) => window.gtk_window.set_visible(visible),
            WindowCommand::SetWindowLevel(level) => {
                if let Some(xwindow) = window.xwindow.lock().unwrap().as_ref() {
                    xwindow.set_window_level(level);
                }
            },
            WindowCommand::SetWindowIcon(icon) => {
                window.set_window_icon(icon.as_ref().and_then(|icon| icon.cast_ref()));
            },
        }
    }
}

fn gdk_cursor_from_icon(cursor_icon: CursorIcon) -> Option<gtk4::gdk::Cursor> {
    gtk4::gdk::Cursor::from_name(cursor_icon.name(), None).or_else(|| {
        cursor_icon.alt_names().iter().find_map(|name| gtk4::gdk::Cursor::from_name(name, None))
    })
}

fn gdk_texture_from_icon(icon: &RgbaIcon) -> gtk4::gdk::Texture {
    let bytes = gtk4::glib::Bytes::from_owned(icon.buffer().to_vec());
    let stride = icon.width() as usize * 4;
    gtk4::gdk::MemoryTexture::new(
        icon.width() as i32,
        icon.height() as i32,
        gtk4::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        stride,
    )
    .upcast()
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

pub(crate) fn theme_from_settings(settings: &gtk4::Settings) -> Theme {
    let is_dark = settings.is_gtk_application_prefer_dark_theme();
    if is_dark { Theme::Dark } else { Theme::Light }
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = self.window_handle.lock().unwrap().ok_or(rwh_06::HandleError::Unavailable)?;

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

    if let Some(handle) = crate::wayland::raw_window_handle(&surface) {
        return Some(handle);
    }

    crate::x11::raw_window_handle(&surface)
}
