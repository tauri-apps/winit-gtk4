//! The input method.
//!
//! GTK composes text through an [`IMContext`](gtk4::IMContext), as `GtkText` does: the context
//! takes each key first and reports what it composes through its `preedit-changed` and `commit`
//! signals, which become [`WindowEvent::Ime`]. The module GTK picks, as it does for its own
//! widgets, decides who composes: GTK's own module composes dead keys and the Compose key, and
//! its Wayland module leaves them, since GTK 4.20, to the compositor's input method, such as IBus
//! or Fcitx, over the text-input protocol.

use std::cell::RefCell;
use std::sync::Weak;

use gtk4::prelude::*;
use winit_core::event::{Ime, KeyEvent, WindowEvent};
use winit_core::window::{ImeCapabilities, ImeRequest, ImeRequestData, ImeRequestError};

use super::{UnownedWindow, Window, WindowCommand};
use crate::event_loop::{ActiveEventLoop, SharedState};

/// The input method's part of the window's state.
#[derive(Debug, Default)]
pub(super) struct ImeState {
    /// What the application enabled the input method with; `None` while it's disabled.
    capabilities: Option<ImeCapabilities>,
    /// What the context reported while it took a key, which [`key`] pairs with the key.
    filtered: Option<Vec<Ime>>,
    preedit: Preedit,
}

/// The preedit as the application last heard of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Preedit {
    #[default]
    None,
    Shown(Ime),
    /// Just cleared, which a commit right after needn't clear again.
    Cleared,
}

impl ImeState {
    /// The events that tell the application of `ime`, ordered as winit's Wayland backend orders
    /// them: an empty preedit before each commit, and one only clears a preedit it was shown. GTK
    /// signals some preedits twice, which the application hears of once.
    fn events(&mut self, ime: Ime) -> Vec<Ime> {
        let clear = Ime::Preedit(String::new(), None);
        match &ime {
            Ime::Preedit(text, _) if text.is_empty() => {
                if !matches!(self.preedit, Preedit::Shown(_)) {
                    return Vec::new();
                }
                self.preedit = Preedit::Cleared;
                vec![clear]
            },
            Ime::Preedit(..) => {
                if matches!(&self.preedit, Preedit::Shown(shown) if *shown == ime) {
                    return Vec::new();
                }
                self.preedit = Preedit::Shown(ime.clone());
                vec![ime]
            },
            Ime::Commit(_) => {
                if std::mem::take(&mut self.preedit) == Preedit::Cleared {
                    vec![ime]
                } else {
                    vec![clear, ime]
                }
            },
            _ => vec![ime],
        }
    }
}

/// Makes the window's input method context. The key handlers own it, so it goes with the GTK
/// window; the window keeps a weak reference for the application's requests.
pub(super) fn connect(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window: &Weak<UnownedWindow>,
) -> gtk4::IMMulticontext {
    let context = gtk4::IMMulticontext::new();
    context.set_client_widget(Some(gtk_window));

    {
        let shared = event_loop.shared.clone();
        let window = window.clone();
        context.connect_commit(move |_, text| {
            if let Some(window) = window.upgrade() {
                report(&shared, &window, Ime::Commit(text.to_owned()));
            }
        });
    }

    {
        let shared = event_loop.shared.clone();
        let window = window.clone();
        context.connect_preedit_changed(move |context| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let (text, _, cursor) = context.preedit_string();
            // GTK counts the cursor in characters, winit in bytes.
            let cursor =
                text.char_indices().nth(cursor as usize).map_or(text.len(), |(byte, _)| byte);
            report(&shared, &window, Ime::Preedit(text.into(), Some((cursor, cursor))));
        });
    }

    if let Some(window) = window.upgrade() {
        window.ime.set(Some(&context));
    }
    context
}

/// Tells the application of `ime`, or holds it for the key the context is taking.
fn report(shared: &RefCell<SharedState>, window: &UnownedWindow, ime: Ime) {
    let events = {
        let mut state = window.state.lock().unwrap();
        if let Some(filtered) = state.ime.filtered.as_mut() {
            filtered.push(ime);
            return;
        }
        state.ime.events(ime)
    };

    let mut shared = shared.borrow_mut();
    for ime in events {
        shared.events_sink.push_window_event(WindowEvent::Ime(ime), window.id());
    }
}

/// Reports a key press or release, which the input method takes first while it's enabled.
///
/// GTK's input methods commit each printable key themselves, as `GtkText` gets its text, so a key
/// whose commit is only its own text, with nothing composing, stays an ordinary key. A key the
/// input method composes with is the input method's alone, as winit's X11 backend drops the keys
/// XIM filters.
pub(super) fn key(
    shared: &RefCell<SharedState>,
    window: &UnownedWindow,
    context: &gtk4::IMMulticontext,
    controller: &gtk4::EventControllerKey,
    key: KeyEvent,
) {
    let push = |event| shared.borrow_mut().events_sink.push_window_event(event, window.id());
    let keyboard_input = |event| WindowEvent::KeyboardInput {
        // Match X11/Wayland: focused window keyboard input is not tied to a Winit device id.
        device_id: None,
        event,
        is_synthetic: false,
    };

    let composing = controller.current_event().and_then(|event| {
        let mut state = window.state.lock().unwrap();
        let ime = &mut state.ime;
        ime.capabilities?;
        ime.filtered = Some(Vec::new());
        Some((event, matches!(ime.preedit, Preedit::Shown(_))))
    });
    let Some((event, composing)) = composing else {
        return push(keyboard_input(key));
    };

    let taken = context.filter_keypress(&event);
    let filtered = window.state.lock().unwrap().ime.filtered.take().unwrap_or_default();
    let own_text = taken
        && !composing
        && matches!(
            filtered.as_slice(),
            [Ime::Commit(text)] if key.text.as_deref() == Some(text.as_str())
        );

    if !taken || own_text {
        push(keyboard_input(key));
    }
    if !own_text {
        for ime in filtered {
            report(shared, window, ime);
        }
    }
}

/// Takes the application's request, which the window's commands carry out on GTK's thread.
pub(super) fn request(window: &Window, request: ImeRequest) -> Result<(), ImeRequestError> {
    let command = {
        let mut state = window.state.lock().unwrap();
        let capabilities = &mut state.ime.capabilities;
        match request {
            ImeRequest::Enable(_) if capabilities.is_some() => {
                return Err(ImeRequestError::AlreadyEnabled);
            },
            ImeRequest::Enable(enable) => {
                let (enabled, data) = enable.into_raw();
                *capabilities = Some(enabled);
                WindowCommand::EnableIme(data)
            },
            ImeRequest::Update(_) if capabilities.is_none() => {
                return Err(ImeRequestError::NotEnabled);
            },
            ImeRequest::Update(data) => WindowCommand::UpdateIme(data),
            ImeRequest::Disable => {
                if capabilities.take().is_none() {
                    return Ok(());
                }
                WindowCommand::DisableIme
            },
            _ => return Err(ImeRequestError::NotSupported),
        }
    };
    window.queue_command(command);
    Ok(())
}

pub(super) fn capabilities(window: &UnownedWindow) -> Option<ImeCapabilities> {
    window.state.lock().unwrap().ime.capabilities
}

/// Starts composing for the application, while the window has the focus.
pub(super) fn enable(shared: &RefCell<SharedState>, window: &UnownedWindow, data: ImeRequestData) {
    shared.borrow_mut().events_sink.push_window_event(WindowEvent::Ime(Ime::Enabled), window.id());
    let Some(context) = window.ime.upgrade() else {
        return;
    };
    if window.state.lock().unwrap().has_focus {
        context.focus_in();
    }
    update(window, data);
}

/// Places the candidate window by the cursor area, the only data the context takes.
pub(super) fn update(window: &UnownedWindow, data: ImeRequestData) {
    let Some(context) = window.ime.upgrade() else {
        return;
    };
    let (capabilities, scale_factor) = {
        let state = window.state.lock().unwrap();
        (state.ime.capabilities, state.scale_factor)
    };
    let Some((position, size)) =
        data.cursor_area.filter(|_| capabilities.is_some_and(|enabled| enabled.cursor_area()))
    else {
        return;
    };

    // The context takes the area in the GTK window's coordinates, the ones pointer events arrive
    // in.
    let position = position.to_logical::<i32>(scale_factor);
    let size = size.to_logical::<i32>(scale_factor);
    context.set_cursor_location(&gtk4::gdk::Rectangle::new(
        position.x,
        position.y,
        size.width,
        size.height,
    ));
}

/// Stops composing, dropping whatever the context was composing.
pub(super) fn disable(shared: &RefCell<SharedState>, window: &UnownedWindow) {
    if let Some(context) = window.ime.upgrade() {
        context.focus_out();
        context.reset();
    }
    window.state.lock().unwrap().ime.preedit = Preedit::None;
    shared.borrow_mut().events_sink.push_window_event(WindowEvent::Ime(Ime::Disabled), window.id());
}

/// Drops what the context was composing, a dead key's accent among it.
pub(super) fn reset(window: &UnownedWindow) {
    if let Some(context) = window.ime.upgrade() {
        context.reset();
    }
}

/// Moves the context's focus with the window's, while the input method is enabled.
pub(super) fn focus(window: &UnownedWindow, focused: bool) {
    if capabilities(window).is_none() {
        return;
    }
    let Some(context) = window.ime.upgrade() else {
        return;
    };
    if focused {
        context.focus_in();
    } else {
        context.focus_out();
    }
}
