use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::PathBuf,
    sync::Arc,
};

use rocket::{
    form::Form,
    http::Status,
    response::{content::RawHtml, Responder},
    Request, State,
};
use rocket_governor::{Method, Quota, RocketGovernable, RocketGovernor};
use sqlx::{Pool, Postgres};
use tera::Tera;
use thiserror::Error;
use tokio::{sync::RwLock, task::JoinSet};
use tokio_stream::StreamExt;

use crate::{
    cookies::SecurePersist,
    db,
    oauth::{Identities, Identity, LoginError},
    Config,
};

#[derive(Error, Debug)]
pub enum GuestbookError {
    #[error("Database error")]
    DbError(#[from] sqlx::Error),
    #[error("You are banned")]
    YouAreBanned,
    #[error("No identity")]
    NoIdentity,
    #[error("NotFound")]
    NotFound,
}

impl<'r, 'o: 'r> Responder<'r, 'o> for GuestbookError {
    fn respond_to(self, request: &'r Request<'_>) -> rocket::response::Result<'o> {
        match self {
            GuestbookError::DbError(e) => {
                dbg!(e);
                (Status::InternalServerError, "Database error").respond_to(request)
            }
            GuestbookError::YouAreBanned => {
                (Status::InternalServerError, "Database error").respond_to(request)
            }
            GuestbookError::NoIdentity => (
                Status::Unauthorized,
                "You aren't logged in with that identity",
            )
                .respond_to(request),
            GuestbookError::NotFound => (Status::Unauthorized, "Not found").respond_to(request),
        }
    }
}

#[get("/<path..>")]
pub async fn display(
    path: PathBuf,
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
    identities: Identities,
    config: &State<Arc<Config>>,
    _rl: RocketGovernor<'_, GuestbookRateLimit>,
) -> Result<RawHtml<String>, GuestbookError> {
    let providers = config.oauth_providers.keys().collect::<Vec<_>>();
    let path = path.to_string_lossy();
    let path = path.trim_start_matches(['.', '/']).trim_end_matches(".md");
    let Some(meta) = db::read_post_meta(db, path).await else {
        return Err(GuestbookError::NotFound);
    };
    let guests = db::read_guestbook(db, path).await?;
    let has_signed: HashSet<String> = identities
        .clone()
        .into_iter()
        .map(|(provider, identity)| {
            let db = (*db).clone();
            async move {
                (
                    provider.clone(),
                    db::has_signed(&db, &provider, &identity.sub)
                        .await
                        .unwrap_or(false),
                )
            }
        })
        .collect::<JoinSet<_>>()
        .join_all()
        .await
        .into_iter()
        .filter_map(|(p, s)| s.then_some(p))
        .collect();
    let content = tera
        .read()
        .await
        .render(
            "guestbook.html.tera",
            &crate::context!({
                "path": path,
                "meta": meta,
                "has_signed": has_signed,
                "guestbook": guests,
                "identities": identities,
                "providers": providers,
            }),
        )
        .unwrap();
    Ok(RawHtml(content))
}

#[derive(FromForm)]
struct GuestbookForm {
    pub identity: String,
    pub do_sign: bool,
}

pub struct GuestbookRateLimit;

impl RocketGovernable<'_> for GuestbookRateLimit {
    fn quota(_method: Method, _route_name: &str) -> Quota {
        #[cfg(debug_assertions)]
        return Quota::per_minute(Self::nonzero(100));
        Quota::per_minute(Self::nonzero(5))
    }
}

#[post("/<path_..>", data = "<form>")]
pub async fn sign(
    form: Form<GuestbookForm>,
    path_: PathBuf,
    db: &State<Pool<Postgres>>,
    tera: &State<Arc<RwLock<Tera>>>,
    identities: Identities,
    config: &State<Arc<Config>>,
    _rl: RocketGovernor<'_, GuestbookRateLimit>,
) -> Result<RawHtml<String>, GuestbookError> {
    let path = path_.to_string_lossy();
    let path = path.trim_start_matches(['.', '/']).trim_end_matches(".md");
    let Some(identity) = identities.get(&form.identity) else {
        return Err(GuestbookError::NoIdentity);
    };
    if form.do_sign {
        if config
            .guestbook_bans
            .get(&form.identity)
            .map(|bans| bans.contains(&identity.sub))
            .unwrap_or(false)
        {
            return Err(GuestbookError::YouAreBanned);
        }
        db::sign_guestbook(db, path, &form.identity, &identity.sub).await?
    } else {
        db::unsign_guestbook(db, path, &form.identity, &identity.sub).await?
    };
    display(path_, db, tera, identities, config, _rl).await
}
