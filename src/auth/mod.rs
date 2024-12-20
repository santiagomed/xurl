pub mod listener;
pub mod token_store;

use crate::auth::listener::listen_for_code;
use crate::auth::token_store::Token;
use crate::auth::token_store::TokenStore;
use crate::auth::token_store::TokenStoreError;
use crate::config::Config;

use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use rand::Rng;
use sha1::Sha1;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Missing environment variable: {0}")]
    MissingEnvVar(&'static str),
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Invalid code: {0}")]
    InvalidCode(String),
    #[error("Invalid token: {0}")]
    InvalidToken(String),
    #[error("Authorization error: {0}")]
    AuthorizationError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Token not found")]
    TokenNotFound(String),
    #[error("Invalid auth type: {0}")]
    InvalidAuthType(String),
    #[error("Non-OAuth2 tokens found when looking for OAuth2 token")]
    WrongTokenFoundInStore,
    #[error("Token store error: {0}")]
    TokenStoreError(#[from] TokenStoreError),
}

pub struct Auth {
    token_store: TokenStore,
    info_url: String,
    client_id: String,
    client_secret: String,
    auth_url: String,
    token_url: String,
    redirect_uri: String,
}

impl Auth {
    pub fn new(config: Config) -> Self {
        Self {
            token_store: TokenStore::new(),
            info_url: config.info_url,
            client_id: config.client_id,
            client_secret: config.client_secret,
            auth_url: config.auth_url,
            token_url: config.token_url,
            redirect_uri: config.redirect_uri,
        }
    }

    #[allow(dead_code)]
    pub fn with_token_store(mut self, token_store: TokenStore) -> Self {
        self.token_store = token_store;
        self
    }

    pub fn oauth1(
        &self,
        method: &str,
        url: &str,
        additional_params: Option<BTreeMap<String, String>>,
    ) -> Result<String, AuthError> {
        let nonce = generate_nonce();
        let timestamp = generate_timestamp()?;

        let token = self
            .token_store
            .get_oauth1_tokens()
            .ok_or(AuthError::TokenNotFound(
                "No OAuth1 tokens found".to_string(),
            ))?;
        let (consumer_key, access_token, consumer_secret, token_secret) = match token {
            Token::OAuth1(token) => (
                token.consumer_key,
                token.access_token,
                token.consumer_secret,
                token.token_secret,
            ),
            _ => return Err(AuthError::InvalidToken("Invalid token type".to_string())),
        };

        let mut params = BTreeMap::new();
        params.insert("oauth_consumer_key".to_string(), consumer_key.to_string());
        params.insert("oauth_nonce".to_string(), nonce);
        params.insert(
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        );
        params.insert("oauth_timestamp".to_string(), timestamp);
        params.insert("oauth_token".to_string(), access_token.to_string());
        params.insert("oauth_version".to_string(), "1.0".to_string());

        // Add any additional parameters
        if let Some(add_params) = additional_params {
            params.extend(add_params);
        }

        let signature = generate_signature(method, url, &params, &consumer_secret, &token_secret)?;

        params.insert("oauth_signature".to_string(), signature);

        let auth_header = params
            .iter()
            .filter(|(k, _)| k.starts_with("oauth_"))
            .map(|(k, v)| format!("{}=\"{}\"", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join(", ");

        Ok(format!("OAuth {}", auth_header))
    }

    pub async fn oauth2(&mut self, username: Option<&str>) -> Result<String, AuthError> {
        if let Some(username) = username {
            if let Some(token) = self.token_store.get_oauth2_token(username) {
                match token {
                    Token::OAuth2(token) => return Ok(token),
                    _ => return Err(AuthError::WrongTokenFoundInStore),
                }
            } else {
                return Err(AuthError::TokenNotFound(format!(
                    "No cached OAuth2 token found for {}",
                    username
                )));
            }
        }

        if self.client_id.is_empty() || self.client_secret.is_empty() {
            return Err(AuthError::MissingEnvVar("CLIENT_ID or CLIENT_SECRET"));
        }

        let client = BasicClient::new(
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
            AuthUrl::new(self.auth_url.clone()).map_err(|e| AuthError::InvalidUrl(e.to_string()))?,
            Some(
                TokenUrl::new(self.token_url.clone())
                    .map_err(|e| AuthError::InvalidUrl(e.to_string()))?,
            ),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.redirect_uri.clone())
                .map_err(|e| AuthError::InvalidUrl(e.to_string()))?,
        );

        let (code_challenge, code_verifier) = PkceCodeChallenge::new_random_sha256();

        let (auth_url, _csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("block.read".to_string()))
            .add_scope(Scope::new("bookmark.read".to_string()))
            .add_scope(Scope::new("dm.read".to_string()))
            .add_scope(Scope::new("follows.read".to_string()))
            .add_scope(Scope::new("like.read".to_string()))
            .add_scope(Scope::new("list.read".to_string()))
            .add_scope(Scope::new("mute.read".to_string()))
            .add_scope(Scope::new("space.read".to_string()))
            .add_scope(Scope::new("tweet.read".to_string()))
            .add_scope(Scope::new("timeline.read".to_string()))
            .add_scope(Scope::new("users.read".to_string()))
            .add_scope(Scope::new("block.write".to_string()))
            .add_scope(Scope::new("bookmark.write".to_string()))
            .add_scope(Scope::new("dm.write".to_string()))
            .add_scope(Scope::new("follows.write".to_string()))
            .add_scope(Scope::new("like.write".to_string()))
            .add_scope(Scope::new("list.write".to_string()))
            .add_scope(Scope::new("mute.write".to_string()))
            .add_scope(Scope::new("tweet.write".to_string()))
            .add_scope(Scope::new("tweet.moderate.write".to_string()))
            .add_scope(Scope::new("timeline.write".to_string()))
            .add_scope(Scope::new("offline.access".to_string()))
            .add_scope(Scope::new("media.write".to_string()))
            .set_pkce_challenge(code_challenge)
            .url();

        webbrowser::open(auth_url.as_str())
            .map_err(|e| AuthError::IoError(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        let code = listen_for_code(8080)
            .await
            .map_err(|e| AuthError::InvalidCode(e))?;

        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(code_verifier)
            .request_async(async_http_client)
            .await
            .map_err(|e| match e {
                oauth2::RequestTokenError::ServerResponse(e) => {
                    AuthError::AuthorizationError(e.to_string())
                }
                oauth2::RequestTokenError::Request(e) => AuthError::NetworkError(e.to_string()),
                _ => AuthError::InvalidToken(e.to_string()),
            })?;

        let token = token.access_token().secret().to_string();

        let username = reqwest::Client::new()
            .get(&self.info_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| AuthError::NetworkError(e.to_string()))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| AuthError::NetworkError(e.to_string()))?;

        let username = username["data"]["username"]
            .as_str()
            .ok_or_else(|| AuthError::NetworkError("Missing username field".to_string()))?
            .to_string();

        self.token_store.save_oauth2_token(&username, &token)?;

        Ok(token)
    }

    pub fn bearer_token(&self) -> Option<String> {
        self.token_store
            .get_bearer_token()
            .as_ref()
            .and_then(|token| match token {
                Token::Bearer(token) => Some(token.clone()),
                _ => None,
            })
    }

    pub fn get_token_store(&mut self) -> &mut TokenStore {
        &mut self.token_store
    }
}

// OAuth 1.0 helper functions

/// Generate OAuth 1.0 signature
fn generate_signature(
    method: &str,
    url: &str,
    params: &BTreeMap<String, String>,
    consumer_secret: &str,
    token_secret: &str,
) -> Result<String, AuthError> {
    let parameter_string = params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        encode(method),
        encode(url),
        encode(&parameter_string)
    );

    let signing_key = format!("{}&{}", encode(consumer_secret), encode(token_secret));

    let mut mac = Hmac::<Sha1>::new_from_slice(signing_key.as_bytes())
        .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

    mac.update(base_string.as_bytes());
    let result = mac.finalize();
    Ok(STANDARD.encode(result.into_bytes()))
}

/// Generate a random nonce
fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let nonce: u64 = rng.gen();
    format!("{:x}", nonce)
}

/// Generate a timestamp
fn generate_timestamp() -> Result<String, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .map_err(|e| AuthError::InvalidToken(e.to_string()))
}

/// Encode a value for OAuth
fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}
