/// Create an sql record from a database entity representation for use in select statements.
/// This can be especially useful in combination with entities implementing [sqlx::Type].
/// When deriving this trait, the following attributes are available:
///
/// ### Container attributes:
/// `query(rename_all = "casing")`: change the casing of all field names. Functionally
/// equivalent to the rename_all attribute provided by sqlx. Options are `lowercase`,
/// `UPPERCASE`, `camelCase`, `PascalCase`, `SCREAMING_SNAKE_CASE` and `kebab-case`
///
/// `query(table = "name")`: set the name of the table to query for this entity. Can be
/// overridden/aliased where applicable.
///
/// ### Field attributes:
/// `query(rename = "name")`: Change the name by which this field is represented in queries.
///
/// `query(exclude)`: Disregard this field entirely. Unlike sqlx(skip), which more
/// or less causes `FromRow` to set the field to its types default value, this
/// will simply cause the field to not be included in the field list.
///
/// `query(exclude_insert)`: Do not include these fields in the generated insert statement.
///
/// `query(foreign_key(table = "foreign table", column = "foreign column", alias = "foreign table alias"))`:
/// Indicate that this field holds a foreign key to be made available to the join mapper and should not be
/// included when constructing the row representation.
///
/// ```ignore
/// use jenny_persistence::QueryFormat;
///
/// #[derive(Type, QueryFormat)]
/// #[query(rename_all = "UPPERCASE")]
/// #[query(table = "my_entity")]
/// struct MyEntity {
///     id: i32,
///     #[query(rename = "foo")]
///     name: String,
///     #[query(exclude)]
///     data: Vec<u8>
///     #[query(foreign_key(table = "foo", column = "id", alias = "f"))]
///     foo_id: i32
/// }
///
/// ```
pub trait QueryFormat {
    const ENTITY_FIELDS: &'static [&'static str];

    /// Format entity fields as comma separated list
    fn row_format(table: &str) -> String {
        Self::ENTITY_FIELDS
            .iter()
            .fold(Vec::new(), |mut agg, r| {
                agg.push(format!("{table}.{r}"));
                agg
            })
            .join(", ")
    }

    /// Format entity fields as composite row
    fn as_record(table: &str, alias: &str) -> String {
        format!("({}) AS {alias}", Self::row_format(table))
    }

    /// Format entity as composite row without alias
    fn as_embedded_record(table: &str) -> String {
        format!("({})", Self::row_format(table))
    }

    /// Format entity fields as composite row or `null` if `check_column` is null
    fn as_optional_record(table: &str, alias: &str, check_column: &str) -> String {
        format!(
            "CASE WHEN {check_column} IS NULL THEN NULL ELSE ({}) END AS {alias}",
            Self::row_format(table)
        )
    }

    /// Format entity as optional composite row without alias
    fn as_embedded_optional_record(table: &str, check_column: &str) -> String {
        format!(
            "CASE WHEN {check_column} IS NULL THEN NULL ELSE ({}) END",
            Self::row_format(table)
        )
    }

    fn insert_query() -> &'static str;

    /// Format FROM and JOIN clauses for the query
    fn table_expression() -> &'static str {
        ""
    }
}

/// Extension trait for [QueryFormat] adding functionality to
/// support more complex queries.
pub trait QueryFormatExt: QueryFormat {
    /// Replace entity fields with composite rows
    fn as_record_with(table: &str, alias: &str, embeds: &[(&str, String)]) -> String {
        embeds
            .iter()
            .fold(Self::as_record(table, alias), |mut agg, e| {
                agg = agg.replace(e.0, &e.1);
                agg
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_record_trait_impl() {
        #[allow(dead_code)]
        struct Foo {
            bar: String,
            baz_id: i32,
            fnord: bool,
        }

        impl QueryFormat for Foo {
            const ENTITY_FIELDS: &'static [&'static str] = &["bar", "fnord", "baz_id"];

            fn insert_query() -> &'static str {
                "INSERT INTO foo (bar, baz_id, fnord) VALUES ($1, $2, $3)"
            }
        }

        impl QueryFormatExt for Foo {}
        #[allow(dead_code)]
        struct Blarn {
            field1: i32,
            field2: Vec<u8>,
        }

        impl QueryFormat for Blarn {
            const ENTITY_FIELDS: &'static [&'static str] = &["field1", "field2"];

            fn insert_query() -> &'static str {
                ""
            }
        }

        let record = Foo::as_record_with(
            "ft",
            "footable",
            &[("ft.bar", Blarn::as_embedded_record("extra"))],
        );
        assert_eq!(
            record,
            "((extra.field1, extra.field2), ft.fnord, ft.baz_id) AS footable"
        );

        assert_eq!(
            Foo::as_record("ft", "foo"),
            "(ft.bar, ft.fnord, ft.baz_id) AS foo"
        );

        assert_eq!(
            Foo::as_optional_record("ft", "foo", "ft.bar"),
            "CASE WHEN ft.bar IS NULL THEN NULL ELSE (ft.bar, ft.fnord, ft.baz_id) END AS foo"
        );
    }
}
