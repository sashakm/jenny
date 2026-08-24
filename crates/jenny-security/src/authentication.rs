//! Turning credentials into a [`Principal`].
pub mod api_key;
pub mod jwk;
pub mod jwt;

use async_trait::async_trait;
use sha2::Digest;

use jenny_core::domain::{AuthenticationError, Credentials, Principal};
use jenny_core::port::outbound::Authenticator;

use crate::authentication::{
    api_key::ApiKeyAuthContextConfig,
    jwt::{JwtAuthContextConfig, JwtClaims},
};

impl From<JwtClaims> for Principal {
    fn from(value: JwtClaims) -> Self {
        Principal::builder(value.user_id)
            .with_attribute("audience", value.audience)
            .with_attribute("issued_at", value.issued_at)
            .with_attribute("issuer", value.issuer)
            .with_attribute("token_id", value.token_id)
            .with_attribute("not_before", value.not_before)
            .with_attribute("authorizing_party", value.authorizing_party)
            .with_attribute("expiration", value.expiration)
            .with_attributes(value.additional_claims)
            .build()
    }
}

/// Which authentication mechanisms this service accepts.
///
/// Each mechanism is opt-in: a credential whose scheme has not been enabled is
/// rejected rather than silently ignored, so turning off JWT support cannot
/// accidentally leave a door open.
#[derive(Clone, Debug, Default)]
pub struct AuthenticationConfig {
    api_key: Option<ApiKeyAuthContextConfig>,
    jwt: Option<JwtAuthContextConfig>,
}

impl AuthenticationConfig {
    pub fn with_api_key_enabled(mut self, config: ApiKeyAuthContextConfig) -> Self {
        self.api_key = Some(config);
        self
    }

    pub fn with_jwt_enabled(mut self, config: JwtAuthContextConfig) -> Self {
        self.jwt = Some(config);
        self
    }

    pub fn api_key(&self) -> Option<&ApiKeyAuthContextConfig> {
        self.api_key.as_ref()
    }

    pub fn jwt(&self) -> Option<&JwtAuthContextConfig> {
        self.jwt.as_ref()
    }
}

#[async_trait]
impl Authenticator for AuthenticationConfig {
    async fn authenticate(
        &self,
        credentials: &Credentials,
    ) -> Result<Principal, AuthenticationError> {
        match credentials {
            Credentials::ApiKey(secret) => {
                let config = self.api_key().ok_or_else(|| {
                    AuthenticationError("api key authentication is not enabled".to_string())
                })?;
                // Keys are stored hashed, so a leaked store does not leak usable keys.
                let hashed = sha2::Sha256::digest(secret.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                config
                    .get(&hashed)
                    .await
                    .map_err(|e| AuthenticationError(e.to_string()))?
                    .ok_or_else(|| AuthenticationError("unknown api key".to_string()))
            }
            Credentials::JsonWebToken(token) => {
                let config = self.jwt().ok_or_else(|| {
                    AuthenticationError("jwt authentication is not enabled".to_string())
                })?;
                let claims = config
                    .verify_token(token)
                    .await
                    .map_err(|e| AuthenticationError(e.to_string()))?
                    .claims;
                Ok(Principal::from(claims))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_disabled_mechanism_rejects_rather_than_ignores() {
        let config = AuthenticationConfig::default();

        let err = config
            .authenticate(&Credentials::ApiKey("whatever".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("api key authentication"));

        let err = config
            .authenticate(&Credentials::JsonWebToken("whatever".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("jwt authentication"));
    }

    #[tokio::test]
    async fn an_unknown_api_key_is_rejected() {
        let config = AuthenticationConfig::default()
            .with_api_key_enabled(ApiKeyAuthContextConfig::new_static(Default::default()));
        let err = config
            .authenticate(&Credentials::ApiKey("nope".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown api key"));
    }

    #[tokio::test]
    async fn a_known_api_key_yields_its_principal() {
        // The store is keyed by the SHA-256 of the presented key.
        let secret = "s3cret";
        let hashed = sha2::Sha256::digest(secret.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let store = std::collections::HashMap::from([(
            hashed,
            r#"{"user_id":"svc-1","name":"Batch Job","roles":["Dispatch"]}"#
                .to_string()
                .into_boxed_str(),
        )]);

        let config = AuthenticationConfig::default()
            .with_api_key_enabled(ApiKeyAuthContextConfig::new_static(store));
        let principal = config
            .authenticate(&Credentials::ApiKey(secret.into()))
            .await
            .unwrap();

        assert_eq!(principal.id(), "svc-1");
        assert_eq!(principal.to_string(), "Principal(Batch Job)");
        assert!(principal.has_role("Dispatch"));
    }

    #[tokio::test]
    async fn the_config_is_usable_through_the_port() {
        let erased: std::sync::Arc<dyn Authenticator> =
            std::sync::Arc::new(AuthenticationConfig::default());
        assert!(
            erased
                .authenticate(&Credentials::ApiKey("x".into()))
                .await
                .is_err()
        );
    }
}
