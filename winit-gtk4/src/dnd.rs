use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::ops::{BitOr, ControlFlow};
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::{fmt, io};

use dpi::{LogicalPosition, PhysicalPosition};
use gtk4::glib::prelude::StaticType;
use gtk4::glib::value::ToValue;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, gio, glib};
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::event::WindowEvent;
use winit_core::event_loop::DndAction;
use winit_core::window::WindowId;

use crate::event_loop::ActiveEventLoop;
use crate::window::UnownedWindow;

#[derive(Debug, Default)]
pub(crate) struct DndState {
    next_transfer_id: i64,
    receive_drag: Option<DataOffer>,
    send_drag: Option<DragSource>,
}

impl DndState {
    pub(crate) fn next_data_transfer_id(&mut self) -> DataTransferId {
        let id = DataTransferId::from_raw(self.next_transfer_id);
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        id
    }

    pub(crate) fn set_receive_drag(&mut self, offer: DataOffer) {
        self.receive_drag = Some(offer);
    }

    pub(crate) fn receive_drag(&self) -> Option<&DataOffer> {
        self.receive_drag.as_ref()
    }

    pub(crate) fn receive_drag_mut(&mut self) -> Option<&mut DataOffer> {
        self.receive_drag.as_mut()
    }

    pub(crate) fn clear_receive_drag(&mut self, id: DataTransferId) {
        if self.receive_drag.as_ref().is_some_and(|offer| offer.transfer_id() == id) {
            self.receive_drag = None;
        }
    }

    pub(crate) fn set_send_drag(&mut self, source: DragSource) {
        self.send_drag = Some(source);
    }

    pub(crate) fn clear_send_drag(&mut self, id: DataTransferId) -> bool {
        if self.send_drag.as_ref().is_some_and(|send| send.data_transfer_id == id) {
            self.send_drag = None;
            true
        } else {
            false
        }
    }
}

pub(crate) fn connect_ingoing_drag(
    event_loop: &ActiveEventLoop,
    gtk_window: &gtk4::ApplicationWindow,
    window: Weak<UnownedWindow>,
) {
    let target = gtk4::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY
            | gdk::DragAction::MOVE
            | gdk::DragAction::LINK
            | gdk::DragAction::ASK,
    );
    target.set_preload(true);

    let drag_state = Rc::new(RefCell::new(WindowDragState::default()));

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window = window.clone();
        target.connect_enter(move |target, x, y| {
            let Some(window) = window.upgrade() else {
                return gdk::DragAction::empty();
            };

            let position = LogicalPosition::new(x, y).to_physical(window.surface_scale());
            let window_id = window.id();

            // The file list is loaded asynchronously. Use this serial so a stale
            // callback from an older drag can't enter the drag state and send a DragEntered event.
            let serial = {
                let mut drag_state = drag_state.borrow_mut();
                drag_state.start(position)
            };

            if let Some(drop) = target.current_drop() {
                let actions = drop.actions();
                let shared = shared.clone();
                let drag_state = drag_state.clone();
                drop.read_value_async(
                    gdk::FileList::static_type(),
                    gtk4::glib::Priority::DEFAULT,
                    None::<&gtk4::gio::Cancellable>,
                    move |result| {
                        let Ok(value) = result else {
                            drag_state.borrow_mut().ignore(serial);
                            return;
                        };
                        let Some(paths) = paths_from_value(&value) else {
                            drag_state.borrow_mut().ignore(serial);
                            return;
                        };

                        let mut drag_state = drag_state.borrow_mut();
                        if drag_state.is_current_pending_enter(serial) {
                            // Get the updated position in case the drag has moved since
                            // connect_enter was called, falling back to the position from
                            // connect_enter if position() returns None.
                            let position = drag_state.position().unwrap_or(position);

                            let mut shared = shared.borrow_mut();

                            let id = shared.dnd.next_data_transfer_id();
                            let offer = DataOffer {
                                data_transfer_id: id,
                                window_id,
                                type_: TypeHint::UriList,
                                paths: paths.into(),
                                source_actions: actions,
                                valid_actions: Vec::new(),
                            };
                            shared.dnd.set_receive_drag(offer);
                            drag_state.set_transfer_id(id);

                            let event = WindowEvent::DragEntered { id, position: Some(position) };
                            shared.events_sink.push_window_event(event, window_id);
                        }
                    },
                );
            }

            gdk::DragAction::empty()
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window = window.clone();
        target.connect_motion(move |_, x, y| {
            let Some(window) = window.upgrade() else {
                return gdk::DragAction::empty();
            };

            let position = LogicalPosition::new(x, y).to_physical(window.surface_scale());

            let mut drag_state = drag_state.borrow_mut();
            drag_state.moved(position);

            if let Some(id) = drag_state.transfer_id() {
                let (proposed_action, selected_gdk_action) = {
                    let shared = shared.borrow();
                    match shared.dnd.receive_drag().filter(|offer| offer.transfer_id() == id) {
                        Some(offer) => (offer.proposed_action(), offer.selected_gdk_action()),
                        None => (None, None),
                    }
                };

                let event = WindowEvent::DragPosition { id, position, proposed_action };
                shared.borrow_mut().events_sink.push_window_event(event, window.id());

                return selected_gdk_action.unwrap_or_else(gdk::DragAction::empty);
            }

            gdk::DragAction::empty()
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        let window = window.clone();
        target.connect_leave(move |_| {
            let Some(window) = window.upgrade() else {
                return;
            };

            let mut drag_state = drag_state.borrow_mut();

            let id = drag_state.transfer_id();

            drag_state.reset();

            if let Some(id) = id {
                let mut shared = shared.borrow_mut();
                shared.dnd.clear_receive_drag(id);
                shared.events_sink.push_window_event(WindowEvent::DragLeft { id }, window.id());
            }
        });
    }

    {
        let shared = event_loop.shared.clone();
        let drag_state = drag_state.clone();
        target.connect_drop(move |target, value, x, y| {
            let Some(window) = window.upgrade() else {
                return false;
            };
            let window_id = window.id();

            let Some(paths) = paths_from_value(value) else {
                let id = {
                    let mut drag_state = drag_state.borrow_mut();
                    let id = drag_state.transfer_id();
                    drag_state.reset();
                    id
                };

                if let Some(id) = id {
                    shared.borrow_mut().dnd.clear_receive_drag(id);
                }

                return false;
            };

            let position = LogicalPosition::new(x, y).to_physical(window.surface_scale());

            let mut drag_state = drag_state.borrow_mut();
            let has_entered = drag_state.transfer_id().is_some();
            let id = match drag_state.transfer_id() {
                Some(id) => id,
                None => {
                    // The enter event hasn't had a chance to fire yet, so we need to create the
                    // transfer here.
                    let source_actions = target
                        .current_drop()
                        .map(|drop| drop.actions())
                        .unwrap_or_else(gdk::DragAction::empty);

                    let mut shared = shared.borrow_mut();

                    let id = shared.dnd.next_data_transfer_id();
                    let offer = DataOffer {
                        data_transfer_id: id,
                        window_id,
                        type_: TypeHint::UriList,
                        paths: paths.into(),
                        source_actions,
                        valid_actions: Vec::new(),
                    };
                    shared.dnd.set_receive_drag(offer);

                    drag_state.set_transfer_id(id);

                    id
                },
            };

            drag_state.reset();

            let mut shared = shared.borrow_mut();

            if !has_entered {
                let entered_event = WindowEvent::DragEntered { id, position: Some(position) };
                shared.events_sink.push_window_event(entered_event, window_id);
            }

            let proposed_action = shared
                .dnd
                .receive_drag()
                .filter(|offer| offer.transfer_id() == id)
                .and_then(DataOffer::proposed_action);

            match proposed_action {
                proposed_action @ Some(_) => {
                    let drop_event = WindowEvent::DragDropped { id, proposed_action };
                    shared.events_sink.push_window_event(drop_event, window_id);
                    true
                },
                None => {
                    shared.dnd.clear_receive_drag(id);
                    shared.events_sink.push_window_event(WindowEvent::DragLeft { id }, window_id);
                    false
                },
            }
        });
    }

    gtk_window.add_controller(target);
}

fn paths_from_value(value: &gtk4::glib::Value) -> Option<Vec<PathBuf>> {
    let file_list = value.get::<gdk::FileList>().ok()?;
    let paths: Vec<_> = file_list.files().into_iter().filter_map(|file| file.path()).collect();

    (!paths.is_empty()).then_some(paths)
}

#[derive(Debug, Default)]
struct WindowDragState {
    active: bool,
    serial: u64,
    position: Option<PhysicalPosition<f64>>,
    transfer_id: Option<DataTransferId>,
}

impl WindowDragState {
    fn position(&self) -> Option<PhysicalPosition<f64>> {
        if !self.active {
            return None;
        }

        self.position
    }

    fn transfer_id(&self) -> Option<DataTransferId> {
        self.transfer_id
    }

    fn set_transfer_id(&mut self, id: DataTransferId) {
        self.transfer_id = Some(id);
    }

    fn start(&mut self, position: PhysicalPosition<f64>) -> u64 {
        self.reset();

        self.active = true;
        self.serial = self.serial.wrapping_add(1);
        self.position = Some(position);

        self.serial
    }

    fn is_current_pending_enter(&self, serial: u64) -> bool {
        self.active && self.serial == serial && self.transfer_id.is_none()
    }

    fn moved(&mut self, position: PhysicalPosition<f64>) {
        if !self.active {
            return;
        }

        self.position = Some(position);
    }

    fn ignore(&mut self, serial: u64) {
        if self.active && self.serial == serial && self.transfer_id.is_none() {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.active = false;
        self.position = None;
        self.transfer_id = None;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DataOffer {
    data_transfer_id: DataTransferId,
    window_id: WindowId,
    type_: TypeHint,
    paths: Arc<[PathBuf]>,
    source_actions: gdk::DragAction,
    valid_actions: Vec<DndAction>,
}

impl DataOffer {
    pub(crate) fn transfer_id(&self) -> DataTransferId {
        self.data_transfer_id
    }

    pub(crate) fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub(crate) fn set_actions(&mut self, action_set: &[DndAction]) {
        self.valid_actions.clear();
        self.valid_actions.extend_from_slice(action_set);
    }

    pub(crate) fn proposed_action(&self) -> Option<DndAction> {
        self.selected_gdk_action().and_then(dnd_action_gdk_to_winit)
    }

    pub(crate) fn selected_gdk_action(&self) -> Option<gdk::DragAction> {
        self.valid_actions.iter().copied().find_map(|action| {
            let action = dnd_action_winit_to_gdk(action);
            self.source_actions.intersects(action).then_some(action)
        })
    }

    pub(crate) fn typed_data(&self, type_: &dyn TransferType) -> Option<TypedFileData> {
        let type_ = *self.find_type_dyn(type_)?;
        Some(TypedFileData { type_, paths: self.paths.clone() })
    }

    pub(crate) fn find_type_dyn<'a>(&'a self, type_: &'a dyn TransferType) -> Option<&'a TypeHint> {
        <TypeHint as TransferType>::matches(&self.type_, type_).then_some(&self.type_)
    }
}

impl DataTransfer for DataOffer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> ControlFlow<()>,
    ) {
        let _ = func(&self.type_);
    }
}

pub(crate) fn connect_outgoing_drag(
    event_loop: &ActiveEventLoop,
    drag: &gdk::Drag,
    id: DataTransferId,
    window_id: WindowId,
) {
    {
        let shared = event_loop.shared.clone();
        let context = event_loop.context.clone();
        drag.connect_drop_performed(move |drag| {
            let mut shared = shared.borrow_mut();

            let action = dnd_action_gdk_to_winit(drag.selected_action());
            let event = WindowEvent::OutgoingDragDropped { id, action };
            shared.events_sink.push_window_event(event, window_id);
            context.wakeup();
        });
    }

    {
        let shared = event_loop.shared.clone();
        let context = event_loop.context.clone();
        drag.connect_cancel(move |_, _| {
            let mut shared = shared.borrow_mut();

            let event = WindowEvent::OutgoingDragCanceled { id };
            shared.events_sink.push_window_event(event, window_id);
            shared.dnd.clear_send_drag(id);
            context.wakeup();
        });
    }

    {
        let shared = event_loop.shared.clone();
        drag.connect_dnd_finished(move |_| {
            let mut shared = shared.borrow_mut();
            shared.dnd.clear_send_drag(id);
        });
    }
}

#[derive(Debug)]
pub(crate) struct DragSource {
    data_transfer_id: DataTransferId,
    _drag: gdk::Drag,
    _provider: GtkContentProvider,
}

impl DragSource {
    pub(crate) fn new(
        data_transfer_id: DataTransferId,
        drag: gdk::Drag,
        provider: GtkContentProvider,
    ) -> Self {
        Self { data_transfer_id, _drag: drag, _provider: provider }
    }
}

#[derive(Debug)]
pub(crate) struct TypedFileData {
    type_: TypeHint,
    paths: Arc<[PathBuf]>,
}

impl TypedFileData {
    fn uris(&self) -> io::Result<Vec<String>> {
        match SendData::from_file_paths(self.paths.iter()) {
            Some(SendData::Uris(uris)) => Ok(uris),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn uri_list_string(&self) -> io::Result<String> {
        let mut uris = self.uris()?.join("\r\n");
        uris.push_str("\r\n");
        Ok(uris)
    }
}

impl TypedData for TypedFileData {
    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        Some(Box::new(io::Cursor::new(self.uri_list_string().ok()?.into_bytes())))
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        self.uris()
    }

    fn try_as_file_paths(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self.paths.to_vec())
    }

    fn try_as_string(&self) -> io::Result<String> {
        self.uri_list_string()
    }
}

pub(crate) fn dnd_actions_to_gdk(actions: &[DndAction]) -> gdk::DragAction {
    actions
        .iter()
        .copied()
        .map(dnd_action_winit_to_gdk)
        .fold(gdk::DragAction::empty(), BitOr::bitor)
}

pub(crate) fn dnd_action_winit_to_gdk(action: DndAction) -> gdk::DragAction {
    match action {
        DndAction::Move => gdk::DragAction::MOVE,
        DndAction::Copy => gdk::DragAction::COPY,
        DndAction::Link => gdk::DragAction::LINK,
        DndAction::Ask => gdk::DragAction::ASK,
        DndAction::Private => gdk::DragAction::empty(),
        _ => gdk::DragAction::empty(),
    }
}

pub(crate) fn dnd_action_gdk_to_winit(action: gdk::DragAction) -> Option<DndAction> {
    if action.contains(gdk::DragAction::MOVE) {
        Some(DndAction::Move)
    } else if action.contains(gdk::DragAction::COPY) {
        Some(DndAction::Copy)
    } else if action.contains(gdk::DragAction::LINK) {
        Some(DndAction::Link)
    } else if action.contains(gdk::DragAction::ASK) {
        Some(DndAction::Ask)
    } else {
        None
    }
}

pub(crate) fn create_content_provider(
    send_data: Box<dyn DataTransferSend>,
) -> Option<GtkContentProvider> {
    let mut types = HashSet::new();
    send_data.for_each_available_type(&mut |type_| {
        if <TypeHint as TransferType>::matches(&TypeHint::UriList, type_) {
            types.insert(GtkSendType::FileList);
        }

        if <TypeHint as TransferType>::matches(&TypeHint::Plaintext, type_) {
            types.insert(GtkSendType::String);
        }

        for mime_type in MimeType::from_dyn(type_) {
            types.insert(GtkSendType::Mime(mime_type));
        }

        ControlFlow::Continue(())
    });

    let types = types.into_iter().collect::<Vec<_>>();
    (!types.is_empty()).then(|| GtkContentProvider::new(send_data, types))
}

fn encode_uri_list<I>(uris: I) -> Vec<u8>
where
    I: IntoIterator<Item = String>,
{
    let mut out = Vec::new();
    for uri in uris {
        out.extend_from_slice(uri.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
enum GtkSendType {
    FileList,
    String,
    Mime(MimeType),
}

impl GtkSendType {
    fn add_to_formats(&self, builder: gdk::ContentFormatsBuilder) -> gdk::ContentFormatsBuilder {
        match self {
            GtkSendType::FileList => builder.add_type(gdk::FileList::static_type()),
            GtkSendType::String => builder.add_type(String::static_type()),
            GtkSendType::Mime(mime_type) => builder.add_mime_type(mime_type.as_str()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct MimeType {
    mime: Arc<str>,
    hint: Option<TypeHint>,
}

impl MimeType {
    const MIME_HINT_MAP: &[(&str, TypeHint)] = &[
        ("text/uri-list", TypeHint::UriList),
        ("text/plain", TypeHint::Plaintext),
        ("text/plain; charset=utf-8", TypeHint::Plaintext),
        ("text/html", TypeHint::Html),
        ("text/html; charset=utf-8", TypeHint::Html),
        ("application/rtf", TypeHint::Rtf),
        ("audio/aac", TypeHint::Audio { extension_hint: Some("aac") }),
        ("audio/aiff", TypeHint::Audio { extension_hint: Some("aif") }),
        ("audio/flac", TypeHint::Audio { extension_hint: Some("flac") }),
        ("audio/vnd.wav", TypeHint::Audio { extension_hint: Some("wav") }),
        ("audio/vnd.wave", TypeHint::Audio { extension_hint: Some("wav") }),
        ("audio/wav", TypeHint::Audio { extension_hint: Some("wav") }),
        ("audio/wave", TypeHint::Audio { extension_hint: Some("wav") }),
        ("audio/x-wav", TypeHint::Audio { extension_hint: Some("wav") }),
        ("audio/ogg", TypeHint::Audio { extension_hint: Some("ogg") }),
        ("audio/mpeg", TypeHint::Audio { extension_hint: Some("mp3") }),
        ("image/bmp", TypeHint::Image { extension_hint: Some("bmp") }),
        ("image/gif", TypeHint::Image { extension_hint: Some("gif") }),
        ("image/jpeg", TypeHint::Image { extension_hint: Some("jpg") }),
        ("image/pjpeg", TypeHint::Image { extension_hint: Some("jpg") }),
        ("image/png", TypeHint::Image { extension_hint: Some("png") }),
        ("image/svg+xml", TypeHint::Image { extension_hint: Some("svg") }),
        ("image/tiff", TypeHint::Image { extension_hint: Some("tiff") }),
        ("image/webp", TypeHint::Image { extension_hint: Some("webp") }),
        ("image/x-icon", TypeHint::Image { extension_hint: Some("ico") }),
        ("image/x-panasonic-raw", TypeHint::Image { extension_hint: Some("raw") }),
    ];

    fn from_dyn(type_: &dyn TransferType) -> impl Iterator<Item = Self> + '_ {
        let downcast = type_.cast_ref::<Self>().cloned();
        let from_hint = downcast.is_none().then_some(()).into_iter().flat_map(move |_| {
            Self::MIME_HINT_MAP
                .iter()
                .filter(move |(_, hint)| <TypeHint as TransferType>::matches(hint, type_))
                .map(|(mime, hint)| Self { mime: (*mime).into(), hint: Some(*hint) })
        });

        downcast.into_iter().chain(from_hint)
    }

    fn parse(mime: &str) -> Self {
        let hint = Self::MIME_HINT_MAP
            .iter()
            .find_map(|(haystack, hint)| (*haystack == mime).then_some(*hint))
            .or_else(|| {
                if mime.starts_with("image/") {
                    Some(TypeHint::Image { extension_hint: None })
                } else if mime.starts_with("audio/") {
                    Some(TypeHint::Audio { extension_hint: None })
                } else {
                    None
                }
            });

        Self { mime: mime.into(), hint }
    }

    fn as_str(&self) -> &str {
        &self.mime
    }

    fn validate_utf8_charset(&self) -> Result<(), glib::Error> {
        let split = self.mime.split_once(';').map(|(_, opts)| opts);
        let Some((_, charset)) = split.and_then(|opts| opts.split_once("charset=")) else {
            return Ok(());
        };
        let charset = charset.split_once(',').map(|(first, _)| first).unwrap_or(charset).trim();

        if charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("utf8") {
            Ok(())
        } else {
            let message = format!("unsupported charset: {charset}");
            Err(glib::Error::new(gio::IOErrorEnum::InvalidData, &message))
        }
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.mime.fmt(f)
    }
}

impl TransferType for MimeType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other_mime) = other.cast_ref::<Self>() {
            self == other_mime
        } else {
            self.hint().is_some_and(|hint| other.hint().is_some_and(|other| hint.matches(&other)))
        }
    }
}

glib::wrapper! {
    pub(crate) struct GtkContentProvider(ObjectSubclass<send_provider::Provider>)
        @extends gdk::ContentProvider;
}

impl GtkContentProvider {
    fn new(send_data: Box<dyn DataTransferSend>, types: Vec<GtkSendType>) -> Self {
        let provider: Self = glib::Object::new();
        let imp = provider.imp();
        *imp.send_data.borrow_mut() = Some(send_data);
        *imp.types.borrow_mut() = types;
        provider
    }
}

mod send_provider {
    use super::*;

    #[derive(Default)]
    pub(crate) struct Provider {
        pub(super) send_data: RefCell<Option<Box<dyn DataTransferSend>>>,
        pub(super) types: RefCell<Vec<GtkSendType>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Provider {
        const NAME: &'static str = "WinitGtk4ContentProvider";
        type Type = GtkContentProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for Provider {}

    impl ContentProviderImpl for Provider {
        fn formats(&self) -> gdk::ContentFormats {
            self.types
                .borrow()
                .iter()
                .fold(gdk::ContentFormats::builder(), |builder, type_| {
                    type_.add_to_formats(builder)
                })
                .build()
        }

        fn storable_formats(&self) -> gdk::ContentFormats {
            self.formats()
        }

        fn value(&self, type_: glib::Type) -> Result<glib::Value, glib::Error> {
            if type_ == gdk::FileList::static_type() {
                let data = self.data_for_type(&TypeHint::UriList)?;
                return file_list_value(data);
            }

            if type_ == String::static_type() {
                let data = self.data_for_type(&TypeHint::Plaintext)?;
                return string_value(data);
            }

            Err(io_error(gio::IOErrorEnum::NotSupported, "unsupported GDK content value type"))
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let stream = stream.clone();
            let bytes = self.bytes_for_mime_type(mime_type);

            Box::pin(async move {
                let bytes = bytes?;
                let result = stream.write_all_future(bytes, io_priority).await;
                let (_, _, partial_error) = result.map_err(|(_, error)| error)?;
                if let Some(error) = partial_error { Err(error) } else { Ok(()) }
            })
        }
    }

    impl Provider {
        fn data_for_type(&self, type_: &dyn TransferType) -> Result<SendData, glib::Error> {
            self.send_data
                .borrow()
                .as_ref()
                .and_then(|send_data| send_data.data_for_type(type_))
                .ok_or_else(|| io_error(gio::IOErrorEnum::NotFound, "data type is not available"))
        }

        fn bytes_for_mime_type(&self, mime_type: &str) -> Result<Vec<u8>, glib::Error> {
            let mime_type = MimeType::parse(mime_type);
            let data = self.data_for_type(&mime_type)?;
            send_data_to_bytes(data, &mime_type)
        }
    }

    fn file_list_value(data: SendData) -> Result<glib::Value, glib::Error> {
        let uris = match data {
            SendData::Uris(uris) => uris,
            SendData::String(uri) => vec![uri],
            _ => {
                return Err(io_error(gio::IOErrorEnum::InvalidData, "URI list data was bytes"));
            },
        };

        let files = uris.iter().map(|uri| gio::File::for_uri(uri.as_str())).collect::<Vec<_>>();
        Ok(gdk::FileList::from_array(&files).to_value())
    }

    fn string_value(data: SendData) -> Result<glib::Value, glib::Error> {
        match data {
            SendData::String(string) => Ok(string.to_value()),
            SendData::Uris(uris) => Ok(String::from_utf8(encode_uri_list(uris))
                .map_err(|_| io_error(gio::IOErrorEnum::InvalidData, "URI list was not UTF-8"))?
                .to_value()),
            _ => Err(io_error(gio::IOErrorEnum::InvalidData, "string data was bytes")),
        }
    }

    fn send_data_to_bytes(data: SendData, mime_type: &MimeType) -> Result<Vec<u8>, glib::Error> {
        match data {
            SendData::Uris(uris) => Ok(encode_uri_list(uris)),
            SendData::String(string) => {
                mime_type.validate_utf8_charset()?;
                Ok(string.into_bytes())
            },
            SendData::Bytes(bytes) => Ok(bytes),
            _ => Err(io_error(gio::IOErrorEnum::NotSupported, "unsupported data kind")),
        }
    }

    fn io_error(message_kind: gio::IOErrorEnum, message: impl AsRef<str>) -> glib::Error {
        glib::Error::new(message_kind, message.as_ref())
    }
}
