use chrono::{DateTime, Utc};
use rocket::{
    http::Cookie,
    outcome::{IntoOutcome, Outcome},
    request::{self, FromRequest},
    Request,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Default)]
pub struct ClientPersist {
    pub viewed: HashMap<String, DateTime<Utc>>,
}

#[async_trait]
impl<'r> FromRequest<'r> for ClientPersist {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        Outcome::Success(
            request
                .cookies()
                .get("client_persist")
                .and_then(|c| serde_json::from_str(c.value()).ok())
                .unwrap_or_default(),
        )
    }
}

impl From<ClientPersist> for Cookie<'static> {
    fn from(val: ClientPersist) -> Self {
        Cookie::new(
            "client_persist",
            serde_json::to_string(&val).unwrap_or_default(),
        )
    }
}
