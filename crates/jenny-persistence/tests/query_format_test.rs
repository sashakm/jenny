use jenny_persistence::QueryFormat;

#[test]
fn test_formatting() {
    #[derive(QueryFormat)]
    #[query(table = "foo")]
    #[allow(dead_code)]
    struct Foo {
        field: String,
        another_field: bool,
        more_fields: i32,
    }

    assert_eq!(
        &["field", "another_field", "more_fields"],
        Foo::ENTITY_FIELDS
    );

    assert_eq!(
        Foo::as_record("ft", "foo"),
        "(ft.field, ft.another_field, ft.more_fields) AS foo"
    );
}

#[test]
fn test_attribute_handling() {
    #[derive(QueryFormat)]
    #[query(rename_all = "camelCase")]
    #[query(table = "foo")]
    #[allow(dead_code)]
    struct Foo {
        #[query(exclude)]
        field_one: String,
        #[query(rename = "foo_bar")]
        field_two: bool,
        field_three: i32,
        #[query(foreign_key(table = "table", column = "id", alias = "t"))]
        fk_id: i32,
        #[query(foreign_key(table = "other_table", column = "name"))]
        ot_name: Option<String>,
    }

    assert_eq!(
        Foo::insert_query(),
        "INSERT INTO foo (fooBar, fieldThree, fkId, otName) VALUES ($1, $2, $3, $4) RETURNING *"
    );

    assert_eq!(
        "FROM foo JOIN table t ON foo.fkId = t.id LEFT JOIN other_table ON foo.otName = other_table.name",
        Foo::table_expression()
    );
    assert_eq!(
        ["fooBar", "fieldThree", "fkId", "otName"],
        Foo::ENTITY_FIELDS
    );
}
