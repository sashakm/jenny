//! The types every inbound adapter converges on.
//!
//! A REST DTO, a protobuf message and a CLI argument struct all differ — they should,
//! they answer to different wire formats. What makes them interchangeable *drivers* of
//! the same behaviour is that each converts into one of these commands. The
//! convergence point is here, not in a shared DTO.
//!
//! Note that the fields are already domain types. Turning a `String` into a
//! [`NoteTitle`] is the adapter's job, on the way in, which is why a use case can take
//! one of these and never wonder whether the title is blank.

use crate::domain::model::{NoteId, NoteTitle};

pub struct CreateNote {
    pub title: NoteTitle,
    pub body: String,
}

pub struct UpdateNote {
    pub id: NoteId,
    pub title: NoteTitle,
    pub body: String,
}
