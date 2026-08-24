use serde::Serialize;
use serde_json::{Map, Value};

/// Casts a [serde_json::Value] to specified type.
///
/// Example
/// ```
/// use serde_json::{Map, Value, json};
/// use uuid::Uuid;
/// use serde::{Deserialize, Serialize};
/// use jenny_persistence::cast_mapmember;
///
/// #[derive(Serialize, Deserialize)]
/// struct Abc {
///     a: i32,
///     b: String,
///     c: Option<Uuid>,
/// }
///
/// let abc = json!({
///     "a": 1i32,
///     "b": format!("Foo"),
///     "c": Some(Uuid::new_v4()),
/// });
///
/// let map = serde_json::from_value::<Map<String, Value>>(abc).unwrap();
/// for (k, v) in map {
///     match k.as_str() {
///         "a" => assert_eq!(1, cast_mapmember!(v, i32).unwrap()),
///         "b" => assert_eq!("Foo", cast_mapmember!(v, String).unwrap()),
///         "c" => assert!(cast_mapmember!(v, Option<Uuid>).unwrap().is_some()),
///         _ => println!("Unknown field"),
///     }
/// }
/// ```
#[macro_export]
macro_rules! cast_mapmember {
    ($mapmem:ident, $dtype:ty) => {
        serde_json::from_value::<$dtype>($mapmem.to_owned())
            .map_err(|e| $crate::MappingError::ArgumentError(e.to_string()))
    };
}

/// Helper function for query construction
/// Example
/// ```
/// # use jenny_persistence::prefixed_attribute;
///
/// assert_eq!("foo_bar", prefixed_attribute("foo", "bar"));
/// assert_eq!("", prefixed_attribute("", ""));
/// assert_eq!("foobar_baz", prefixed_attribute("foo bar", "baz"));
/// ```
pub fn prefixed_attribute(prefix: &str, attr: &str) -> String {
    let mut p_clean = String::default();
    p_clean.extend(prefix.chars().filter(|c| !c.is_whitespace()));
    if !p_clean.is_empty() {
        p_clean.push('_')
    };
    format!("{p_clean}{attr}")
}

/// Helper to create column name to alias mappings
/// Example
/// ```
/// # use jenny_persistence::ColumnAliasMapper;
///
/// let mapping = ColumnAliasMapper::default()
///                 .set_table_name("test")
///                 .set_alias_prefix("XD")
///                 .set_column_names(vec![format!("id"), format!("na me")])
///                 .build_map();
/// assert_eq!("test.id as XD_id, test.name as XD_name", mapping);
/// ```
#[derive(Clone, Debug, Default)]
pub struct ColumnAliasMapper {
    alias: Option<String>,
    table: String,
    columns: Vec<String>,
}

impl ColumnAliasMapper {
    pub fn set_table_name(mut self, name: &str) -> Self {
        name.clone_into(&mut self.table);
        self
    }

    pub fn set_alias_prefix(mut self, alias: &str) -> Self {
        self.alias = Some(alias.to_owned());
        self
    }

    pub fn set_column_names(mut self, names: Vec<String>) -> Self {
        self.columns = names
            .iter()
            .map(|entry| entry.chars().filter(|c| !c.is_whitespace()).collect())
            .collect();
        self
    }

    /// Create mapping
    pub fn build_map(&self) -> String {
        let mut res = String::default();
        let mut prefix = String::default();
        let column_count = self.columns.len() - 1;
        if let Some(ap) = self.alias.as_ref() {
            prefix = format!("{ap}_");
        }
        for (i, n) in self.columns.iter().enumerate() {
            let comma = if i != column_count { ", " } else { "" };
            res += format!("{table}.{n} as {prefix}{n}{comma}", table = self.table).as_str();
        }
        res
    }
}

/// Creates an update statement from provided update type. The generated update query can be passed to [sqlx::query()]
///
/// Example
/// ```
/// use jenny_persistence::UpdateStatementBuilder;
/// use serde::{Serialize};
/// use serde_json::Value;
///
/// #[derive(Serialize)]
/// struct Example {
///     name: String
/// }
///
/// let example_data = Example { name: format!("updated name")};
/// let update_builder = UpdateStatementBuilder::new(&example_data, "table1").unwrap();
/// assert_eq!("UPDATE table1 SET name = $1 WHERE id = $2 RETURNING *", update_builder.query());
/// assert!(update_builder.data_map().contains_key("name"));
/// ```
pub struct UpdateStatementBuilder {
    query: String,
    data_map: Map<String, Value>,
}

impl UpdateStatementBuilder {
    pub fn new(data: &impl Serialize, table: &str) -> Result<Self, super::error::MappingError> {
        let data_map = serde_json::from_value::<Map<String, Value>>(serde_json::to_value(data)?)?;
        let column_binds = data_map
            .keys()
            .enumerate()
            .map(|(i, k)| format!("{} = ${}", k, i + 1))
            .collect::<Vec<String>>()
            .join(",");
        let id_arg_pos = data_map.len() + 1;
        let sql_template =
            format!("UPDATE {table} SET {column_binds} WHERE id = ${id_arg_pos} RETURNING *");
        Ok(Self {
            query: sql_template,
            data_map,
        })
    }

    /// Get a reference to the update builder's query.
    #[must_use]
    pub fn query(&self) -> &str {
        self.query.as_ref()
    }

    /// Get a reference to the update statement builder's data map.
    #[must_use]
    pub fn data_map(&self) -> &Map<String, Value> {
        &self.data_map
    }
}
