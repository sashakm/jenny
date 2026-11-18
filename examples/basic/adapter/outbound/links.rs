//! Where the links between notes live.
//!
//! With the `graph` feature and a database, [`GraphLinks`] keeps them in an Apache AGE
//! graph in the same Postgres as the notes: each note a vertex, each link an edge,
//! each question one Cypher query. Without, [`MemoryLinks`] answers the same port from
//! a set. `main.rs` picks one; the use cases cannot tell which.

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;
use jenny::DomainResult;
#[cfg(feature = "graph")]
use jenny::persistence::Graph;

use crate::domain::model::NoteId;
use crate::domain::port::{Links, NoteLinks};

/// Links in memory, for tests and for a service started without a database.
#[derive(Debug, Default)]
pub struct MemoryLinks {
    links: Mutex<BTreeSet<(NoteId, NoteId)>>,
}

impl MemoryLinks {
    fn links(&self) -> std::sync::MutexGuard<'_, BTreeSet<(NoteId, NoteId)>> {
        self.links
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl NoteLinks for MemoryLinks {
    async fn link(&self, from: NoteId, to: NoteId) -> DomainResult<()> {
        self.links().insert((from, to));
        Ok(())
    }

    async fn unlink(&self, from: NoteId, to: NoteId) -> DomainResult<()> {
        self.links().remove(&(from, to));
        Ok(())
    }

    async fn links(&self, id: NoteId) -> DomainResult<Links> {
        let links = self.links();
        Ok(Links {
            to: links
                .iter()
                .filter(|(f, _)| *f == id)
                .map(|(_, t)| *t)
                .collect(),
            from: links
                .iter()
                .filter(|(_, t)| *t == id)
                .map(|(f, _)| *f)
                .collect(),
        })
    }

    async fn forget(&self, id: NoteId) -> DomainResult<()> {
        self.links().retain(|(from, to)| *from != id && *to != id);
        Ok(())
    }
}

/// Links as edges between `Note` vertices in an AGE graph. A vertex holds only the
/// note's id: the note itself lives in its store, and the graph knows what it needs to
/// traverse.
///
/// AGE has no uniqueness constraints, so two concurrent `MERGE`s of the same link can
/// both create it. Reading with `DISTINCT` and deleting every match makes a duplicate
/// harmless.
#[cfg(feature = "graph")]
#[derive(Debug, Clone)]
pub struct GraphLinks {
    graph: Graph,
}

#[cfg(feature = "graph")]
impl GraphLinks {
    pub fn new(graph: Graph) -> Self {
        Self { graph }
    }

    async fn ids(&self, query: &'static str, id: NoteId) -> DomainResult<Vec<NoteId>> {
        self.graph.cypher(query).bind("id", id).fetch_all().await
    }
}

#[cfg(feature = "graph")]
#[async_trait]
impl NoteLinks for GraphLinks {
    async fn link(&self, from: NoteId, to: NoteId) -> DomainResult<()> {
        self.graph
            .cypher(
                "MERGE (a:Note {id: $from}) MERGE (b:Note {id: $to}) \
                 MERGE (a)-[:LINKS_TO]->(b)",
            )
            .bind("from", from)
            .bind("to", to)
            .execute()
            .await
    }

    async fn unlink(&self, from: NoteId, to: NoteId) -> DomainResult<()> {
        self.graph
            .cypher("MATCH (:Note {id: $from})-[r:LINKS_TO]->(:Note {id: $to}) DELETE r")
            .bind("from", from)
            .bind("to", to)
            .execute()
            .await
    }

    async fn links(&self, id: NoteId) -> DomainResult<Links> {
        Ok(Links {
            to: self
                .ids(
                    "MATCH (:Note {id: $id})-[:LINKS_TO]->(n:Note) \
                     RETURN DISTINCT n.id ORDER BY n.id",
                    id,
                )
                .await?,
            from: self
                .ids(
                    "MATCH (n:Note)-[:LINKS_TO]->(:Note {id: $id}) \
                     RETURN DISTINCT n.id ORDER BY n.id",
                    id,
                )
                .await?,
        })
    }

    async fn forget(&self, id: NoteId) -> DomainResult<()> {
        self.graph
            .cypher("MATCH (n:Note {id: $id}) DETACH DELETE n")
            .bind("id", id)
            .execute()
            .await
    }
}
