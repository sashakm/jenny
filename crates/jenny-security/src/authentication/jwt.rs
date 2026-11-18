//! Provides a basic data structure to implement JWT authentication
use jsonwebtoken::{TokenData, jwk::Jwk};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use url::Url;

use std::{collections::HashMap, sync::Arc};

use super::jwk::JwksCache;

/// A JWT representation with common oauth2 auth flow claims used by oidc.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct JwtClaims {
    /// Required: The subject (user) identifier
    #[serde(alias = "sub")]
    pub user_id: String,
    /// Required: The time the token was issued at
    #[serde(alias = "iat")]
    pub issued_at: u64,
    /// Required: The time after which the token must not be accepted
    #[serde(alias = "exp")]
    pub expiration: u64,
    /// Required: The client or clients the token is intended for
    #[serde(alias = "aud")]
    #[serde(default)]
    #[serde(deserialize_with = "crate::utils::deserialize_one_or_many_strings")]
    pub audience: Vec<String>,
    /// Required: The url of the tokens issuing authority
    #[serde(alias = "iss")]
    pub issuer: String,
    /// Optional: The time before which the token must not be accepted
    #[serde(default)]
    #[serde(alias = "nbf")]
    pub not_before: u64,
    /// Optional: The unique identifier for the token
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(alias = "jti")]
    pub token_id: String,
    /// Optional: The client id which facilitated the authentication
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    /// Optional: The id of the entity which requested the issuance of the token
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(alias = "azp")]
    pub authorizing_party: String,
    /// Optional: Any additional claims present in the token
    #[serde(default)]
    #[serde(flatten)]
    pub additional_claims: HashMap<String, Value>,
}

/// Holds OIDC configuration details
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OidcConfig {
    /// the auth providers realm url
    pub issuer: Vec<Url>,
    /// the jwks url source
    pub jwks_url: Url,
    /// jwks cache time-to-live in seconds. Default: 1800 (30 minutes)
    pub jwks_cache_ttl: Option<u64>,
    /// the client (or resource) id
    pub client_id: Option<String>,
    /// the client secret, if applicable
    pub client_secret: Option<String>,
    /// verify authorizing party claim (`azp`) contains one or more of [valid_authorizing_parties](Self::valid_authorizing_parties)
    pub valid_authorizing_parties: Option<Vec<String>>,
    /// verify audience (`aud`) claim contains one or more of [valid_audience](Self::valid_audience)
    pub valid_audience: Option<Vec<String>>,
    /// don't verify token issuer claim with configured [issuer](Self::issuer). Default: false
    #[serde(default)]
    pub skip_issuer_validation: bool,
    /// accept tokens whatever audience and authorizing party they name. Default: false
    ///
    /// Without either check, a token the same provider issued to *any* client is accepted
    /// here, so this has to be asked for explicitly.
    #[serde(default)]
    pub skip_audience_validation: bool,
}

impl OidcConfig {
    /// Create a [JwtAuthContextConfig] from this configuration.
    ///
    /// Fails when neither [`valid_audience`](Self::valid_audience) nor
    /// [`valid_authorizing_parties`](Self::valid_authorizing_parties) is set, unless
    /// [`skip_audience_validation`](Self::skip_audience_validation) says so.
    pub fn auth_context_config(&self) -> anyhow::Result<JwtAuthContextConfig> {
        let unchecked = self.valid_audience.as_ref().is_none_or(Vec::is_empty)
            && self
                .valid_authorizing_parties
                .as_ref()
                .is_none_or(Vec::is_empty);
        if unchecked {
            if !self.skip_audience_validation {
                anyhow::bail!(
                    "set `valid_audience` or `valid_authorizing_parties`; without either, tokens \
                     the provider issued to any other client are accepted \
                     (`skip_audience_validation: true` allows that explicitly)"
                );
            }
            warn!("Audience and authorizing party validation are disabled. This is insecure.");
        }
        Ok(JwtAuthContextConfig::new(
            JwksCache::new(self.jwks_url.as_str(), self.jwks_cache_ttl.unwrap_or(1800)),
            if self.skip_issuer_validation {
                warn!("Issuer validation disabled. This is insecure.");
                Default::default()
            } else {
                self.issuer.iter().map(Url::to_string).collect()
            },
            self.valid_audience.clone().unwrap_or_default(),
            self.valid_authorizing_parties.clone().unwrap_or_default(),
        ))
    }
}

/// Configuration for JWT-based authentication.
#[derive(Debug, Clone)]
pub struct JwtAuthContextConfig {
    jwks_cache: Arc<JwksCache>,
    valid_issuers: Vec<String>,
    valid_audience: Vec<String>,
    valid_authorizing_parties: Vec<String>,
}

impl JwtAuthContextConfig {
    pub fn new(
        jwks_cache: JwksCache,
        valid_issuers: Vec<String>,
        valid_audience: Vec<String>,
        valid_authorizing_parties: Vec<String>,
    ) -> Self {
        Self {
            jwks_cache: Arc::new(jwks_cache),
            valid_issuers,
            valid_audience,
            valid_authorizing_parties,
        }
    }

    /// Fetch signing key from jwk cache by key id
    pub async fn lookup_jwk(&self, kid: &str) -> anyhow::Result<Jwk> {
        Ok(self.jwks_cache.read_jwk(kid).await?)
    }

    /// Validate JWT signature, expiration and issuer and decode token into [TokenData]<[JwtClaims]>
    pub async fn verify_token(&self, jwt: &str) -> anyhow::Result<TokenData<JwtClaims>> {
        let header = jsonwebtoken::decode_header(jwt)?;
        self.lookup_jwk(&header.kid.unwrap_or_default())
            .await
            .and_then(|jwk| {
                // The header is the token's own claim about how it was signed. A key that
                // declares its algorithm is held to that one instead.
                if let Some(declared) = jwk.common.key_algorithm
                    && jsonwebtoken::Algorithm::try_from(declared)? != header.alg
                {
                    anyhow::bail!(
                        "token algorithm {:?} does not match the signing key's {declared:?}",
                        header.alg
                    );
                }
                self.create_token_decoder(&jwk).map(|decoder| {
                    let mut validation = jsonwebtoken::Validation::new(header.alg);
                    validation.validate_aud = false;
                    if !self.valid_issuers.is_empty() {
                        validation.set_issuer(&self.valid_issuers);
                        validation.required_spec_claims.insert("iss".to_string());
                    }
                    (decoder, validation)
                })
            })
            .and_then(|(decoder, validation)| {
                let token_data = jsonwebtoken::decode::<JwtClaims>(jwt, &decoder, &validation)?;
                if !self.valid_audience.is_empty()
                    && !token_data
                        .claims
                        .audience
                        .iter()
                        .any(|aud| self.valid_audience.contains(aud))
                {
                    return Err(anyhow::anyhow!("Invalid JWT Audience"));
                }
                if !self.valid_authorizing_parties.is_empty()
                    && !self
                        .valid_authorizing_parties
                        .contains(&token_data.claims.authorizing_party)
                {
                    return Err(anyhow::anyhow!("Invalid Authorizing Party"));
                }
                Ok(token_data)
            })
    }

    /// Create [jsonwebtoken::DecodingKey] from signing [Jwk]
    pub fn create_token_decoder(&self, signer: &Jwk) -> anyhow::Result<jsonwebtoken::DecodingKey> {
        // `from_jwk` base64url-decodes symmetric keys, which the JWK format requires and
        // using the encoded text as the secret got wrong.
        Ok(jsonwebtoken::DecodingKey::from_jwk(signer)?)
    }
}
