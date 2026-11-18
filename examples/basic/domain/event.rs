//! What happened. Definitions only — nothing here knows about a bus.
//!
//! Events derive `Serialize`: what they carry *is* their published contract, so the
//! encoding belongs with the definition rather than in a mapping one ring out.

use jenny::DomainEvent;
use serde::Serialize;

use super::model::{NoteId, NoteTitle};

/// A note was created. Published as `note.created`, the name derived from the type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, DomainEvent)]
pub struct NoteCreated {
    pub id: NoteId,
    pub title: NoteTitle,
}

/// A note was archived.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, DomainEvent)]
pub struct NoteArchived {
    pub id: NoteId,
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

    #[test]
    fn an_event_carries_its_fields_on_the_wire() {
        let id = NoteId::generate();
        let created = NoteCreated {
            id,
            title: NoteTitle::new("Shopping").unwrap(),
        };
        assert_eq!(
            serde_json::to_value(&created).unwrap(),
            serde_json::json!({ "id": id.as_uuid(), "title": "Shopping" })
        );
    }
}
