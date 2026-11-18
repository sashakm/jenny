use jenny_security::authentication::api_key::ApiKeyAuthContextConfig;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_api_key_store_refresh() {
    let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let temp_path = temp_file.path().to_string_lossy().to_string();
    let initial_data: HashMap<String, Value> = HashMap::from([
        ("key1".to_string(), json!({"user_id": 1, "name": "Alice"})),
        ("key2".to_string(), json!({"user_id": 2, "name": "Bob"})),
    ]);

    let initial_json = serde_json::to_string_pretty(&initial_data).unwrap();
    temp_file
        .write_all(initial_json.as_bytes())
        .expect("Failed to write initial data");
    temp_file.flush().expect("Failed to flush");

    // Look at the file on every lookup, so each rewrite below is seen immediately.
    let store = ApiKeyAuthContextConfig::new_file_backed(&temp_path)
        .with_refresh_interval(std::time::Duration::ZERO);

    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"user_id": 1, "name": "Alice"}))
    );
    assert_eq!(
        store.get("key2").await.unwrap(),
        Some(json!({"user_id": 2, "name": "Bob"}))
    );
    assert_eq!(store.get("key3").await.unwrap(), None::<Value>);

    let update1: HashMap<String, Value> = HashMap::from([
        ("key1".to_string(), json!({"user_id": 1, "name": "Alice"})),
        ("key2".to_string(), json!({"user_id": 2, "name": "Bob"})),
        ("key3".to_string(), json!({"user_id": 3, "name": "Charlie"})),
    ]);
    let update1_json = serde_json::to_string_pretty(&update1).unwrap();
    std::fs::write(&temp_path, update1_json).expect("Failed to write update 1");

    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"user_id": 1, "name": "Alice"}))
    );
    assert_eq!(
        store.get("key2").await.unwrap(),
        Some(json!({"user_id": 2, "name": "Bob"}))
    );
    assert_eq!(
        store.get("key3").await.unwrap(),
        Some(json!({"user_id": 3, "name": "Charlie"}))
    );

    let update2: HashMap<String, Value> = HashMap::from([
        (
            "key1".to_string(),
            json!({"user_id": 1, "name": "Alice Updated", "role": "admin"}),
        ),
        ("key2".to_string(), json!({"user_id": 2, "name": "Bob"})),
        ("key3".to_string(), json!({"user_id": 3, "name": "Charlie"})),
    ]);
    let update2_json = serde_json::to_string_pretty(&update2).unwrap();
    std::fs::write(&temp_path, update2_json).expect("Failed to write update 2");

    assert_eq!(
        store.get("key2").await.unwrap(),
        Some(json!({"user_id": 2, "name": "Bob"}))
    );
    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"user_id": 1, "name": "Alice Updated", "role": "admin"})),
    );
    assert_eq!(
        store.get("key3").await.unwrap(),
        Some(json!({"user_id": 3, "name": "Charlie"}))
    );

    let update3: HashMap<String, Value> = HashMap::from([
        (
            "key1".to_string(),
            json!({"user_id": 1, "name": "Alice Updated", "role": "admin"}),
        ),
        ("key3".to_string(), json!({"user_id": 3, "name": "Charlie"})),
    ]);
    let update3_json = serde_json::to_string_pretty(&update3).unwrap();
    std::fs::write(&temp_path, update3_json).expect("Failed to write update 3");

    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"user_id": 1, "name": "Alice Updated", "role": "admin"}))
    );
    assert_eq!(store.get("key2").await.unwrap(), None::<Value>);
    assert_eq!(
        store.get("key3").await.unwrap(),
        Some(json!({"user_id": 3, "name": "Charlie"}))
    );

    let update4: HashMap<String, Value> = HashMap::from([
        (
            "key1".to_string(),
            json!({"user_id": 1, "name": "Alice Updated", "role": "admin"}),
        ),
        ("key3".to_string(), json!({"user_id": 3, "name": "Charlie"})),
        ("key4".to_string(), json!({"user_id": 4, "name": "David"})),
        (
            "key5".to_string(),
            json!({"user_id": 5, "name": "Eve", "role": "moderator"}),
        ),
    ]);
    let update4_json = serde_json::to_string_pretty(&update4).unwrap();
    std::fs::write(&temp_path, update4_json).expect("Failed to write update 4");

    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"user_id": 1, "name": "Alice Updated", "role": "admin"}))
    );
    assert_eq!(store.get("key2").await.unwrap(), None::<Value>);
    assert_eq!(
        store.get("key3").await.unwrap(),
        Some(json!({"user_id": 3, "name": "Charlie"}))
    );
    assert_eq!(
        store.get("key4").await.unwrap(),
        Some(json!({"user_id": 4, "name": "David"}))
    );
    assert_eq!(
        store.get("key5").await.unwrap(),
        Some(json!({"user_id": 5, "name": "Eve", "role": "moderator"}))
    );
}

#[tokio::test]
async fn a_broken_rewrite_keeps_the_previous_store() {
    let file = NamedTempFile::new().unwrap();
    std::fs::write(file.path(), r#"{"key1": {"id": "alice"}}"#).unwrap();
    let store = ApiKeyAuthContextConfig::new_file_backed(&file.path().to_string_lossy())
        .with_refresh_interval(std::time::Duration::ZERO);
    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"id": "alice"}))
    );

    std::fs::write(file.path(), "{ not json").unwrap();
    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"id": "alice"})),
        "a malformed edit must not lock every client out"
    );

    std::fs::remove_file(file.path()).unwrap();
    assert_eq!(
        store.get("key1").await.unwrap(),
        Some(json!({"id": "alice"}))
    );
}

#[tokio::test]
async fn a_store_that_never_loaded_fails_without_naming_its_path() {
    let store = ApiKeyAuthContextConfig::new_file_backed("/nonexistent/keys.json");
    let err = store.get::<Value>("key1").await.unwrap_err().to_string();
    assert_eq!(err, "the api key store is unavailable");
}

#[tokio::test]
async fn the_file_is_not_looked_at_more_often_than_the_interval() {
    let file = NamedTempFile::new().unwrap();
    std::fs::write(file.path(), r#"{"key1": {"id": "alice"}}"#).unwrap();
    let store = ApiKeyAuthContextConfig::new_file_backed(&file.path().to_string_lossy())
        .with_refresh_interval(std::time::Duration::from_secs(3600));
    assert!(store.get::<Value>("key1").await.unwrap().is_some());

    std::fs::write(file.path(), r#"{"key2": {"id": "bob"}}"#).unwrap();
    assert!(
        store.get::<Value>("key2").await.unwrap().is_none(),
        "the rewrite is only picked up once the interval has passed"
    );
}
