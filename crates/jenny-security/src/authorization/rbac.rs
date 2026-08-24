//! The RBAC rule model: resources, roles, and the JSON they are loaded from.
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Debug,
    fs::File,
    hash::Hash,
    io::{BufReader, Read},
    path::Path,
};
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum RbacError {
    #[error("Error in RBAC specification: {0}")]
    SpecificationError(String),
}

/// A representation of an RBAC protected resource
///
/// Example
/// ```
/// use jenny_security::RbacResourceSpec;
///
/// let spec = RbacResourceSpec::new("SomeApiResource", "/api/some-resource*").unwrap();
/// ```
#[derive(Clone, Default, Debug, PartialEq, Eq, PartialOrd, Deserialize, Hash, Ord)]
pub struct RbacResourceSpec<Ident> {
    name: String,
    ident: Ident,
}

impl<Ident: Clone> RbacResourceSpec<Ident> {
    /// Get name of resource spec
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Create a new resource spec from a name and some identifying property
    pub fn new<I: Into<String>>(name: I, ident: Ident) -> Result<Self, RbacError> {
        Ok(Self {
            name: name.into(),
            ident,
        })
    }

    pub fn ident_as<T>(&self) -> T
    where
        T: From<Ident>,
    {
        T::from(self.ident.clone())
    }

    pub fn ident(&self) -> &Ident {
        &self.ident
    }
}

/// Represents a group or group of groups that permissions can be mapped against.
/// HTTP methods are stored as uppercase strings (e.g. `"GET"`, `"POST"`) to avoid
/// any dependency on a specific HTTP crate version.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RbacRoleSpec<Ident> {
    name: String,
    accessible_resources: HashMap<String, BTreeSet<Ident>>,
}

impl<Ident: Default + Ord + Clone + DeserializeOwned> RbacRoleSpec<Ident> {
    /// Create new spec with `name` and an empty resource map
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Update role spec from [RbacResourceSpec] and HTTP method strings.
    /// Method strings are normalised to uppercase before storage.
    pub fn update_accessible_resources<M: AsRef<str>>(
        mut self,
        resource_spec: RbacResourceSpec<Ident>,
        permissions: &[M],
    ) -> Self {
        for perm in permissions {
            self.accessible_resources
                .entry(perm.as_ref().to_ascii_uppercase())
                .and_modify(|e| {
                    e.insert(resource_spec.ident.clone());
                })
                .or_default()
                .insert(resource_spec.ident.clone());
        }
        self
    }

    /// Merge another role spec into the existing one
    pub fn inherit_from_role(mut self, role: &RbacRoleSpec<Ident>) -> Self {
        for (method, perms) in &role.accessible_resources {
            self.accessible_resources
                .entry(method.clone())
                .or_default()
                .extend(perms.clone());
        }
        self
    }

    /// Get role name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get resources accessible by role, keyed by uppercase HTTP method string
    pub fn accessible_resources(&self) -> &HashMap<String, BTreeSet<Ident>> {
        &self.accessible_resources
    }
}

/// Top level rbac configuration providing all necessary access to handled resources and available roles.
/// ```
/// use std::io::{BufReader, Read};
/// use jenny_security::RbacConfig;
///
/// let mut spec_reader = BufReader::new(include_str!("../../../../docs/data/sample_rbac_config.json").as_bytes());
/// let rbac_config = RbacConfig::<String>::load_from_reader(&mut spec_reader);
/// assert!(rbac_config.is_ok());
/// ```
#[derive(Clone, Debug, Default)]
pub struct RbacConfig<Ident>(HashMap<String, RbacRoleSpec<Ident>>);

impl<Ident: DeserializeOwned + Default + Ord + Clone> RbacConfig<Ident> {
    /// Manually extend the role table
    pub fn add_role(mut self, role_spec: RbacRoleSpec<Ident>) -> Self {
        self.0
            .entry(role_spec.name().to_string())
            .and_modify(|existing| *existing = existing.clone().inherit_from_role(&role_spec))
            .or_insert(role_spec);
        self
    }

    /// Attempt to load an rbac config from `path`
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let file = File::open(path)?;
        let mut rdr = BufReader::new(file);
        RbacConfig::load_from_reader(&mut rdr)
    }

    /// Parse rbac config from `reader`
    pub fn load_from_reader(reader: &mut impl Read) -> Result<Self, std::io::Error> {
        let i_config: RbacIntermediateConfig<Ident> =
            serde_json::from_reader::<_, RbacIntermediateConfig<Ident>>(reader.by_ref())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        Ok(Self::from(i_config))
    }

    /// Get roles provided by this config
    pub fn roles(&self) -> &HashMap<String, RbacRoleSpec<Ident>> {
        &self.0
    }

    fn from(data: RbacIntermediateConfig<Ident>) -> RbacConfig<Ident> {
        let mut spec_table = HashMap::new();
        for role_definition in &data.roles {
            for role in role_definition {
                let mut spec = RbacRoleSpec::new(role.0);
                let access = &role.1.resource_access;
                for (name, perms) in access {
                    if let Some(res_spec) = data.resources.iter().find(|res| name == res.name()) {
                        spec = spec.update_accessible_resources(res_spec.clone(), perms);
                    }
                }
                spec_table.insert(spec.name.clone(), spec);
            }
        }
        for role_definition in data.roles {
            for role in role_definition {
                if let Some(inheritances) = role.1.inherited_roles {
                    for inheritance in inheritances {
                        if let Some(inherited_spec) = spec_table.get(inheritance.as_str()).cloned()
                            && let Some(inheriting_spec) = spec_table.get(&role.0)
                        {
                            spec_table.insert(
                                role.0.clone(),
                                inheriting_spec.clone().inherit_from_role(&inherited_spec),
                            );
                        };
                    }
                }
            }
        }
        RbacConfig(spec_table)
    }
}

/// Used for deserialization of config file
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RbacRoleConfig {
    #[serde(deserialize_with = "RbacRoleConfig::deserialize_resource_access")]
    resource_access: HashMap<String, Vec<String>>,
    inherited_roles: Option<Vec<String>>,
}

impl RbacRoleConfig {
    fn deserialize_resource_access<'de, D>(
        deserializer: D,
    ) -> Result<HashMap<String, Vec<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let buf: HashMap<String, Vec<String>> = HashMap::deserialize(deserializer)?;
        let mut new_map: HashMap<String, Vec<String>> = HashMap::new();
        for (name, permissions) in buf {
            let normalised: Vec<String> = permissions
                .into_iter()
                .filter_map(|perm| {
                    let upper = perm.to_ascii_uppercase();
                    let known = matches!(
                        upper.as_str(),
                        "GET"
                            | "POST"
                            | "PUT"
                            | "PATCH"
                            | "DELETE"
                            | "HEAD"
                            | "OPTIONS"
                            | "TRACE"
                            | "CONNECT"
                    );
                    if !known {
                        warn!("Unrecognised HTTP method in RBAC config: {perm}. Ignoring...");
                        None
                    } else {
                        Some(upper)
                    }
                })
                .collect();
            new_map.insert(name, normalised);
        }
        Ok(new_map)
    }
}

/// Used for deserialization of config file
#[derive(Debug, Deserialize)]
struct RbacIntermediateConfig<Ident> {
    resources: Vec<RbacResourceSpec<Ident>>,
    roles: Vec<HashMap<String, RbacRoleConfig>>,
}

#[cfg(test)]
mod tests {

    use glob::Pattern;

    use super::*;

    const ANY_METHODS: [&str; 5] = ["GET", "PATCH", "POST", "PUT", "DELETE"];
    const CREATE_METHODS: [&str; 1] = ["POST"];
    const READ_METHODS: [&str; 1] = ["GET"];
    const UPDATE_METHODS: [&str; 3] = ["PATCH", "POST", "PUT"];

    impl From<glob::PatternError> for RbacError {
        fn from(value: glob::PatternError) -> Self {
            RbacError::SpecificationError(value.to_string())
        }
    }

    pub trait AccessChecker<Ident> {
        type Request;

        fn check_resource_access(
            role: &RbacRoleSpec<Ident>,
            requested: &Self::Request,
        ) -> Result<bool, RbacError>
        where
            Ident: Ord + Clone + Default;
    }

    #[test]
    fn test_rbac_config_parsing() {
        let spec_data = r#"{
            "resources": [
                {"name": "ApiFull", "ident": "/api/**"},
                {"name": "Resource01", "ident": "/api/resource01/*"},
                {"name": "Resource02", "ident": "/api/resource02*"}
            ],
            "roles": [
                {
                    "Admin": {
                        "resourceAccess": {"ApiFull": ["GET","POST","PUT","PATCH","DELETE"]}
                    }
                },
                {
                    "Role01": {
                        "resourceAccess": {"Resource01": ["GET","PATCH","PUT","POST"]}
                    }
                },
                {
                    "Role02": {
                        "resourceAccess": {
                            "Resource02": ["GET","DELETE"],
                            "Resource01": [
                                "DELETE"
                            ]
                        },
                        "inheritedRoles": [
                            "Role01"
                        ]
                    }
                }
            ]
        }"#;

        let mut buf = BufReader::new(spec_data.as_bytes());
        let parsed_rbac_config = RbacConfig::<String>::load_from_reader(&mut buf);
        assert!(parsed_rbac_config.is_ok());

        let rbac_config = parsed_rbac_config.unwrap();
        assert!(rbac_config.roles().len() == 3);
        for (name, role) in rbac_config.roles() {
            match name.as_str() {
                "Admin" => {
                    assert!(role.accessible_resources.len() == 5);
                    for perm in ANY_METHODS {
                        assert!(
                            role.accessible_resources()
                                .get(perm)
                                .unwrap()
                                .contains("/api/**")
                        )
                    }
                }
                "Role01" => {
                    assert!(role.accessible_resources.len() == 4);
                    assert!(
                        role.accessible_resources()
                            .get("GET")
                            .unwrap()
                            .contains("/api/resource01/*")
                    );
                    for perm in UPDATE_METHODS {
                        assert!(
                            role.accessible_resources()
                                .get(perm)
                                .unwrap()
                                .contains("/api/resource01/*")
                        );
                    }
                }
                "Role02" => {
                    assert!(role.accessible_resources.len() == 5);
                    assert!(
                        role.accessible_resources()
                            .get("DELETE")
                            .unwrap()
                            .contains("/api/resource02*")
                    );
                    assert!(
                        role.accessible_resources()
                            .get("GET")
                            .unwrap()
                            .contains("/api/resource02*")
                    );
                    for perm in ANY_METHODS {
                        assert!(
                            role.accessible_resources()
                                .get(perm)
                                .unwrap()
                                .contains("/api/resource01/*")
                        )
                    }
                }
                _ => panic!(),
            }
        }
    }

    #[test]
    fn test_rbac_role_spec_functionality() {
        struct RbacValidator;
        impl AccessChecker<String> for RbacValidator {
            type Request = (String, &'static str);

            fn check_resource_access(
                role: &RbacRoleSpec<String>,
                requested: &Self::Request,
            ) -> Result<bool, RbacError> {
                if let Some(patterns) = role.accessible_resources.get(requested.1) {
                    for pattern in patterns {
                        let check = Pattern::new(pattern)?;
                        if check.matches(&requested.0) {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
        }

        let full_api_spec = RbacResourceSpec::new("ApiFull", "/api/**".to_string()).unwrap();
        let some_api_resource_collection_spec =
            RbacResourceSpec::new("SomeResource", "/api/some-resource*".to_string()).unwrap();
        let delete_some_api_resource_collection_item_spec = RbacResourceSpec::new(
            "SomeResourceDeletion",
            "/api/some-resource/*:delete".to_string(),
        )
        .unwrap();

        let super_user_group = RbacRoleSpec::new("Admin")
            .update_accessible_resources(full_api_spec, &["GET", "POST", "PUT", "PATCH", "DELETE"]);

        let resource_editor_group = RbacRoleSpec::new("ResourceEditor")
            .update_accessible_resources(
                some_api_resource_collection_spec,
                &["GET", "POST", "PATCH", "PUT"],
            );
        let resource_manager_group = RbacRoleSpec::new("ResourceManager")
            .update_accessible_resources(delete_some_api_resource_collection_item_spec, &["DELETE"])
            .inherit_from_role(&resource_editor_group);

        let editor_urls = &[
            "/api/some-resource".to_string(),
            "/api/some-resource/".to_string(),
            "/api/some-resource/12345".to_string(),
        ];
        let add_manager_urls = &["/api/some-resource/12345:delete".to_string()];
        let add_admin_urls = &[
            "/api/some-other-resource".to_string(),
            "/api/some-other-resource/12345".to_string(),
        ];
        let mut admin_urls = Vec::from(editor_urls);
        admin_urls.extend_from_slice(add_admin_urls);
        admin_urls.extend_from_slice(add_manager_urls);

        for method in ANY_METHODS {
            for path in admin_urls.clone() {
                assert!(
                    RbacValidator::check_resource_access(&super_user_group, &(path, method))
                        .unwrap()
                );
            }
        }

        let mut editor_methods = Vec::from(CREATE_METHODS);
        editor_methods.extend_from_slice(&UPDATE_METHODS);
        editor_methods.extend_from_slice(&READ_METHODS);
        for method in &editor_methods {
            for path in editor_urls {
                assert!(
                    RbacValidator::check_resource_access(
                        &resource_editor_group,
                        &(path.clone(), method)
                    )
                    .unwrap()
                );
            }
        }

        let mut manager_methods = editor_methods.clone();
        manager_methods.push("DELETE");
        for method in &manager_methods {
            for path in add_manager_urls {
                assert!(
                    RbacValidator::check_resource_access(
                        &resource_manager_group,
                        &(path.clone(), method)
                    )
                    .unwrap()
                );
            }
        }
    }

    #[test]
    fn test_rbac_resource_spec() {
        let spec_data = r#"{"name": "ApiFull", "ident": "/api/**"}"#;

        let spec: RbacResourceSpec<String> = serde_json::from_str(spec_data).unwrap();

        assert_eq!(spec.name(), "ApiFull");
        assert_eq!(spec.ident().clone(), "/api/**".to_string());
    }
}
