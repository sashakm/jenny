//! Entities and value objects. Invariants live in the types.

use jenny::{DomainError, DomainResult};
use uuid::Uuid;

use super::event::{NoteArchived, NoteCreated};

/// A note's identity. A newtype rather than a bare `Uuid`, so a note id can never be
/// passed where an author id was meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteId(Uuid);

impl NoteId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for NoteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A note's title, guaranteed non-empty and at most [`NoteTitle::MAX_LEN`] characters.
///
/// This is the "make illegal states unrepresentable" rule at work. Because the only
/// way to obtain a `NoteTitle` is through [`new`](NoteTitle::new), no code downstream
/// — no use case, no repository, no handler — ever has to re-check whether a title is
/// blank. The check happens once, and the type carries the proof afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteTitle(String);

impl NoteTitle {
    pub const MAX_LEN: usize = 120;

    pub fn new(raw: impl Into<String>) -> DomainResult<Self> {
        let raw = raw.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainError::invalid("title must not be empty"));
        }
        if trimmed.chars().count() > Self::MAX_LEN {
            return Err(DomainError::invalid(format!(
                "title must be at most {} characters",
                Self::MAX_LEN
            )));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NoteTitle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The aggregate.
///
/// State changes go through methods that enforce the rules and hand back the event
/// they raised. The aggregate does not publish anything — raising and publishing are
/// separate jobs, and publishing is the application layer's, after the state change
/// has been persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    id: NoteId,
    title: NoteTitle,
    body: String,
    archived: bool,
}

impl Note {
    /// Create a note, returning it together with the event that says so.
    pub fn create(title: NoteTitle, body: String) -> (Self, NoteCreated) {
        let note = Self {
            id: NoteId::generate(),
            title,
            body,
            archived: false,
        };
        let event = NoteCreated {
            id: note.id,
            title: note.title.clone(),
        };
        (note, event)
    }

    /// Reconstitute a note from storage. Only a driven persistence adapter should call
    /// this: it bypasses [`create`](Self::create) and so raises no event, which is
    /// correct when loading something that already happened.
    pub fn rehydrate(id: NoteId, title: NoteTitle, body: String, archived: bool) -> Self {
        Self {
            id,
            title,
            body,
            archived,
        }
    }

    pub fn id(&self) -> NoteId {
        self.id
    }

    pub fn title(&self) -> &NoteTitle {
        &self.title
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn is_archived(&self) -> bool {
        self.archived
    }

    /// Edit a note. Archived notes are immutable.
    pub fn edit(&mut self, title: NoteTitle, body: String) -> DomainResult<()> {
        if self.archived {
            return Err(DomainError::conflict("an archived note cannot be edited"));
        }
        self.title = title;
        self.body = body;
        Ok(())
    }

    /// Archive a note. Archiving twice is a conflict, not a silent no-op — the caller
    /// asked for a state change that cannot happen, and deserves to hear about it.
    pub fn archive(&mut self) -> DomainResult<NoteArchived> {
        if self.archived {
            return Err(DomainError::conflict("note is already archived"));
        }
        self.archived = true;
        Ok(NoteArchived { id: self.id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(raw: &str) -> NoteTitle {
        NoteTitle::new(raw).unwrap()
    }

    #[test]
    fn a_title_must_carry_something() {
        assert!(NoteTitle::new("").is_err());
        assert!(NoteTitle::new("   ").is_err());
        assert!(matches!(
            NoteTitle::new("\t\n").unwrap_err(),
            DomainError::Invalid { .. }
        ));
    }

    #[test]
    fn a_title_is_trimmed_and_bounded() {
        assert_eq!(title("  hello  ").as_str(), "hello");
        assert!(NoteTitle::new("x".repeat(NoteTitle::MAX_LEN)).is_ok());
        assert!(NoteTitle::new("x".repeat(NoteTitle::MAX_LEN + 1)).is_err());
    }

    #[test]
    fn creating_a_note_raises_the_matching_event() {
        let (note, event) = Note::create(title("Shopping"), "milk".into());
        assert_eq!(event.id, note.id());
        assert_eq!(event.title.as_str(), "Shopping");
        assert!(!note.is_archived());
    }

    #[test]
    fn archiving_twice_is_a_conflict() {
        let (mut note, _) = Note::create(title("Shopping"), String::new());

        let event = note.archive().unwrap();
        assert_eq!(event.id, note.id());
        assert!(note.is_archived());

        assert!(matches!(
            note.archive().unwrap_err(),
            DomainError::Conflict { .. }
        ));
    }

    #[test]
    fn an_archived_note_cannot_be_edited() {
        let (mut note, _) = Note::create(title("Shopping"), String::new());
        note.edit(title("Groceries"), "eggs".into()).unwrap();
        assert_eq!(note.title().as_str(), "Groceries");
        assert_eq!(note.body(), "eggs");

        note.archive().unwrap();
        assert!(matches!(
            note.edit(title("Nope"), String::new()).unwrap_err(),
            DomainError::Conflict { .. }
        ));
    }

    #[test]
    fn rehydration_restores_state_without_raising_anything() {
        let id = NoteId::generate();
        let note = Note::rehydrate(id, title("Old"), "body".into(), true);
        assert_eq!(note.id(), id);
        assert!(note.is_archived());
    }
}
