//! Role-based access control behind the [`Authorizer`] port.

mod rbac;

use async_trait::async_trait;
use glob::Pattern;

use jenny_core::domain::{AuthorizationError, Principal};
use jenny_core::port::outbound::Authorizer;

pub use rbac::{RbacConfig, RbacError, RbacResourceSpec, RbacRoleSpec};

/// Grants access when the principal holds a role whose rules cover this
/// `action`/`resource` pair.
///
/// - `action` is matched case-insensitively against the methods in the role's rules.
/// - `resource` is matched with glob patterns, so a rule written as
///   `/api/orders/*` covers `/api/orders/2341` and `/api/**` covers everything below
///   `/api`.
///
/// Roles are read from the principal via [`Principal::roles`], which accepts either a
/// `roles` or a `groups` claim — OIDC tokens and directory-backed stores disagree
/// about the name, and callers should not have to care which one they got.
///
/// This is the coarse gate an inbound adapter runs before dispatch. Rules that depend
/// on the *data* ("may this user edit this particular note") are domain logic and
/// belong in a domain service, so they hold on every transport.
///
/// ```
/// use jenny_core::domain::Principal;
/// use jenny_core::port::outbound::Authorizer;
/// use jenny_security::{RbacConfig, RbacResourceSpec, RbacRoleSpec};
///
/// # #[tokio::main] async fn main() {
/// let rbac = RbacConfig::default().add_role(
///     RbacRoleSpec::new("Dispatch").update_accessible_resources(
///         RbacResourceSpec::new("Orders", "/api/orders/*".to_string()).unwrap(),
///         &["GET"],
///     ),
/// );
///
/// let dispatcher = Principal::builder("u-1")
///     .with_attribute("roles", vec!["Dispatch"])
///     .build();
///
/// assert!(rbac.authorize(&dispatcher, "GET", "/api/orders/2341").await.is_ok());
/// assert!(rbac.authorize(&dispatcher, "DELETE", "/api/orders/2341").await.is_err());
/// # }
/// ```
#[async_trait]
impl Authorizer for RbacConfig<String> {
    async fn authorize(
        &self,
        principal: &Principal,
        action: &str,
        resource: &str,
    ) -> Result<(), AuthorizationError> {
        let held = principal.roles();
        if held.is_empty() {
            return Err(AuthorizationError(format!(
                "{principal} carries no roles or groups"
            )));
        }

        let action = action.to_ascii_uppercase();
        let granted = self.roles().iter().any(|(name, role)| {
            held.iter().any(|role_name| role_name == name)
                && role
                    .accessible_resources()
                    .get(&action)
                    .is_some_and(|patterns| {
                        patterns
                            .iter()
                            .filter_map(|pattern| Pattern::new(pattern).ok())
                            .any(|pattern| pattern.matches(resource))
                    })
        });

        if granted {
            Ok(())
        } else {
            Err(AuthorizationError(format!(
                "{principal} may not {action} {resource}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rbac() -> RbacConfig<String> {
        RbacConfig::default()
            .add_role(RbacRoleSpec::new("Admin").update_accessible_resources(
                RbacResourceSpec::new("ApiFull", "/api/**".to_string()).unwrap(),
                &["GET", "POST", "PUT", "PATCH", "DELETE"],
            ))
            .add_role(RbacRoleSpec::new("Reader").update_accessible_resources(
                RbacResourceSpec::new("Orders", "/api/orders*".to_string()).unwrap(),
                &["GET"],
            ))
    }

    fn principal_with(attribute: &str, roles: Vec<&str>) -> Principal {
        Principal::builder("u-1")
            .with_attribute(attribute, roles)
            .build()
    }

    #[tokio::test]
    async fn a_matching_role_grants_access() {
        let rbac = rbac();
        let admin = principal_with("roles", vec!["Admin"]);
        assert!(
            rbac.authorize(&admin, "DELETE", "/api/orders/2341")
                .await
                .is_ok()
        );
        assert!(rbac.authorize(&admin, "GET", "/api/anything").await.is_ok());
    }

    #[tokio::test]
    async fn the_action_is_matched_case_insensitively() {
        let reader = principal_with("roles", vec!["Reader"]);
        assert!(
            rbac()
                .authorize(&reader, "get", "/api/orders")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_role_only_grants_the_actions_it_was_given() {
        let reader = principal_with("roles", vec!["Reader"]);
        let err = rbac()
            .authorize(&reader, "DELETE", "/api/orders/1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("may not DELETE"));
    }

    #[tokio::test]
    async fn a_role_only_grants_the_resources_it_was_given() {
        let reader = principal_with("roles", vec!["Reader"]);
        assert!(
            rbac()
                .authorize(&reader, "GET", "/api/payments")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_role_the_principal_does_not_hold_grants_nothing() {
        let nobody = principal_with("roles", vec!["Stranger"]);
        assert!(
            rbac()
                .authorize(&nobody, "GET", "/api/orders")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn groups_are_accepted_in_place_of_roles() {
        let via_groups = principal_with("groups", vec!["Reader"]);
        assert!(
            rbac()
                .authorize(&via_groups, "GET", "/api/orders")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_principal_without_roles_is_denied_with_a_useful_reason() {
        let anonymous = Principal::builder("u-1").build();
        let err = rbac()
            .authorize(&anonymous, "GET", "/api/orders")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("carries no roles or groups"));
    }
}
