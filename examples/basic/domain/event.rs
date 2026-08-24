//! What happened. Definitions only — nothing here knows about a bus.

use jenny::DomainEvent;

use super::model::{NoteId, NoteTitle};

/// A note was created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteCreated {
    pub id: NoteId,
    pub title: NoteTitle,
}

impl DomainEvent for NoteCreated {
    fn name(&self) -> &str {
        "note.created"
    }
}

/// A note was archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteArchived {
    pub id: NoteId,
}

impl DomainEvent for NoteArchived {
    fn name(&self) -> &str {
        "note.archived"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_name_themselves_in_past_tense() {
        let created = NoteCreated {
            id: NoteId::generate(),
            title: NoteTitle::new("Shopping").unwrap(),
        };
        assert_eq!(created.name(), "note.created");
        assert_eq!(NoteArchived { id: created.id }.name(), "note.archived");
    }
}
