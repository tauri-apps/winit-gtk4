use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use dpi::{LogicalPosition, PhysicalPosition};
use gtk4::prelude::*;
use winit_core::event::{
    ButtonSource, ElementState, FingerId, Force, PointerKind, PointerSource, WindowEvent,
};
use winit_core::window::WindowId;

use super::WindowState;
use super::pointers::device_id;
use crate::event_loop::ActiveEventLoop;

pub(crate) fn connect(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window_id: WindowId,
    window_state: &Arc<Mutex<WindowState>>,
) {
    let controller = gtk4::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let touch_state = Rc::new(RefCell::new(TouchState::default()));
    let shared = event_loop.shared.clone();
    let window_state = window_state.clone();

    controller.connect_event(move |_, event| {
        if let Some(events) = touch_events(event, &window_state, &mut touch_state.borrow_mut()) {
            let mut shared = shared.borrow_mut();
            for event in events {
                shared.events_sink.push_window_event(event, window_id);
            }
        }

        gtk4::glib::Propagation::Proceed
    });

    gtk_window.add_controller(controller);
}

#[derive(Clone, Copy, Debug)]
struct TouchPoint {
    finger_id: FingerId,
    position: PhysicalPosition<f64>,
}

#[derive(Debug, Default)]
struct TouchState {
    active: HashMap<gtk4::gdk::EventSequence, TouchPoint>,
    first_touch: Option<gtk4::gdk::EventSequence>,
    next_finger_id: usize,
}

impl TouchState {
    fn next_finger_id(&mut self) -> FingerId {
        let finger_id = FingerId::from_raw(self.next_finger_id);
        self.next_finger_id = self.next_finger_id.wrapping_add(1);
        finger_id
    }

    fn is_first_touch(&self, sequence: &gtk4::gdk::EventSequence) -> bool {
        self.first_touch.as_ref() == Some(sequence)
    }

    fn reset_first_touch(&mut self) {
        if self.active.is_empty() {
            self.first_touch = None;
        }
    }
}

fn touch_events(
    event: &gtk4::gdk::Event,
    window_state: &Arc<Mutex<WindowState>>,
    touch_state: &mut TouchState,
) -> Option<Vec<WindowEvent>> {
    match event.event_type() {
        gtk4::gdk::EventType::TouchBegin => {
            touch_begin(event, window_state, touch_state).map(|events| events.to_vec())
        },
        gtk4::gdk::EventType::TouchUpdate => {
            touch_update(event, window_state, touch_state).map(|event| vec![event])
        },
        gtk4::gdk::EventType::TouchEnd => {
            touch_end(event, window_state, touch_state).map(|events| events.to_vec())
        },
        gtk4::gdk::EventType::TouchCancel => {
            touch_cancel(event, window_state, touch_state).map(|event| vec![event])
        },
        _ => None,
    }
}

fn touch_begin(
    event: &gtk4::gdk::Event,
    window_state: &Arc<Mutex<WindowState>>,
    touch_state: &mut TouchState,
) -> Option<[WindowEvent; 2]> {
    let sequence = event.event_sequence();

    // Already tracking this touch sequence, ignore
    if touch_state.active.contains_key(&sequence) {
        return None;
    }

    let (x, y) = event.position()?;

    // This is the first finger touch, track it as the primary touch
    if touch_state.active.is_empty() {
        touch_state.first_touch = Some(sequence.clone());
    }

    let finger_id = touch_state.next_finger_id();
    let position = {
        let scale_factor = window_state.lock().unwrap().scale_factor;
        LogicalPosition::new(x, y).to_physical(scale_factor)
    };
    touch_state.active.insert(sequence.clone(), TouchPoint { finger_id, position });

    let primary = touch_state.is_first_touch(&sequence);
    let device_id = event.device().and_then(|device| device_id(&device));
    let force = event.axis(gtk4::gdk::AxisUse::Pressure).map(Force::Normalized);

    Some([
        WindowEvent::PointerEntered {
            device_id,
            primary,
            position,
            kind: PointerKind::Touch(finger_id),
        },
        WindowEvent::PointerButton {
            device_id,
            primary,
            state: ElementState::Pressed,
            position,
            button: ButtonSource::Touch { finger_id, force },
        },
    ])
}

fn touch_update(
    event: &gtk4::gdk::Event,
    window_state: &Arc<Mutex<WindowState>>,
    touch_state: &mut TouchState,
) -> Option<WindowEvent> {
    let sequence = event.event_sequence();
    let primary = touch_state.is_first_touch(&sequence);
    let (x, y) = event.position()?;
    let position = {
        let scale_factor = window_state.lock().unwrap().scale_factor;
        LogicalPosition::new(x, y).to_physical(scale_factor)
    };
    let touch_point = touch_state.active.get_mut(&sequence)?;
    touch_point.position = position;

    Some(WindowEvent::PointerMoved {
        device_id: event.device().and_then(|device| device_id(&device)),
        primary,
        position,
        source: PointerSource::Touch {
            finger_id: touch_point.finger_id,
            force: event.axis(gtk4::gdk::AxisUse::Pressure).map(Force::Normalized),
        },
    })
}

fn touch_end(
    event: &gtk4::gdk::Event,
    window_state: &Arc<Mutex<WindowState>>,
    touch_state: &mut TouchState,
) -> Option<[WindowEvent; 2]> {
    let sequence = event.event_sequence();
    let primary = touch_state.is_first_touch(&sequence);
    let mut touch_point = touch_state.active.remove(&sequence)?;

    if let Some((x, y)) = event.position() {
        let scale_factor = window_state.lock().unwrap().scale_factor;
        let position = LogicalPosition::new(x, y).to_physical(scale_factor);
        touch_point.position = position;
    }
    touch_state.reset_first_touch();

    let device_id = event.device().and_then(|device| device_id(&device));
    let force = event.axis(gtk4::gdk::AxisUse::Pressure).map(Force::Normalized);

    Some([
        WindowEvent::PointerButton {
            device_id,
            primary,
            state: ElementState::Released,
            position: touch_point.position,
            button: ButtonSource::Touch { finger_id: touch_point.finger_id, force },
        },
        WindowEvent::PointerLeft {
            device_id,
            primary,
            position: Some(touch_point.position),
            kind: PointerKind::Touch(touch_point.finger_id),
        },
    ])
}

fn touch_cancel(
    event: &gtk4::gdk::Event,
    window_state: &Arc<Mutex<WindowState>>,
    touch_state: &mut TouchState,
) -> Option<WindowEvent> {
    let sequence = event.event_sequence();
    let primary = touch_state.is_first_touch(&sequence);
    let mut touch_point = touch_state.active.remove(&sequence)?;

    if let Some((x, y)) = event.position() {
        let scale_factor = window_state.lock().unwrap().scale_factor;
        let position = LogicalPosition::new(x, y).to_physical(scale_factor);
        touch_point.position = position;
    }
    touch_state.reset_first_touch();

    Some(WindowEvent::PointerLeft {
        device_id: event.device().and_then(|device| device_id(&device)),
        primary,
        position: Some(touch_point.position),
        kind: PointerKind::Touch(touch_point.finger_id),
    })
}
