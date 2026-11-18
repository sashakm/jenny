//! `#[derive(Aggregate)]`, `#[derive(DomainEvent)]` and `#[use_cases]`, through the
//! facade the way a service uses them.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use jenny::memory::Memory;
use jenny::port::UseCaseObserver;
use jenny::testing::RecordedEvents;
use jenny::{
    Aggregate, Context, DomainError, DomainEvent, DomainResult, Events, Store, StoreExt, use_cases,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Aggregate)]
struct Note {
    id: u32,
    title: String,
    archived: bool,
}

#[derive(Serialize, Deserialize, Aggregate)]
#[aggregate(kind = "line")]
struct OrderLine {
    #[aggregate(id)]
    number: String,
}

#[derive(Serialize, DomainEvent)]
struct NoteArchived {
    id: u32,
}

#[derive(Serialize, DomainEvent)]
#[event(name = "note.purged.v2")]
struct NotePurged;

impl Note {
    fn archive(&mut self) -> DomainResult<NoteArchived> {
        if self.archived {
            return Err(DomainError::conflict("already archived"));
        }
        self.archived = true;
        Ok(NoteArchived { id: self.id })
    }
}

#[test]
fn an_aggregate_takes_its_identity_from_the_id_field_or_the_marked_one() {
    let note = Note {
        id: 7,
        title: "t".into(),
        archived: false,
    };
    assert_eq!(note.id(), 7);
    assert_eq!(Note::KIND, "note");

    let line = OrderLine {
        number: "A-1".into(),
    };
    assert_eq!(line.id(), "A-1");
    assert_eq!(OrderLine::KIND, "line");
}

#[test]
fn an_event_is_named_after_its_type_unless_told_otherwise() {
    assert_eq!(NoteArchived { id: 1 }.name(), "note.archived");
    assert_eq!(NotePurged.name(), "note.purged.v2");
}

#[derive(Clone)]
struct Notes {
    notes: Arc<dyn Store<Note>>,
    events: Events,
}

#[use_cases]
impl Notes {
    pub async fn create(&self, title: &str, _cx: &Context) -> DomainResult<u32> {
        let id = self.notes.list(&Default::default()).await?.len() as u32 + 1;
        self.notes
            .insert(Note {
                id,
                title: title.to_string(),
                archived: false,
            })
            .await?;
        Ok(id)
    }

    pub async fn archive(&self, id: u32, cx: &Context) -> DomainResult<()> {
        let archived = self.notes.change(&id, Note::archive).await?;
        self.events.publish(&archived.outcome, cx).await;
        Ok(())
    }

    /// Early returns and `?` inside the body behave as they would unwrapped.
    #[use_case(name = "title_of")]
    pub async fn title(&self, id: u32, _cx: &Context) -> DomainResult<String> {
        if id == 0 {
            return Err(DomainError::invalid("ids start at 1"));
        }
        Ok(self.notes.load(&id).await?.title.clone())
    }

    #[use_case(skip)]
    pub async fn unobserved(&self) -> DomainResult<()> {
        Ok(())
    }

    /// Private: a helper, not a use case.
    async fn helper(&self) -> usize {
        self.notes
            .list(&Default::default())
            .await
            .map_or(0, |n| n.len())
    }

    pub fn count_hint(&self) -> &'static str {
        "synchronous methods are left alone"
    }
}

/// `(use case, outcome)` pairs, in the order they finished.
type Seen = Arc<Mutex<Vec<(&'static str, &'static str)>>>;

/// Everything the observer has seen, process-wide: the hook is installed once.
fn seen() -> &'static Seen {
    static SEEN: OnceLock<Seen> = OnceLock::new();

    struct Record(Seen);
    impl UseCaseObserver for Record {
        fn finished(&self, use_case: &'static str, outcome: &'static str, _: Duration) {
            self.0.lock().unwrap().push((use_case, outcome));
        }
    }

    SEEN.get_or_init(|| {
        let seen = Seen::default();
        jenny::port::observe_use_cases(Record(Arc::clone(&seen)));
        seen
    })
}

fn notes() -> (Notes, Arc<RecordedEvents>) {
    let recorded = Arc::new(RecordedEvents::default());
    let notes = Notes {
        notes: Arc::new(Memory::<Note>::new()),
        events: Events::new(recorded.clone()),
    };
    (notes, recorded)
}

#[tokio::test]
async fn every_public_async_method_is_observed_with_its_outcome() {
    let seen = seen();
    let (notes, recorded) = notes();
    let cx = Context::new();

    let id = notes.create("Shopping", &cx).await.unwrap();
    notes.archive(id, &cx).await.unwrap();
    assert!(matches!(
        notes.archive(id, &cx).await.unwrap_err(),
        DomainError::Conflict { .. }
    ));
    assert_eq!(recorded.names(), ["note.archived"]);

    let seen = seen.lock().unwrap().clone();
    assert!(seen.contains(&("notes.create", "ok")), "{seen:?}");
    assert!(seen.contains(&("notes.archive", "ok")), "{seen:?}");
    assert!(seen.contains(&("notes.archive", "conflict")), "{seen:?}");
}

#[tokio::test]
async fn early_returns_and_question_marks_keep_their_meaning() {
    let seen = seen();
    let (notes, _) = notes();
    let cx = Context::new();

    assert!(matches!(
        notes.title(0, &cx).await.unwrap_err(),
        DomainError::Invalid { .. }
    ));
    assert!(matches!(
        notes.title(99, &cx).await.unwrap_err(),
        DomainError::NotFound
    ));
    let id = notes.create("Gardening", &cx).await.unwrap();
    assert_eq!(notes.title(id, &cx).await.unwrap(), "Gardening");

    let seen = seen.lock().unwrap().clone();
    assert!(seen.contains(&("notes.title_of", "invalid")), "{seen:?}");
    assert!(seen.contains(&("notes.title_of", "not_found")), "{seen:?}");
    assert!(seen.contains(&("notes.title_of", "ok")), "{seen:?}");
}

#[tokio::test]
async fn skipped_private_and_synchronous_methods_are_left_alone() {
    let seen = seen();
    let (notes, _) = notes();

    notes.unobserved().await.unwrap();
    assert_eq!(notes.helper().await, 0);
    assert_eq!(notes.count_hint(), "synchronous methods are left alone");

    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|(name, _)| !name.ends_with("unobserved") && !name.ends_with("helper")),
        "{seen:?}"
    );
}
