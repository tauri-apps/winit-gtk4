use std::vec::Drain;

use winit_core::event::WindowEvent;
use winit_core::window::WindowId;

use crate::event_loop::Event;
use crate::window::WindowCommand;

#[derive(Debug, Default)]
pub(crate) struct CommandSink {
    commands: Vec<Command>,
}

#[derive(Debug)]
pub(crate) enum Command {
    Window { window_id: WindowId, command: WindowCommand },
}

impl CommandSink {
    #[inline]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub(crate) fn push_window_command(&mut self, window_id: WindowId, command: WindowCommand) {
        self.commands.push(Command::Window { window_id, command });
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    #[inline]
    pub(crate) fn append(&mut self, other: &mut Self) {
        self.commands.append(&mut other.commands);
    }

    #[inline]
    pub(crate) fn drain(&mut self) -> Drain<'_, Command> {
        self.commands.drain(..)
    }
}

#[derive(Debug, Default)]
pub(crate) struct EventSink {
    events: Vec<Event>,
}

impl EventSink {
    #[inline]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub(crate) fn push_window_event(&mut self, event: WindowEvent, window_id: WindowId) {
        self.events.push(Event::Window { window_id, event });
    }

    #[inline]
    pub(crate) fn append(&mut self, other: &mut Self) {
        self.events.append(&mut other.events);
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    #[inline]
    pub(crate) fn drain(&mut self) -> Drain<'_, Event> {
        self.events.drain(..)
    }
}
