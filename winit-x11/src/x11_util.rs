//! Re-exports of X11 helpers for sibling backend crates.

pub use crate::atoms::{AtomName, Atoms};
pub use crate::util::{
    FrameExtents, FrameExtentsHeuristic, FrameExtentsHeuristicPath, StateOperation,
};
pub use crate::xdisplay::{XConnection, XErrorHandler, XNotSupported};
