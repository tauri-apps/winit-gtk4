use std::cell::RefCell;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};

use dpi::{LogicalSize, PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use gtk4::gdk::prelude::{DeviceExt, DisplayExt, SeatExt, SurfaceExt};
use gtk4::prelude::*;
use winit_core::cursor::{Cursor, CursorIcon};
use winit_core::error::{NotSupportedError, RequestError};
use winit_core::event::WindowEvent;
use winit_core::icon::{Icon, RgbaIcon};
use winit_core::keyboard::{ModifiersState, PhysicalKey};
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
mod touches;

const TRANSPARENT_WINDOW_CSS_CLASS: &str = "winit-transparent-window";
const TRANSPARENT_WINDOW_CSS: &str = r#"
.winit-transparent-window {
    background-color: transparent;
}
"#;

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
    xwindow: Mutex<Option<crate::x11::GtkXWindow>>,

    display_handle: OwnedDisplayHandle,
    window_handle: Mutex<Option<rwh_06::RawWindowHandle>>,

    context: gtk4::glib::MainContext,
    commands: Arc<Mutex<CommandSink>>,
    state: Arc<Mutex<WindowState>>,
}

#[derive(Clone, Copy, Debug)]
struct InitialSurfaceAttributes {
    position: Option<PhysicalPosition<i32>>,
    parent_window: Option<rwh_06::RawWindowHandle>,
}

#[derive(Debug)]
pub(crate) struct WindowState {
    pub(crate) surface_size: LogicalSize<u32>,
    pub(crate) last_layout: Option<PhysicalSize<u32>>,
    pub(crate) last_position: Option<PhysicalPosition<i32>>,
    pub(crate) inner_position_rel_parent: Option<PhysicalPosition<i32>>,
    pub(crate) frame_extents: Option<crate::x11::FrameExtentsHeuristic>,
    pub(crate) scale_factor: f64,
    pub(crate) visible: bool,
    pub(crate) resizable: bool,
    pub(crate) maximized: bool,
    pub(crate) fullscreen: Option<Fullscreen>,
    pub(crate) decorated: bool,
    pub(crate) enabled_buttons: WindowButtons,
    pub(crate) has_focus: bool,
    pub(crate) modifiers: ModifiersState,
    pub(crate) held_key_press: Option<PhysicalKey>,
    pub(crate) theme: Option<Theme>,
    pub(crate) title: String,
    pub(crate) window_level: WindowLevel,
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

    pub(crate) fn gtk_window(&self) -> gtk4::ApplicationWindow {
        self.gtk_window.clone()
    }

    pub(crate) fn gdk_surface(&self) -> Option<gtk4::gdk::Surface> {
        self.gtk_window.surface()
    }
}

impl UnownedWindow {
    fn new(
        event_loop: &ActiveEventLoop,
        mut attributes: WindowAttributes,
    ) -> Result<Arc<Self>, RequestError> {
        let _gtk4_attributes = attributes
            .platform
            .take()
            .and_then(|attrs| attrs.cast::<crate::WindowAttributesGtk4>().ok())
            .unwrap_or_default();

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

        let surface_attributes = InitialSurfaceAttributes {
            position,
            parent_window: attributes.parent_window().copied(),
        };

        let fullscreen = effective_fullscreen(attributes.fullscreen);
        let fullscreened = fullscreen.is_some();
        let maximized = attributes.maximized && !fullscreened;
        let decorated = attributes.decorations;
        let enabled_buttons = attributes.enabled_buttons;

        let mut builder = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title(attributes.title.as_str())
            .default_width(surface_size.width as i32)
            .default_height(surface_size.height as i32)
            .resizable(attributes.resizable)
            .decorated(decorated)
            // GTK only exposes the close button through the native toplevel API.
            // Minimize/maximize hints require taking over the titlebar, so winit-gtk4 leaves them
            // to the compositor/window manager.
            .deletable(enabled_buttons.contains(WindowButtons::CLOSE))
            .maximized(maximized)
            .fullscreened(fullscreened);

        // GTK4/GDK exposes minimum toplevel size, but no maximum-size constraint.
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
        let resizable = attributes.resizable;
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
            inner_position_rel_parent: None,
            frame_extents: None,
            scale_factor,
            visible,
            resizable,
            maximized,
            fullscreen: fullscreen.clone(),
            decorated,
            enabled_buttons,
            has_focus: false,
            modifiers: ModifiersState::default(),
            held_key_press: None,
            theme,
            title,
            window_level,
        };
        let state = Arc::new(Mutex::new(state));

        install_transparency_css(&gtk_window);

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
            surface_attributes,
        );

        // Realize before `present()` so that X11-only initial state, such as
        // position and window level, can be applied before the window is shown.
        WidgetExt::realize(&window.gtk_window);

        let window_handle = raw_window_handle(&window.gtk_window);
        *window.window_handle.lock().unwrap() = window_handle;

        window.set_window_icon(window_icon.as_ref().and_then(|icon| icon.cast_ref()));
        window.set_transparent(attributes.transparent);
        if fullscreen.is_some() {
            window.set_fullscreen(fullscreen.as_ref());
        }

        if visible {
            window.gtk_window.present();
        }

        Ok(window)
    }

    pub(crate) fn id(&self) -> WindowId {
        self.window_id
    }

    pub(crate) fn xwindow(&self) -> std::sync::MutexGuard<'_, Option<crate::x11::GtkXWindow>> {
        self.xwindow.lock().unwrap()
    }

    fn connect_events(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window_id: WindowId,
        window: Weak<UnownedWindow>,
        surface_attributes: InitialSurfaceAttributes,
    ) {
        Self::connect_close_request(event_loop, gtk_window, window.clone());
        Self::connect_destroy(event_loop, gtk_window, window_id);
        Self::connect_focus(event_loop, gtk_window, window.clone());
        Self::connect_surface_events(event_loop, gtk_window, window.clone(), surface_attributes);
        Self::connect_maximized(gtk_window, window.clone());
        Self::connect_fullscreen(gtk_window, window.clone());
        Self::connect_decorated(gtk_window, window.clone());
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

    fn connect_maximized(gtk_window: &gtk4::ApplicationWindow, window: Weak<UnownedWindow>) {
        gtk_window.connect_maximized_notify(move |gtk_window| {
            let Some(window) = window.upgrade() else {
                return;
            };

            window.state.lock().unwrap().maximized = gtk_window.is_maximized();
        });
    }

    fn connect_fullscreen(gtk_window: &gtk4::ApplicationWindow, window: Weak<UnownedWindow>) {
        gtk_window.connect_fullscreened_notify(move |gtk_window| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let mut state = window.state.lock().unwrap();
            if gtk_window.is_fullscreen() {
                // GTK reports fullscreen as a boolean, so preserve any monitor-specific
                // state cached from a winit request.
                if state.fullscreen.is_none() {
                    state.fullscreen = Some(Fullscreen::Borderless(None));
                }
            } else {
                state.fullscreen = None;
            }
        });
    }

    fn connect_decorated(gtk_window: &gtk4::ApplicationWindow, window: Weak<UnownedWindow>) {
        gtk_window.connect_decorated_notify(move |gtk_window| {
            let Some(window) = window.upgrade() else {
                return;
            };

            window.state.lock().unwrap().decorated = gtk_window.is_decorated();
        });
    }

    fn connect_surface_events(
        event_loop: &ActiveEventLoop,
        gtk_window: &gtk4::ApplicationWindow,
        window: Weak<UnownedWindow>,
        surface_attributes: InitialSurfaceAttributes,
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
            let xwindow = xconn.and_then(|x| crate::x11::GtkXWindow::from_surface(&surface, x));

            if let Some(xwindow) = &xwindow {
                let parent = surface_attributes.parent_window.and_then(crate::x11::parent_window);
                if let Some(parent) = parent {
                    xwindow.set_parent(parent, surface_attributes.position.unwrap_or_default());
                }

                if let Some(position) = surface_attributes.position {
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

                let configure_position = PhysicalPosition::new(configure.x, configure.y);
                // `XSendEvent` (synthetic `ConfigureNotify`) -> position relative to root.
                // `XConfigureNotify` (real `ConfigureNotify`) -> position relative to parent.
                let is_synthetic = configure.send_event != 0;
                if !is_synthetic {
                    window.update_frame_extents_if_changed(configure_position);
                    return gtk4::glib::Propagation::Proceed;
                }

                let position = window
                    .frame_extents()
                    .map(|frame_extents| {
                        let (x, y) = configure_position.into();
                        let (x, y) = frame_extents.inner_pos_to_outer(x, y);
                        PhysicalPosition::new(x, y)
                    })
                    .unwrap_or(configure_position);
                let moved = {
                    let mut state = window.state.lock().unwrap();
                    let moved = state.last_position != Some(position);
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

    fn set_fullscreen(&self, fullscreen: Option<&Fullscreen>) {
        match fullscreen {
            Some(Fullscreen::Borderless(Some(monitor)) | Fullscreen::Exclusive(monitor, _)) => {
                if let Some(monitor) = crate::monitor::gdk_monitor(monitor) {
                    self.gtk_window.fullscreen_on_monitor(&monitor);
                } else {
                    self.gtk_window.fullscreen();
                }
            },
            Some(Fullscreen::Borderless(None)) => self.gtk_window.fullscreen(),
            None => self.gtk_window.unfullscreen(),
        }
    }

    fn set_transparent(&self, transparent: bool) {
        if transparent {
            self.gtk_window.add_css_class(TRANSPARENT_WINDOW_CSS_CLASS);
        } else {
            self.gtk_window.remove_css_class(TRANSPARENT_WINDOW_CSS_CLASS);
        }
    }

    fn inner_position(&self) -> Option<PhysicalPosition<i32>> {
        self.xwindow().as_ref().and_then(|xwindow| xwindow.inner_position())
    }

    fn outer_position(&self) -> Option<PhysicalPosition<i32>> {
        let inner_position = self.inner_position()?;
        let frame_extents = self.frame_extents()?;
        Some(frame_extents.inner_pos_to_outer(inner_position.x, inner_position.y).into())
    }

    fn update_cached_frame_extents(&self) -> Option<crate::x11::FrameExtentsHeuristic> {
        let frame_extents = self.xwindow().as_ref().map(|xwindow| xwindow.frame_extents())?;
        self.state.lock().unwrap().frame_extents = Some(frame_extents.clone());
        Some(frame_extents)
    }

    fn invalidate_cached_frame_extents(&self) {
        self.state.lock().unwrap().frame_extents = None;
    }

    fn update_frame_extents_if_changed(&self, inner_position_rel_parent: PhysicalPosition<i32>) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = state
                .inner_position_rel_parent
                .map(|last| last != inner_position_rel_parent)
                .unwrap_or(true);
            state.inner_position_rel_parent = Some(inner_position_rel_parent);
            changed
        };

        if changed {
            self.invalidate_cached_frame_extents();
        }
    }

    fn frame_extents(&self) -> Option<crate::x11::FrameExtentsHeuristic> {
        if let Some(frame_extents) = self.state.lock().unwrap().frame_extents.clone() {
            Some(frame_extents)
        } else {
            self.update_cached_frame_extents()
        }
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
        self.frame_extents()
            .map(|frame_extents| frame_extents.surface_position())
            .unwrap_or((0, 0))
            .into()
    }

    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        if let Some(position) = self.0.outer_position() {
            return Ok(position);
        }

        if let Some(position) = self.state.lock().unwrap().last_position {
            return Ok(position);
        }

        Err(NotSupportedError::new("window position information is not available").into())
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
        let surface_size = self.surface_size();
        self.frame_extents()
            .map(|frame_extents| {
                frame_extents.surface_size_to_outer(surface_size.width, surface_size.height).into()
            })
            .unwrap_or(surface_size)
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        PhysicalInsets::new(0, 0, 0, 0)
    }

    fn set_min_surface_size(&self, min_size: Option<Size>) {
        let scale_factor = self.state.lock().unwrap().scale_factor;
        self.queue_command(WindowCommand::SetMinSurfaceSize { min_size, scale_factor });
    }

    fn set_max_surface_size(&self, _max_size: Option<Size>) {
        // GTK4/GDK does not expose a native maximum toplevel size constraint.
    }

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        None
    }

    fn set_surface_resize_increments(&self, _increments: Option<Size>) {
        // GTK4/GDK does not expose a native toplevel geometry-increment hint.
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        self.state.lock().unwrap().title = title.clone();
        self.queue_command(WindowCommand::SetTitle(title));
    }

    fn set_transparent(&self, transparent: bool) {
        self.queue_command(WindowCommand::SetTransparent(transparent));
    }

    fn set_blur(&self, _blur: bool) {}

    fn set_visible(&self, visible: bool) {
        self.state.lock().unwrap().visible = visible;
        self.queue_command(WindowCommand::SetVisible(visible));
    }

    fn is_visible(&self) -> Option<bool> {
        Some(self.state.lock().unwrap().visible)
    }

    fn set_resizable(&self, resizable: bool) {
        self.state.lock().unwrap().resizable = resizable;
        self.queue_command(WindowCommand::SetResizable(resizable));
    }

    fn is_resizable(&self) -> bool {
        self.state.lock().unwrap().resizable
    }

    fn set_enabled_buttons(&self, buttons: WindowButtons) {
        self.state.lock().unwrap().enabled_buttons = buttons;
        self.queue_command(WindowCommand::SetEnabledButtons(buttons));
    }

    fn enabled_buttons(&self) -> WindowButtons {
        self.state.lock().unwrap().enabled_buttons
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
        self.state.lock().unwrap().maximized
    }

    fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
        let fullscreen = effective_fullscreen(fullscreen);
        self.state.lock().unwrap().fullscreen = fullscreen.clone();
        self.queue_command(WindowCommand::SetFullscreen(fullscreen));
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        self.state.lock().unwrap().fullscreen.clone()
    }

    fn set_decorations(&self, decorations: bool) {
        self.state.lock().unwrap().decorated = decorations;
        self.queue_command(WindowCommand::SetDecorated(decorations));
    }

    fn is_decorated(&self) -> bool {
        self.state.lock().unwrap().decorated
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

    fn set_content_protected(&self, _protected: bool) {}

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
        self.gtk_window.surface().and_then(|surface| crate::monitor::current_monitor(&surface))
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = MonitorHandle>> {
        crate::monitor::available_monitors()
    }

    fn primary_monitor(&self) -> Option<MonitorHandle> {
        crate::monitor::primary_monitor()
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
    SetMinSurfaceSize { min_size: Option<Size>, scale_factor: f64 },
    SetOuterPosition { position: Position, scale_factor: f64 },
    SetTheme(Option<Theme>),
    SetTitle(String),
    SetTransparent(bool),
    SetVisible(bool),
    SetResizable(bool),
    SetEnabledButtons(WindowButtons),
    SetFullscreen(Option<Fullscreen>),
    SetDecorated(bool),
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
            WindowCommand::SetMinSurfaceSize { min_size, scale_factor } => {
                let (width, height) = min_size
                    .map(|size| size.to_logical::<i32>(scale_factor).into())
                    .unwrap_or((-1, -1));
                window.gtk_window.set_size_request(width, height);
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
            WindowCommand::SetTransparent(transparent) => window.set_transparent(transparent),
            WindowCommand::SetVisible(visible) => window.gtk_window.set_visible(visible),
            WindowCommand::SetResizable(resizable) => window.gtk_window.set_resizable(resizable),
            WindowCommand::SetEnabledButtons(buttons) => {
                // GTK4 only exposes the close button through the native toplevel API.
                window.gtk_window.set_deletable(buttons.contains(WindowButtons::CLOSE));
            },
            WindowCommand::SetFullscreen(fullscreen) => window.set_fullscreen(fullscreen.as_ref()),
            WindowCommand::SetDecorated(decorated) => window.gtk_window.set_decorated(decorated),
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

fn install_transparency_css(window: &gtk4::ApplicationWindow) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(TRANSPARENT_WINDOW_CSS);
    gtk4::style_context_add_provider_for_display(
        &WidgetExt::display(window),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// GTK has no exclusive video mode fullscreen, so we map it to borderless fullscreen.
fn effective_fullscreen(fullscreen: Option<Fullscreen>) -> Option<Fullscreen> {
    match fullscreen {
        Some(Fullscreen::Exclusive(monitor, _)) => Some(Fullscreen::Borderless(Some(monitor))),
        fullscreen => fullscreen,
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
