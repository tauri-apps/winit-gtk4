use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Weak;

use gtk4::gdk::prelude::DisplayExtManual;
use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use winit_core::event::{ElementState, KeyEvent, Modifiers, WindowEvent};
use winit_core::keyboard::{
    Key, KeyCode, ModifiersState, NamedKey, NativeKey, PhysicalKey, SmolStr,
};

use super::UnownedWindow;
use crate::event_loop::{ActiveEventLoop, SharedState};

pub(crate) fn connect(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window: Weak<UnownedWindow>,
) {
    let controller = gtk4::EventControllerKey::new();

    {
        let shared = event_loop.shared.clone();
        let window = window.clone();
        controller.connect_key_pressed(move |controller, keyval, keycode, modifiers| {
            let Some(window) = window.upgrade() else {
                return gtk4::glib::Propagation::Proceed;
            };

            let event = key_event(controller, &window, keyval, keycode, ElementState::Pressed);
            shared.borrow_mut().events_sink.push_window_event(event, window.id());
            queue_modifiers_event(&shared, &window, keyval, ElementState::Pressed, modifiers);
            gtk4::glib::Propagation::Proceed
        });
    }

    {
        let shared = event_loop.shared.clone();
        let window = window.clone();
        controller.connect_key_released(move |controller, keyval, keycode, modifiers| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let event = key_event(controller, &window, keyval, keycode, ElementState::Released);
            shared.borrow_mut().events_sink.push_window_event(event, window.id());
            queue_modifiers_event(&shared, &window, keyval, ElementState::Released, modifiers);
        });
    }

    {
        let shared = event_loop.shared.clone();
        let window = window.clone();
        controller.connect_modifiers(move |controller, modifiers| {
            let Some(window) = window.upgrade() else {
                return gtk4::glib::Propagation::Proceed;
            };

            let Some(event) = controller.current_event() else {
                return gtk4::glib::Propagation::Proceed;
            };

            let Some(event) = event.downcast::<gtk4::gdk::KeyEvent>().ok() else {
                return gtk4::glib::Propagation::Proceed;
            };

            let state = match event.event_type() {
                gtk4::gdk::EventType::KeyPress => ElementState::Pressed,
                gtk4::gdk::EventType::KeyRelease => ElementState::Released,
                _ => return gtk4::glib::Propagation::Proceed,
            };

            queue_modifiers_event(&shared, &window, event.keyval(), state, modifiers);

            gtk4::glib::Propagation::Proceed
        });
    }

    gtk_window.add_controller(controller);
}

// Maps a GDK key to its corresponding modifier mask, if applicable.
fn modifier_mask(key: gtk4::gdk::Key) -> Option<gtk4::gdk::ModifierType> {
    use gtk4::gdk::{Key, ModifierType};

    match key {
        Key::Shift_L | Key::Shift_R => Some(ModifierType::SHIFT_MASK),
        Key::Control_L | Key::Control_R => Some(ModifierType::CONTROL_MASK),
        Key::Alt_L | Key::Alt_R => Some(ModifierType::ALT_MASK),
        Key::Meta_L | Key::Meta_R => Some(ModifierType::META_MASK),
        Key::Super_L | Key::Super_R => Some(ModifierType::SUPER_MASK),
        Key::Hyper_L | Key::Hyper_R => Some(ModifierType::HYPER_MASK),
        _ => None,
    }
}

fn queue_modifiers_event(
    shared: &Rc<RefCell<SharedState>>,
    window: &UnownedWindow,
    keyval: gtk4::gdk::Key,
    state: ElementState,
    mut modifiers: gtk4::gdk::ModifierType,
) {
    // connect_modifiers, connect_key_pressed, all report modifier state,
    // before this key event, so if current key is a modifier, modifiers
    // state is not updated yet, so we need to update it manually.
    // before sending the winit event.
    if let Some(mask) = modifier_mask(keyval) {
        match state {
            ElementState::Pressed => modifiers.insert(mask),
            ElementState::Released => modifiers.remove(mask),
        }
    }

    let modifiers = gdk_mods_to_winit_mods(modifiers);
    let changed = {
        let mut state = window.state.lock().unwrap();
        let changed = state.modifiers != modifiers;
        state.modifiers = modifiers;
        changed
    };

    if changed {
        let event = WindowEvent::ModifiersChanged(Modifiers::from(modifiers));
        shared.borrow_mut().events_sink.push_window_event(event, window.id());
    }
}

fn key_event(
    controller: &gtk4::EventControllerKey,
    window: &UnownedWindow,
    keyval: gtk4::gdk::Key,
    keycode: u32,
    state: ElementState,
) -> WindowEvent {
    let physical_key = winit_common::xkb::raw_keycode_to_physicalkey(keycode);
    let repeatable = key_repeats(controller, physical_key, keyval);
    let repeat = update_repeat_state(window, physical_key, state, repeatable);
    let logical = logical_key(keyval);
    let text = key_text(keyval, state);
    let key_without_modifiers = key_without_modifiers(controller, keyval, keycode);
    let location = winit_common::xkb::keysym_location(keyval.into_glib());

    WindowEvent::KeyboardInput {
        // Match X11/Wayland: focused window keyboard input is not tied to a Winit device id.
        device_id: None,
        event: KeyEvent {
            physical_key,
            logical_key: logical,
            text: text.clone(),
            location,
            state,
            repeat,
            text_with_all_modifiers: text,
            key_without_modifiers,
        },
        is_synthetic: false,
    }
}

fn update_repeat_state(
    window: &UnownedWindow,
    physical_key: PhysicalKey,
    state: ElementState,
    repeatable: bool,
) -> bool {
    let mut window_state = window.state.lock().unwrap();

    if !repeatable {
        return false;
    }

    let is_latest_held = window_state.held_key_press == Some(physical_key);
    if state == ElementState::Pressed {
        window_state.held_key_press = Some(physical_key);
        is_latest_held
    } else {
        if is_latest_held {
            window_state.held_key_press = None;
        }
        false
    }
}

fn key_repeats(
    controller: &gtk4::EventControllerKey,
    physical_key: PhysicalKey,
    keyval: gtk4::gdk::Key,
) -> bool {
    // TODO:
    // winit X11/Wayland backends use `xkb_keymap_key_repeats`, but GDK doesn't expose the
    // underlying keymap through the backend-neutral GTK API. Full parity needs the real xkb
    // keymap on X11, and Wayland remains blocked by GDK not exposing the keymap fd.
    let event_is_modifier = controller
        .current_event()
        .and_then(|event| event.downcast::<gtk4::gdk::KeyEvent>().ok())
        .map(|event| event.is_modifier())
        .unwrap_or(false);

    !event_is_modifier
        && !is_non_repeating_key(physical_key)
        && !is_non_repeating_named_key(logical_key(keyval))
}

#[inline]
fn is_non_repeating_key(physical_key: PhysicalKey) -> bool {
    matches!(
        physical_key,
        PhysicalKey::Code(
            KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::ControlLeft
                | KeyCode::ControlRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::MetaLeft
                | KeyCode::CapsLock
                | KeyCode::NumLock
        )
    )
}

#[allow(deprecated)]
#[inline]
fn is_non_repeating_named_key(key: Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::Meta
                | NamedKey::CapsLock
                | NamedKey::NumLock
        )
    )
}

fn logical_key(keyval: gtk4::gdk::Key) -> Key {
    let keysym = keyval.into_glib();
    let key = winit_common::xkb::keysym_to_key(keysym);
    if !matches!(key, Key::Unidentified(_)) {
        key
    } else if let Some(ch) = keyval.to_unicode() {
        let ch = SmolStr::new(ch.encode_utf8(&mut [0; 4]));
        Key::Character(ch)
    } else {
        Key::Unidentified(NativeKey::Xkb(keysym))
    }
}

fn key_without_modifiers(
    controller: &gtk4::EventControllerKey,
    keyval: gtk4::gdk::Key,
    keycode: u32,
) -> Key {
    let group = controller
        .current_event()
        .and_then(|event| event.downcast::<gtk4::gdk::KeyEvent>().ok())
        .map(|event| event.layout() as i32)
        .unwrap_or(0);

    let Some(display) = gtk4::gdk::Display::default() else {
        return logical_key(keyval.to_lower());
    };

    display
        .map_keycode(keycode)
        .into_iter()
        .flatten()
        .find(|(key, _)| key.group() == group && key.level() == 0)
        .map(|(_, keyval)| logical_key(keyval))
        .unwrap_or_else(|| logical_key(keyval.to_lower()))
}

fn key_text(keyval: gtk4::gdk::Key, state: ElementState) -> Option<SmolStr> {
    if state == ElementState::Released {
        return None;
    }

    keyval.to_unicode().map(|ch| SmolStr::new(ch.encode_utf8(&mut [0; 4])))
}

pub(crate) fn gdk_mods_to_winit_mods(modifiers: gtk4::gdk::ModifierType) -> ModifiersState {
    let mut state = ModifiersState::empty();

    if modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        state |= ModifiersState::SHIFT;
    }
    if modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        state |= ModifiersState::CONTROL;
    }
    if modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK) {
        state |= ModifiersState::ALT;
    }
    if modifiers.intersects(
        gtk4::gdk::ModifierType::META_MASK
            | gtk4::gdk::ModifierType::SUPER_MASK
            | gtk4::gdk::ModifierType::HYPER_MASK,
    ) {
        state |= ModifiersState::META;
    }

    state
}
