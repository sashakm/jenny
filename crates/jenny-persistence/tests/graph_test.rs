//! `Graph` against a real Postgres with Apache AGE: parameters, placeholders, what rows
//! decode into, transactions of the caller's, and the graph being made before serving.
//!
//! Needs `JENNY_TEST_AGE_URL`, a database with the AGE extension available, such as
//! the `apache/age` image. `just test-age` starts one and sets it; CI does the same.
//! Without it the tests say so and pass, except under `CI`, where a missing database is
//! a failure rather than a skip.

use jenny_core::domain::DomainError;
use jenny_persistence::{Graph, Postgres, PostgresConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The database URL, or `None` to skip — never `None` under CI.
fn database_url() -> Option<String> {
    match std::env::var("JENNY_TEST_AGE_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!(
                "JENNY_TEST_AGE_URL must be set in CI; these tests would otherwise pass without running"
            )
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_AGE_URL is not set (try `just test-age`)");
            None
        }
    }
}

fn postgres(url: &str, create_tables: bool) -> Postgres {
    Postgres::connect_lazy(&PostgresConfig {
        url: Some(url.to_string()),
        create_tables,
        ..Default::default()
    })
    .unwrap()
}

/// A graph of its own, so tests run in parallel, dropped at the end.
struct Fixture {
    postgres: Postgres,
    graph: Graph,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = database_url()?;
        let postgres = postgres(&url, true);
        let name = format!("graph_test_{}", uuid::Uuid::new_v4().simple());
        let graph = postgres
            .graph(&name)
            .identifiers(["Person", "KNOWS", "FOLLOWS"]);
        postgres.prepare().await.unwrap();
        Some(Self { postgres, graph })
    }

    /// Ada knows Grace, who knows Linus; Ada follows Linus.
    async fn seeded() -> Option<Self> {
        let fixture = Self::new().await?;
        for (id, name) in [(1, "Ada"), (2, "Grace"), (3, "Linus")] {
            fixture
                .graph
                .cypher("CREATE (:Person {id: $id, name: $name})")
                .bind("id", id)
                .bind("name", name)
                .execute()
                .await
                .unwrap();
        }
        for (from, relation, to) in [(1, "KNOWS", 2), (2, "KNOWS", 3), (1, "FOLLOWS", 3)] {
            fixture
                .graph
                .cypher(
                    "MATCH (a:Person {id: $from}), (b:Person {id: $to}) \
                     CREATE (a)-[:{{relation}} {since: 2020}]->(b)",
                )
                .bind("from", from)
                .bind("to", to)
                .identifier("relation", relation)
                .execute()
                .await
                .unwrap();
        }
        Some(fixture)
    }

    async fn drop(self) {
        sqlx::query("SELECT ag_catalog.drop_graph($1, true)")
            .bind(self.graph.name())
            .execute(self.postgres.pool())
            .await
            .unwrap();
    }
}

#[derive(Debug, PartialEq, Deserialize)]
struct Vertex {
    id: i64,
    label: String,
    properties: Person,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Person {
    id: i64,
    name: String,
}

#[tokio::test]
async fn traversals_bind_parameters_and_decode_rows() {
    let Some(fixture) = Fixture::seeded().await else {
        return;
    };
    let graph = &fixture.graph;

    let known: Vec<String> = graph
        .cypher("MATCH (:Person {id: $id})-[:KNOWS*1..2]->(f:Person) RETURN f.name ORDER BY f.name")
        .bind("id", 1)
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(known, ["Grace", "Linus"]);

    // A vertex arrives with its id, label and properties.
    let ada: Vertex = graph
        .cypher("MATCH (p:Person {name: $name}) RETURN p")
        .bind("name", "Ada")
        .fetch_optional()
        .await
        .unwrap()
        .expect("Ada is there");
    assert_eq!(ada.label, "Person");
    assert_eq!(
        ada.properties,
        Person {
            id: 1,
            name: "Ada".into()
        }
    );

    // A map is how a query returns several things per row.
    let edges: Vec<Value> = graph
        .cypher(
            "MATCH (a:Person)-[r]->(b:Person) \
             RETURN {from: a.name, relation: type(r), to: b.name, since: r.since} \
             ORDER BY a.name, b.name",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        edges,
        [
            json!({ "from": "Ada", "relation": "KNOWS", "to": "Grace", "since": 2020 }),
            json!({ "from": "Ada", "relation": "FOLLOWS", "to": "Linus", "since": 2020 }),
            json!({ "from": "Grace", "relation": "KNOWS", "to": "Linus", "since": 2020 }),
        ]
    );

    // A path is the vertices and edges along it; an edge names both ends.
    let path: Vec<Value> = graph
        .cypher("MATCH p = (:Person {id: 1})-[:KNOWS]->(:Person {id: 2}) RETURN p")
        .fetch_optional()
        .await
        .unwrap()
        .expect("the path exists");
    assert_eq!(path.len(), 3);
    assert_eq!(path[1]["label"], "KNOWS");
    assert_eq!(path[1]["start_id"], json!(ada.id));
    assert_eq!(path[2]["properties"]["name"], "Grace");

    // Numbers of every kind, and nothing at all.
    let numbers: Value = graph
        .cypher("RETURN {integer: 1, float: 1.5, numeric: 2.5::numeric, none: null}")
        .fetch_optional()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        numbers,
        json!({ "integer": 1, "float": 1.5, "numeric": 2.5, "none": null })
    );
    let nobody: Option<Vertex> = graph
        .cypher("MATCH (p:Person {name: 'nobody'}) RETURN p")
        .fetch_optional()
        .await
        .unwrap();
    assert_eq!(nobody, None);

    fixture.drop().await;
}

#[tokio::test]
async fn a_parameter_is_a_value_never_cypher() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let graph = &fixture.graph;
    let hostile = "x'}) MATCH (n) DETACH DELETE n // $jenny_cypher$ $$ \"\\";
    graph
        .cypher("CREATE (:Person {id: 1, name: $name, tags: $tags, meta: $meta})")
        .bind("name", hostile)
        .bind("tags", ["a", "b"])
        .bind("meta", json!({ "nested": { "deep": true } }))
        .execute()
        .await
        .unwrap();

    let stored: Value = graph
        .cypher("MATCH (p:Person) RETURN {name: p.name, tags: p.tags, meta: p.meta}")
        .fetch_optional()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored,
        json!({ "name": hostile, "tags": ["a", "b"], "meta": { "nested": { "deep": true } } })
    );
    fixture.drop().await;
}

#[tokio::test]
async fn a_relationship_off_the_allowlist_never_reaches_the_database() {
    let Some(fixture) = Fixture::seeded().await else {
        return;
    };
    let graph = &fixture.graph;

    let followed: Vec<String> = graph
        .cypher("MATCH (:Person {id: 1})-[:{{relation}}]->(f) RETURN f.name")
        .identifier("relation", "FOLLOWS")
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(followed, ["Linus"]);

    let refused = graph
        .cypher("MATCH (:Person {id: 1})-[:{{relation}}]->(f) RETURN f.name")
        .identifier("relation", "KNOWS]->() DETACH DELETE f //")
        .fetch_all::<String>()
        .await
        .unwrap_err();
    assert!(
        matches!(refused, DomainError::Invalid { .. }),
        "{refused:?}"
    );

    let everyone: Vec<String> = graph
        .cypher("MATCH (p:Person) RETURN p.name")
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(everyone.len(), 3);
    fixture.drop().await;
}

#[tokio::test]
async fn graph_writes_commit_or_roll_back_with_the_callers_transaction() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let pool = fixture.postgres.pool();
    let schema = format!("graph_tx_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; CREATE TABLE {schema}.audit (what text)"
    )))
    .execute(pool)
    .await
    .unwrap();

    let mut transaction = pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path = {schema}"
    )))
    .execute(&mut *transaction)
    .await
    .unwrap();
    fixture
        .graph
        .cypher("CREATE (:Person {id: 1, name: 'Ada'})")
        .fetch_all_in::<Value>(&mut transaction)
        .await
        .unwrap();
    // The caller's own schema still resolves after the graph query.
    sqlx::query("INSERT INTO audit VALUES ('added Ada')")
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.rollback().await.unwrap();

    let people = || {
        fixture
            .graph
            .cypher("MATCH (p:Person) RETURN p.name")
            .fetch_all::<String>()
    };
    assert!(people().await.unwrap().is_empty(), "rolled back");

    let mut transaction = pool.begin().await.unwrap();
    fixture
        .graph
        .cypher("CREATE (:Person {id: 2, name: 'Grace'})")
        .fetch_all_in::<Value>(&mut transaction)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {schema}.audit VALUES ('added Grace')"
    )))
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(people().await.unwrap(), ["Grace"]);

    let audit: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT what FROM {schema}.audit"
    )))
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(audit, ["added Grace"]);

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(pool)
        .await
        .unwrap();
    fixture.drop().await;
}

#[tokio::test]
async fn the_search_path_of_a_pooled_connection_is_left_alone() {
    let Some(url) = database_url() else {
        return;
    };
    // One connection, so the query after the graph's is on the same one.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let postgres = Postgres::from_pool(pool.clone());
    let name = format!("graph_test_{}", uuid::Uuid::new_v4().simple());
    let graph = postgres.graph(&name);
    postgres.prepare().await.unwrap();

    let before: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(&pool)
        .await
        .unwrap();
    graph.cypher("RETURN 1").execute().await.unwrap();
    let after: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);

    sqlx::query("SELECT ag_catalog.drop_graph($1, true)")
        .bind(&name)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn prepare_makes_the_graph_once_even_from_several_replicas() {
    let Some(url) = database_url() else {
        return;
    };
    let name = format!("graph_test_{}", uuid::Uuid::new_v4().simple());
    let replicas: Vec<Postgres> = (0..4).map(|_| postgres(&url, true)).collect();
    for replica in &replicas {
        replica.graph(&name);
    }
    let prepared =
        futures::future::join_all(replicas.iter().map(|replica| replica.prepare())).await;
    for result in prepared {
        result.unwrap();
    }
    // And again, as a restart would.
    replicas[0].prepare().await.unwrap();

    sqlx::query("SELECT ag_catalog.drop_graph($1, true)")
        .bind(&name)
        .execute(replicas[0].pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn without_create_tables_a_missing_graph_stops_the_service() {
    let Some(url) = database_url() else {
        return;
    };
    let postgres = postgres(&url, false);
    let graph = postgres.graph(&format!("graph_test_{}", uuid::Uuid::new_v4().simple()));
    let err = postgres.prepare().await.unwrap_err();
    assert!(
        format!("{err:#}").contains(&format!("graph `{}` does not exist", graph.name())),
        "{err:#}"
    );

    // What a migration would run, in the transaction a migration runs in, makes it pass.
    let mut migration = postgres.pool().begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(graph.schema()))
        .execute(&mut *migration)
        .await
        .unwrap();
    migration.commit().await.unwrap();
    postgres.prepare().await.unwrap();
    sqlx::query("SELECT ag_catalog.drop_graph($1, true)")
        .bind(graph.name())
        .execute(postgres.pool())
        .await
        .unwrap();
}
