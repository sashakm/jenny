pub(crate) fn apply_casing(casing: &str, value: &str) -> Result<String, darling::Error> {
    let renamed = match casing {
        "camelCase" => stringcase::camel_case(value),
        "kebab-case" => stringcase::kebab_case(value).to_lowercase(),
        "snake_case" => stringcase::snake_case(value).to_lowercase(),
        "lowercase" => value.split('_').collect::<String>().to_lowercase(),
        "UPPERCASE" => value.split('_').collect::<String>().to_uppercase(),
        "PascalCase" => stringcase::pascal_case(value),
        "SCREAMING_SNAKE_CASE" => value.to_uppercase(),
        _ => return Err(darling::Error::custom("Unsupported renaming pattern")),
    };
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use crate::utils::apply_casing;

    #[test]
    fn test_apply_casing() {
        let base = "frank_nord";
        let cases = [
            ("camelCase", "frankNord"),
            ("kebab-case", "frank-nord"),
            ("snake_case", "frank_nord"),
            ("lowercase", "franknord"),
            ("UPPERCASE", "FRANKNORD"),
            ("PascalCase", "FrankNord"),
            ("SCREAMING_SNAKE_CASE", "FRANK_NORD"),
        ];
        for case in cases {
            assert_eq!(apply_casing(case.0, base).unwrap(), case.1)
        }
    }
}
