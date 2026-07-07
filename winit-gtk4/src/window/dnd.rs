use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use dpi::{LogicalPosition, PhysicalPosition};
use gtk4::gdk::{DragAction, FileList};
use gtk4::glib::prelude::StaticType;
use gtk4::prelude::*;
use winit_core::event::WindowEvent;
use winit_core::window::WindowId;

use super::WindowState;
use crate::event_loop::ActiveEventLoop;

pub(crate) fn connect(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window_id: WindowId,
    window_state: &Arc<Mutex<WindowState>>,
) {
    let target = gtk4::DropTarget::new(FileList::static_type(), DragAction::COPY);
    target.set_preload(true);

    let drag_state = Rc::new(RefCell::new(DragState::default()));

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window_state = window_state.clone();
        target.connect_enter(move |target, x, y| {
            let position = {
                let scale_factor = window_state.lock().unwrap().scale_factor;
                LogicalPosition::new(x, y).to_physical(scale_factor)
            };

            // The file list is loaded asynchronously. Use this serial so a stale
            // callback from an older drag can't enter the drag state and send a DragEntered event.
            let serial = {
                let mut drag_state = drag_state.borrow_mut();
                drag_state.start(position)
            };

            if let Some(drop) = target.current_drop() {
                let shared = shared.clone();
                let drag_state = drag_state.clone();
                drop.read_value_async(
                    FileList::static_type(),
                    gtk4::glib::Priority::DEFAULT,
                    None::<&gtk4::gio::Cancellable>,
                    move |result| {
                        let Ok(value) = result else {
                            return;
                        };
                        let Some(paths) = paths_from_value(&value) else {
                            return;
                        };

                        let mut drag_state = drag_state.borrow_mut();
                        if drag_state.enter(serial) {
                            // Get the updated position in case the drag has moved since connect_enter was called,
                            // falling back to the position from connect_enter if position() returns None
                            // which shouldn't happen but is a safeguard against unwrapping a None value.
                            let position = drag_state.position().unwrap_or(position);

                            let event = WindowEvent::DragEntered { paths, position };
                            shared.borrow_mut().events_sink.push_window_event(event, window_id);
                        }
                    },
                );
            }

            DragAction::COPY
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window_state = window_state.clone();
        target.connect_motion(move |_, x, y| {
            let position = {
                let scale_factor = window_state.lock().unwrap().scale_factor;
                LogicalPosition::new(x, y).to_physical(scale_factor)
            };

            // Store position so it can be later used in DragLeft
            let mut drag_state = drag_state.borrow_mut();
            drag_state.moved(position);

            if drag_state.has_entered() {
                let event = WindowEvent::DragMoved { position };
                shared.borrow_mut().events_sink.push_window_event(event, window_id);
            }

            DragAction::COPY
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        target.connect_leave(move |_| {
            let mut drag_state = drag_state.borrow_mut();

            let position = drag_state.position();
            let has_entered = drag_state.has_entered();

            drag_state.reset();

            if has_entered {
                let event = WindowEvent::DragLeft { position };
                shared.borrow_mut().events_sink.push_window_event(event, window_id);
            }
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window_state = window_state.clone();
        target.connect_drop(move |_, value, x, y| {
            let Some(paths) = paths_from_value(value) else {
                drag_state.borrow_mut().reset();
                return false;
            };

            let position = {
                let scale_factor = window_state.lock().unwrap().scale_factor;
                LogicalPosition::new(x, y).to_physical(scale_factor)
            };

            let mut drag_state = drag_state.borrow_mut();
            let has_entered = drag_state.has_entered();
            drag_state.reset();

            let mut shared = shared.borrow_mut();

            // If the drag has not entered, we need to send a DragEntered event before sending DragDropped.
            if !has_entered {
                let entered_event = WindowEvent::DragEntered { paths: paths.clone(), position };
                shared.events_sink.push_window_event(entered_event, window_id);
            }

            let drop_event = WindowEvent::DragDropped { paths, position };
            shared.events_sink.push_window_event(drop_event, window_id);

            true
        });
    }

    gtk_window.add_controller(target);
}

#[derive(Debug, Default)]
struct DragState {
    active: bool,
    serial: u64,
    position: Option<PhysicalPosition<f64>>,
    has_entered: bool,
}

impl DragState {
    fn position(&self) -> Option<PhysicalPosition<f64>> {
        if !self.active {
            return None;
        }

        self.position
    }

    fn has_entered(&self) -> bool {
        self.has_entered
    }

    fn start(&mut self, position: PhysicalPosition<f64>) -> u64 {
        self.reset();

        self.active = true;
        self.serial = self.serial.wrapping_add(1);
        self.position = Some(position);

        self.serial
    }

    /// Returns true if the drag has entered, false otherwise.
    fn enter(&mut self, serial: u64) -> bool {
        if !self.active || self.serial != serial || self.has_entered {
            return false;
        }

        self.has_entered = true;

        true
    }

    fn moved(&mut self, position: PhysicalPosition<f64>) {
        if !self.active {
            return;
        }

        self.position = Some(position);
    }

    fn reset(&mut self) {
        self.active = false;
        self.position = None;
        self.has_entered = false;
    }
}

fn paths_from_value(value: &gtk4::glib::Value) -> Option<Vec<PathBuf>> {
    let file_list = value.get::<FileList>().ok()?;
    let paths: Vec<_> = file_list.files().into_iter().filter_map(|file| file.path()).collect();

    (!paths.is_empty()).then_some(paths)
}
