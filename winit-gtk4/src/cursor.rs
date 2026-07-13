use winit_core::cursor::{CursorImage, CustomCursorProvider, CustomCursorSource};
use winit_core::error::{NotSupportedError, RequestError};

thread_local! {
    static INVISIBLE_CURSOR: gtk4::gdk::Cursor = create_invisible_cursor();
}

#[derive(Debug)]
pub(crate) struct GtkCustomCursor {
    cursor: gtk4::gdk::Cursor,
}

unsafe impl Send for GtkCustomCursor {}
unsafe impl Sync for GtkCustomCursor {}

impl GtkCustomCursor {
    pub(crate) fn new(source: CustomCursorSource) -> Result<Self, RequestError> {
        let image = match source {
            CustomCursorSource::Image(image) => image,
            CustomCursorSource::Animation { .. } | CustomCursorSource::Url { .. } => {
                return Err(NotSupportedError::new("unsupported cursor kind").into());
            },
        };

        let texture = texture_from_image(&image);
        let cursor = gtk4::gdk::Cursor::from_texture(
            &texture,
            image.hotspot_x() as i32,
            image.hotspot_y() as i32,
            None,
        );

        Ok(Self { cursor })
    }

    pub(crate) fn cursor(&self) -> gtk4::gdk::Cursor {
        self.cursor.clone()
    }
}

impl CustomCursorProvider for GtkCustomCursor {
    fn is_animated(&self) -> bool {
        false
    }
}

pub(crate) fn invisible_cursor() -> gtk4::gdk::Cursor {
    INVISIBLE_CURSOR.with(Clone::clone)
}

fn create_invisible_cursor() -> gtk4::gdk::Cursor {
    let bytes = gtk4::glib::Bytes::from_static(&[0, 0, 0, 0]);
    let texture = gtk4::gdk::MemoryTexture::new(1, 1, gtk4::gdk::MemoryFormat::R8g8b8a8, &bytes, 4);

    gtk4::gdk::Cursor::from_texture(&texture, 0, 0, None)
}

fn texture_from_image(image: &CursorImage) -> gtk4::gdk::MemoryTexture {
    let bytes = gtk4::glib::Bytes::from_owned(image.buffer().to_vec());
    let stride = image.width() as usize * 4;

    gtk4::gdk::MemoryTexture::new(
        image.width() as i32,
        image.height() as i32,
        gtk4::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        stride,
    )
}
