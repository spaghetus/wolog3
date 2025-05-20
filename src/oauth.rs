use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, LazyLock},
};

use dashmap::DashMap;
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, ConfigurationError, CsrfToken,
    IssuerUrl, LanguageTag, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
};
use reqwest::StatusCode;
use rocket::{
    http::{Cookie, CookieJar, SameSite, Status},
    request::{FromRequest, Outcome},
    response::{content::RawHtml, Redirect, Responder},
    Request, State,
};
use rocket_governor::{Method, Quota, RocketGovernable, RocketGovernor};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use tera::Tera;
use thiserror::Error;
use tokio::{
    sync::{RwLock, Semaphore},
    task::JoinSet,
};
use tracing::debug;
use url::Url;

use crate::{cookies::SecurePersist, db, Config};

#[derive(Error, Debug)]
pub enum LoginError {
    #[error("Database error")]
    DbError(#[from] sqlx::Error),
    #[error("Missing provider configuration")]
    NoProviderConfigured(String),
    #[error("Missing oauth state")]
    NoOAuthState,
    #[error("Bad oauth state")]
    BadOAuthState(serde_json::Error),
    #[error("Bad exchange code")]
    BadExchangeCode(ConfigurationError),
    #[error("Couldn't get auth token")]
    FailedAuthToken,
    #[error("Bad auth token")]
    BadAuthToken,
    #[error("No verified email")]
    NoVerifiedEmail,
    #[error("No name")]
    NoName,
}

impl<'r, 'o: 'r> Responder<'r, 'o> for LoginError {
    fn respond_to(self, request: &'r Request<'_>) -> rocket::response::Result<'o> {
        match self {
            LoginError::DbError(e) => {
                dbg!(e);
                (Status::InternalServerError, "Database error").respond_to(request)
            }
            LoginError::NoProviderConfigured(p) => (
                Status::InternalServerError,
                format!("Provider {p} not configured"),
            )
                .respond_to(request),
            LoginError::NoOAuthState => {
                (Status::BadRequest, "Missing oauth state").respond_to(request)
            }
            LoginError::BadOAuthState(e) => {
                dbg!(e);
                (Status::BadRequest, "Invalid oauth state").respond_to(request)
            }
            LoginError::BadExchangeCode(e) => {
                dbg!(e);
                (Status::Unauthorized, "Bad exchange code").respond_to(request)
            }
            LoginError::FailedAuthToken => {
                (Status::InternalServerError, "Couldn't get auth token").respond_to(request)
            }
            LoginError::BadAuthToken => {
                (Status::Unauthorized, "Couldn't get auth token").respond_to(request)
            }
            LoginError::NoVerifiedEmail => {
                (Status::Unauthorized, "A verified email is required").respond_to(request)
            }
            LoginError::NoName => {
                (Status::Unauthorized, "A user name is required").respond_to(request)
            }
        }
    }
}

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
});

static PROVIDERS: LazyLock<DashMap<String, CoreProviderMetadata>> = LazyLock::new(DashMap::new);

#[derive(Serialize, Deserialize, Clone)]
pub struct OAuthProvider {
    pub client_id: String,
    pub client_secret: String,
    pub issuer: Url,
    pub scopes: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Identity {
    pub sub: String,
    pub email: String,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct OAuthState {
    pub pkce_verifier: PkceCodeVerifier,
    pub csrf_token: CsrfToken,
    pub nonce: Nonce,
}

pub struct OAuthRateLimit;

impl RocketGovernable<'_> for OAuthRateLimit {
    fn quota(_method: Method, _route_name: &str) -> Quota {
        #[cfg(debug_assertions)]
        return Quota::per_minute(Self::nonzero(100));
        Quota::per_minute(Self::nonzero(2))
    }
}

async fn provider(url: &Url) -> CoreProviderMetadata {
    if let Some(p) = PROVIDERS.get(url.as_str()) {
        return p.value().clone();
    }
    let provider = CoreProviderMetadata::discover_async(IssuerUrl::from_url(url.clone()), &*CLIENT)
        .await
        .unwrap();
    PROVIDERS.insert(url.to_string(), provider.clone());
    provider
}

#[get("/clear")]
pub async fn clear(mut secure_persist: SecurePersist, cookies: &CookieJar<'_>) -> String {
    let amt = secure_persist.identities.len();
    secure_persist.identities.clear();
    cookies.add_private(secure_persist);
    format!("Cleared {amt} identities from your cookies")
}

#[get("/forgetme")]
pub async fn forgetme(
    mut secure_persist: SecurePersist,
    cookies: &CookieJar<'_>,
    db: &State<Pool<Postgres>>,
) -> Result<String, LoginError> {
    let amt = secure_persist.identities.len();
    for (provider, sub) in &secure_persist.identities {
        db::delete_identity(db, provider, sub).await?
    }
    secure_persist.identities.clear();
    cookies.add_private(secure_persist);
    Ok(format!(
        "Cleared {amt} identities from your cookies and the database"
    ))
}

#[get("/challenge/<provider_id>")]
pub async fn login(
    provider_id: &str,
    cfg: &State<Arc<Config>>,
    cookies: &CookieJar<'_>,
    _limit: RocketGovernor<'_, OAuthRateLimit>,
) -> Result<Redirect, LoginError> {
    let Some(provider_cfg) = cfg.oauth_providers.get(provider_id) else {
        return Err(LoginError::NoProviderConfigured(provider_id.to_string()));
    };
    let provider = provider(&provider_cfg.issuer).await;
    let client = CoreClient::from_provider_metadata(
        provider.clone(),
        ClientId::new(provider_cfg.client_id.clone()),
        Some(ClientSecret::new(provider_cfg.client_secret.clone())),
    )
    .set_redirect_uri(
        RedirectUrl::new(format!("{}login/callback/{provider_id}", cfg.origin)).unwrap(),
    );
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_token, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(pkce_challenge)
        .add_scopes(provider_cfg.scopes.iter().map(|s| Scope::new(s.clone())))
        .url();
    let mut cookie = Cookie::new(
        "oauth_state",
        serde_json::to_string(&OAuthState {
            pkce_verifier,
            csrf_token,
            nonce,
        })
        .unwrap(),
    );
    cookie.set_same_site(SameSite::Lax);
    cookies.add_private(cookie);
    Ok(Redirect::found(auth_url.to_string()))
}

#[allow(clippy::too_many_arguments)]
#[get("/callback/<provider_id>?<code>&<state>")]
pub async fn callback(
    provider_id: &str,
    cfg: &State<Arc<Config>>,
    cookies: &CookieJar<'_>,
    _limit: RocketGovernor<'_, OAuthRateLimit>,
    tera: &State<Arc<RwLock<Tera>>>,
    code: &str,
    state: &str,
    db: &State<Pool<Postgres>>,
    mut secure_persist: SecurePersist,
) -> Result<String, LoginError> {
    cookies.remove_private("oauth_state");
    let OAuthState {
        pkce_verifier,
        csrf_token,
        nonce,
    } = cookies
        .get_private("oauth_state")
        .ok_or_else(|| LoginError::NoOAuthState)
        .and_then(|s| serde_json::from_str(s.value()).map_err(|e| LoginError::BadOAuthState(e)))?;
    let Some(provider_cfg) = cfg.oauth_providers.get(provider_id) else {
        return Err(LoginError::NoProviderConfigured(provider_id.to_string()));
    };
    let provider = provider(&provider_cfg.issuer).await;
    let client = CoreClient::from_provider_metadata(
        provider.clone(),
        ClientId::new(provider_cfg.client_id.clone()),
        Some(ClientSecret::new(provider_cfg.client_secret.clone())),
    )
    .set_redirect_uri(
        RedirectUrl::new(format!("{}login/callback/{provider_id}", cfg.origin)).unwrap(),
    );

    debug!("Got client");

    let token_response = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .map_err(|e| LoginError::BadExchangeCode(e))?
        .set_pkce_verifier(pkce_verifier)
        .request_async(&*CLIENT)
        .await
        .map_err(|e| {
            eprintln!("{e}");
            LoginError::FailedAuthToken
        })?;

    debug!("Got token");

    let id_token = token_response.id_token().unwrap();
    let id_token_verifier = client.id_token_verifier();
    let claims = id_token.claims(&id_token_verifier, &nonce).unwrap();
    if let Some(expected_access_token_hash) = claims.access_token_hash() {
        let actual_access_token_hash = AccessTokenHash::from_token(
            token_response.access_token(),
            id_token.signing_alg().unwrap(),
            id_token.signing_key(&id_token_verifier).unwrap(),
        )
        .unwrap();
        if actual_access_token_hash != *expected_access_token_hash {
            return Err(LoginError::BadAuthToken);
        }
    }

    debug!("Verified token");

    let Some(email) = claims.email() else {
        return Err(LoginError::NoVerifiedEmail);
    };

    if claims.email_verified() != Some(true) {
        return Err(LoginError::NoVerifiedEmail);
    }

    let Some(name) = claims.name().and_then(|n| {
        n.get(Some(&LanguageTag::new("en".to_string())))
            .or_else(|| n.get(None))
    }) else {
        return Err(LoginError::NoName);
    };

    let identity = Identity {
        sub: claims.subject().to_string(),
        email: email.to_string(),
        name: name.to_string(),
    };

    secure_persist
        .identities
        .insert(provider_id.to_string(), identity.sub.clone());
    cookies.add_private(secure_persist);

    db::add_identity(db, provider_id, identity).await?;

    Ok("Login OK!".to_string())
}

#[derive(Default, Serialize)]
pub struct Identities(HashMap<String, Identity>);

#[async_trait]
impl<'r> FromRequest<'r> for Identities {
    type Error = String;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let cookies = request.cookies();
        let Outcome::Success(mut secure_persist) = SecurePersist::from_request(request).await
        else {
            return Outcome::Success(Default::default());
        };
        let db: Pool<Postgres> = request.rocket().state::<Pool<Postgres>>().unwrap().clone();
        let identities: Vec<(String, Result<Option<Identity>, _>)> = secure_persist
            .identities
            .clone()
            .into_iter()
            .map(|(provider, sub)| {
                let db = db.clone();
                async move {
                    (
                        provider.clone(),
                        db::get_identity(&db, &provider, &sub).await,
                    )
                }
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        let mut bad = false;
        let identities = identities
            .into_iter()
            .filter_map(|(provider, identity)| {
                if let Ok(Some(identity)) = identity {
                    return Some((provider, identity));
                }
                if let Err(e) = identity {
                    dbg!(e);
                }
                bad = true;
                secure_persist.identities.remove(&provider);
                None
            })
            .collect();
        if bad {
            cookies.add_private(secure_persist);
        }
        rocket::outcome::Outcome::Success(Self(identities))
    }
}

impl Deref for Identities {
    type Target = HashMap<String, Identity>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
