//! Where each of the framework's endpoint groups is served (ADR-020).
//!
//! [`resolve`] turns `server.endpoints` and `server.management` into a [`Layout`]: for
//! every group that is enabled and available, its listener, its path and whether its
//! requests are traced. Everything that can be wrong with that is a startup error
//! naming the key at fault, raised before anything binds.

use crate::config::{EndpointConfig, HttpConfig, Listener};

/// One of the framework's endpoint groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    Probes,
    Status,
    Metrics,
    Openapi,
    Swagger,
    PublicStatus,
}

impl Group {
    const ALL: [Group; 6] = [
        Group::Probes,
        Group::Status,
        Group::Metrics,
        Group::Openapi,
        Group::Swagger,
        Group::PublicStatus,
    ];

    fn key(self) -> &'static str {
        match self {
            Group::Probes => "probes",
            Group::Status => "status",
            Group::Metrics => "metrics",
            Group::Openapi => "openapi",
            Group::Swagger => "swagger",
            Group::PublicStatus => "public_status",
        }
    }

    /// Appended to `server.framework_path` unless the group has a `path` of its own.
    fn default_path(self) -> &'static str {
        match self {
            Group::Probes => "/health",
            Group::Status => "/health/status",
            Group::Metrics => "/metrics",
            Group::Openapi => "/openapi.json",
            Group::Swagger => "/swagger",
            Group::PublicStatus => "/health/public",
        }
    }

    /// Whether `path` is a prefix the group serves under, rather than its one route.
    fn is_prefix(self) -> bool {
        matches!(self, Group::Probes | Group::Swagger)
    }

    fn config(self, server: &HttpConfig) -> &EndpointConfig {
        let endpoints = &server.endpoints;
        match self {
            Group::Probes => &endpoints.probes,
            Group::Status => &endpoints.status,
            Group::Metrics => &endpoints.metrics.endpoint,
            Group::Openapi => &endpoints.openapi,
            Group::Swagger => &endpoints.swagger,
            Group::PublicStatus => &endpoints.public_status,
        }
    }
}

/// Where a group is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub listener: Listener,
    /// The route for a single-route group; the prefix (possibly empty) for `probes` and
    /// `swagger`.
    pub path: String,
    pub log_requests: bool,
}

/// What this build and this service can serve at all, whatever the configuration says.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Available {
    /// `telemetry-prometheus`, with a registry.
    pub metrics: bool,
    /// `openapi`, with a document from at least one adapter.
    pub openapi: bool,
    /// `swagger-ui` compiled in.
    pub swagger: bool,
}

/// A REST adapter's claim on the API listener's paths.
///
/// Mounted at a path, it claims everything under it. Mounted at the root, it claims its
/// route templates by string, so a group at `/status` beside an adapter's `/{slug}` is
/// not a clash: the static route wins, and that one value of `slug` is shadowed.
pub(crate) struct AdapterRoutes {
    pub name: String,
    /// Its mount path; empty when mounted at the root.
    pub mount: String,
    /// Every route template, mount path included.
    pub routes: Vec<String>,
}

/// The placement of every group that is served.
#[derive(Debug, Default)]
pub(crate) struct Layout(Vec<(Group, Placement)>);

impl Layout {
    pub fn get(&self, group: Group) -> Option<&Placement> {
        self.0.iter().find(|(g, _)| *g == group).map(|(_, p)| p)
    }
}

/// The configured key of a group: `.path` when the path came from there, else the
/// group and `server.framework_path`, which its default path is under.
fn key_of(group: Group, server: &HttpConfig) -> String {
    if group.config(server).path.is_some() {
        format!("`server.endpoints.{}.path`", group.key())
    } else {
        format!(
            "`server.endpoints.{}` (at its default path under `server.framework_path`)",
            group.key()
        )
    }
}

/// Leading slash on, trailing slash off. A single route keeps `/` for the root; a
/// prefix at the root is empty.
fn normalize(path: &str, prefix: bool) -> String {
    let trimmed = path.trim().trim_end_matches('/');
    let path = if trimmed.starts_with('/') || trimmed.is_empty() {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    if path.is_empty() && !prefix {
        "/".to_string()
    } else {
        path
    }
}

pub(crate) fn resolve(
    server: &HttpConfig,
    available: Available,
    adapters: &[AdapterRoutes],
) -> anyhow::Result<Layout> {
    let framework_path = normalize(&server.framework_path, true);
    let managed = server.management.is_some();

    let mut placements = Vec::new();
    let listener_of = |group: Group| -> anyhow::Result<Listener> {
        let config = group.config(server);
        Ok(match (config.listener, managed) {
            (Some(Listener::Management), false) => anyhow::bail!(
                "`server.endpoints.{}.listener` is `management`, but `server.management` is \
                 not set: set `server.management.address`, or serve the group on `api`",
                group.key()
            ),
            (Some(listener), _) => listener,
            (None, true) => Listener::Management,
            (None, false) => Listener::Api,
        })
    };
    for group in Group::ALL {
        let config = group.config(server);
        if !config.enabled {
            continue;
        }
        // Unset, Swagger UI goes where its document is.
        let listener = match (group, config.listener) {
            (Group::Swagger, None) => listener_of(Group::Openapi)?,
            _ => listener_of(group)?,
        };
        let path = match config.path.as_deref() {
            Some(path) => {
                // axum reads these as parameters, or panics on them.
                if path.contains(['{', '}', '*'])
                    || path.contains("//")
                    || path.split('/').any(|segment| segment.starts_with(':'))
                {
                    anyhow::bail!(
                        "`server.endpoints.{}.path` (`{path}`) must be a literal path, \
                         without `{{…}}`, `*`, `:` segments or `//`",
                        group.key()
                    );
                }
                let path = normalize(path, group.is_prefix());
                // Everything under the root would include the document it loads.
                if group == Group::Swagger && path.is_empty() {
                    anyhow::bail!(
                        "`server.endpoints.swagger.path` cannot be `/`: Swagger UI would \
                         claim every path, the `openapi` document's among them"
                    );
                }
                path
            }
            None => normalize(
                &format!("{framework_path}{}", group.default_path()),
                group.is_prefix(),
            ),
        };
        placements.push((
            group,
            Placement {
                listener,
                path,
                log_requests: config.log_requests,
            },
        ));
    }

    // Swagger UI loads the document from the `openapi` group, so it follows that group:
    // off when it is off, and where it is unless placed explicitly, which then has to
    // be the same listener.
    if available.swagger
        && let Some((_, swagger)) = placements.iter().find(|(g, _)| *g == Group::Swagger)
    {
        match placements.iter().find(|(g, _)| *g == Group::Openapi) {
            None => {
                tracing::warn!(
                    "`server.endpoints.swagger` is enabled but `server.endpoints.openapi` is \
                     not; Swagger UI is not served, as it loads the document from there"
                );
                placements.retain(|(g, _)| *g != Group::Swagger);
            }
            Some((_, openapi)) if openapi.listener != swagger.listener => anyhow::bail!(
                "`server.endpoints.swagger.listener` is `{}` but \
                 `server.endpoints.openapi.listener` is `{}`: Swagger UI loads the \
                 document from the same listener",
                swagger.listener,
                openapi.listener
            ),
            Some(_) => {}
        }
    }

    placements.retain(|(group, _)| match group {
        Group::Metrics => available.metrics,
        Group::Openapi => available.openapi,
        // Nothing to show without a document.
        Group::Swagger => available.swagger && available.openapi,
        _ => true,
    });

    check_clashes(server, &placements, adapters)?;
    Ok(Layout(placements))
}

/// A path someone serves: one route, or everything under a prefix.
struct Claim {
    path: String,
    prefix: bool,
    owner: String,
    listener: Listener,
}

/// Whether the prefix `prefix` covers `path`, on segment boundaries.
fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl Claim {
    fn clashes_with(&self, other: &Claim) -> bool {
        self.listener == other.listener
            && match (self.prefix, other.prefix) {
                (false, false) => self.path == other.path,
                (true, false) => covers(&self.path, &other.path),
                (false, true) => covers(&other.path, &self.path),
                (true, true) => covers(&self.path, &other.path) || covers(&other.path, &self.path),
            }
    }

    fn describe(&self) -> String {
        if self.prefix {
            format!("{} (everything under `{}`)", self.owner, self.path)
        } else {
            format!("{} (`{}`)", self.owner, self.path)
        }
    }
}

fn check_clashes(
    server: &HttpConfig,
    placements: &[(Group, Placement)],
    adapters: &[AdapterRoutes],
) -> anyhow::Result<()> {
    let mut claims = Vec::new();
    for (group, placement) in placements {
        let owner = key_of(*group, server);
        let mut claim = |path: String, prefix: bool| {
            claims.push(Claim {
                path,
                prefix,
                owner: owner.clone(),
                listener: placement.listener,
            })
        };
        match group {
            Group::Probes => {
                claim(format!("{}/live", placement.path), false);
                claim(format!("{}/ready", placement.path), false);
            }
            Group::Swagger => claim(placement.path.clone(), true),
            _ => claim(placement.path.clone(), false),
        }
    }
    let groups = claims.len();
    for adapter in adapters {
        let owner = format!("REST adapter `{}`", adapter.name);
        if adapter.mount.is_empty() {
            // At the root, an adapter claims its own routes only.
            claims.extend(adapter.routes.iter().map(|route| Claim {
                path: route.clone(),
                prefix: false,
                owner: owner.clone(),
                listener: Listener::Api,
            }));
        } else {
            claims.push(Claim {
                path: adapter.mount.clone(),
                prefix: true,
                owner,
                listener: Listener::Api,
            });
        }
    }

    // Only a group can be at fault: adapters are checked against each other already.
    for (index, claim) in claims[..groups].iter().enumerate() {
        if let Some(other) = claims
            .iter()
            .enumerate()
            .find(|(other, c)| *other != index && c.owner != claim.owner && claim.clashes_with(c))
            .map(|(_, c)| c)
        {
            anyhow::bail!(
                "{} clashes with {} on the {} listener; give one of them another `path`",
                claim.describe(),
                other.describe(),
                claim.listener
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ManagementConfig;

    const EVERYTHING: Available = Available {
        metrics: true,
        openapi: true,
        swagger: true,
    };

    fn managed() -> HttpConfig {
        HttpConfig {
            management: Some(ManagementConfig {
                address: "127.0.0.1:0".into(),
            }),
            ..HttpConfig::default()
        }
    }

    fn notes_at(mount: &str) -> AdapterRoutes {
        AdapterRoutes {
            name: "my-notes".into(),
            mount: mount.into(),
            routes: vec![format!("{mount}/notes")],
        }
    }

    fn placed(listener: Listener, path: &str, log_requests: bool) -> Placement {
        Placement {
            listener,
            path: path.into(),
            log_requests,
        }
    }

    #[test]
    fn without_management_every_group_defaults_to_the_api_listener() {
        let layout = resolve(&HttpConfig::default(), EVERYTHING, &[notes_at("/api")]).unwrap();
        use Listener::Api;
        assert_eq!(
            layout.get(Group::Probes),
            Some(&placed(Api, "/j/health", false))
        );
        assert_eq!(
            layout.get(Group::Status),
            Some(&placed(Api, "/j/health/status", false))
        );
        assert_eq!(
            layout.get(Group::Metrics),
            Some(&placed(Api, "/j/metrics", false))
        );
        assert_eq!(
            layout.get(Group::Openapi),
            Some(&placed(Api, "/j/openapi.json", true))
        );
        assert_eq!(layout.get(Group::Swagger), None, "off by default");
        assert_eq!(layout.get(Group::PublicStatus), None, "off by default");
    }

    #[test]
    fn with_management_every_group_defaults_to_it_at_the_same_path() {
        let layout = resolve(&managed(), EVERYTHING, &[]).unwrap();
        for group in [Group::Probes, Group::Status, Group::Metrics, Group::Openapi] {
            assert_eq!(
                layout.get(group).unwrap().listener,
                Listener::Management,
                "{group:?}"
            );
        }
        assert_eq!(layout.get(Group::Probes).unwrap().path, "/j/health");
    }

    #[test]
    fn a_path_replaces_the_default_and_the_framework_path_moves_the_defaults() {
        let mut server = managed();
        server.framework_path = "/_internal/".into();
        server.endpoints.public_status.enabled = true;
        server.endpoints.public_status.listener = Some(Listener::Api);
        server.endpoints.public_status.path = Some("status/".into());
        server.endpoints.probes.path = Some("/".into());
        let layout = resolve(&server, EVERYTHING, &[notes_at("/api")]).unwrap();
        assert_eq!(
            layout.get(Group::PublicStatus),
            Some(&placed(Listener::Api, "/status", false))
        );
        assert_eq!(
            layout.get(Group::Probes).unwrap().path,
            "",
            "probes at /live"
        );
        assert_eq!(
            layout.get(Group::Status).unwrap().path,
            "/_internal/health/status"
        );
    }

    #[test]
    fn what_this_build_cannot_serve_is_left_out() {
        let layout = resolve(
            &HttpConfig::default(),
            Available {
                metrics: false,
                openapi: false,
                swagger: true,
            },
            &[],
        )
        .unwrap();
        assert!(layout.get(Group::Metrics).is_none());
        assert!(layout.get(Group::Openapi).is_none());
        assert!(layout.get(Group::Swagger).is_none(), "no document to show");
        assert!(layout.get(Group::Probes).is_some());
    }

    #[test]
    fn the_management_listener_must_exist_to_be_named() {
        let mut server = HttpConfig::default();
        server.endpoints.metrics.endpoint.listener = Some(Listener::Management);
        let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
        assert!(err.contains("`server.endpoints.metrics.listener`"), "{err}");
        assert!(err.contains("`server.management`"), "{err}");

        // A disabled group's listener does not matter.
        server.endpoints.metrics.endpoint.enabled = false;
        assert!(resolve(&server, EVERYTHING, &[]).is_ok());
    }

    #[test]
    fn two_groups_on_one_path_are_refused() {
        let mut server = HttpConfig::default();
        server.endpoints.public_status.enabled = true;
        server.endpoints.public_status.path = Some("/j/health/status".into());
        let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
        assert!(err.contains("`server.endpoints.status`"), "{err}");
        assert!(
            err.contains("`server.endpoints.public_status.path`"),
            "{err}"
        );
        assert!(err.contains("api listener"), "{err}");

        // On different listeners they do not clash.
        let mut server = managed();
        server.endpoints.public_status.enabled = true;
        server.endpoints.public_status.listener = Some(Listener::Api);
        server.endpoints.public_status.path = Some("/j/health/status".into());
        assert!(resolve(&server, EVERYTHING, &[]).is_ok());
    }

    #[test]
    fn a_route_under_another_groups_prefix_is_refused() {
        let mut server = with_swagger(HttpConfig::default());
        server.endpoints.metrics.endpoint.path = Some("/j/swagger/metrics".into());
        let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
        assert!(err.contains("`server.endpoints.metrics.path`"), "{err}");
        assert!(err.contains("everything under `/j/swagger`"), "{err}");
    }

    #[test]
    fn a_group_inside_an_adapters_mount_is_refused() {
        let mut server = HttpConfig::default();
        server.endpoints.openapi.path = Some("/api/openapi.json".into());
        let err = resolve(&server, EVERYTHING, &[notes_at("/api")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`server.endpoints.openapi.path`"), "{err}");
        assert!(err.contains("REST adapter `my-notes`"), "{err}");

        // On the management listener the API's paths are free.
        let mut server = managed();
        server.endpoints.openapi.path = Some("/api/openapi.json".into());
        server.endpoints.swagger.path = Some("/api/swagger".into());
        assert!(resolve(&server, EVERYTHING, &[notes_at("/api")]).is_ok());
    }

    #[test]
    fn at_the_root_an_adapter_claims_only_its_routes() {
        let root = notes_at("");
        assert!(
            resolve(
                &HttpConfig::default(),
                EVERYTHING,
                std::slice::from_ref(&root)
            )
            .is_ok()
        );

        let mut server = HttpConfig::default();
        server.endpoints.status.path = Some("/notes".into());
        let err = resolve(&server, EVERYTHING, &[root])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`server.endpoints.status.path` (`/notes`)"),
            "{err}"
        );
    }

    /// Every group's defaults, Swagger UI switched on.
    fn with_swagger(mut server: HttpConfig) -> HttpConfig {
        server.endpoints.swagger.enabled = true;
        server
    }

    #[test]
    fn swagger_is_off_until_enabled() {
        let layout = resolve(&HttpConfig::default(), EVERYTHING, &[]).unwrap();
        assert!(layout.get(Group::Swagger).is_none());
    }

    #[test]
    fn swagger_follows_the_openapi_group() {
        // Off while `openapi` is off, without stopping the service.
        let mut server = with_swagger(HttpConfig::default());
        server.endpoints.openapi.enabled = false;
        let layout = resolve(&server, EVERYTHING, &[]).unwrap();
        assert!(layout.get(Group::Swagger).is_none());
        assert!(layout.get(Group::Openapi).is_none());

        // On `openapi`'s listener unless given one.
        let mut server = with_swagger(managed());
        server.endpoints.openapi.listener = Some(Listener::Api);
        let layout = resolve(&server, EVERYTHING, &[]).unwrap();
        assert_eq!(layout.get(Group::Swagger).unwrap().listener, Listener::Api);
        assert_eq!(
            resolve(&with_swagger(managed()), EVERYTHING, &[])
                .unwrap()
                .get(Group::Swagger)
                .unwrap()
                .listener,
            Listener::Management
        );

        // Given another one explicitly, it could not load the document.
        server.endpoints.swagger.listener = Some(Listener::Management);
        let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
        assert!(err.contains("`server.endpoints.swagger.listener`"), "{err}");
        assert!(
            err.contains("`server.endpoints.openapi.listener` is `api`"),
            "{err}"
        );

        // Without `swagger-ui` compiled in there is no UI to place.
        let without_ui = Available {
            swagger: false,
            ..EVERYTHING
        };
        assert!(
            resolve(&server, without_ui, &[])
                .unwrap()
                .get(Group::Swagger)
                .is_none()
        );
    }

    #[test]
    fn a_path_must_be_literal() {
        for path in ["/status/{id}", "/:health", "/a//b", "/docs/*"] {
            let mut server = HttpConfig::default();
            server.endpoints.status.path = Some(path.into());
            let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
            assert!(
                err.contains("`server.endpoints.status.path`"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn swagger_cannot_take_the_root() {
        let mut server = with_swagger(HttpConfig::default());
        server.endpoints.swagger.path = Some("/".into());
        let err = resolve(&server, EVERYTHING, &[]).unwrap_err().to_string();
        assert!(
            err.contains("`server.endpoints.swagger.path` cannot be `/`"),
            "{err}"
        );
    }

    #[test]
    fn a_clash_at_a_default_path_names_the_framework_path() {
        let server = HttpConfig {
            framework_path: "/api".into(),
            ..HttpConfig::default()
        };
        let err = resolve(&server, EVERYTHING, &[notes_at("/api")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`server.framework_path`"), "{err}");
    }
}
