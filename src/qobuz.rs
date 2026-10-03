//! The bits of the Qobuz HTTP API this controller needs: a Connect token and the catalogue.
//!
//! Only endpoints that a logged-in user may call with an app id and a user auth token. Nothing
//! here resolves stream URLs: the renderer fetches the audio itself, which is the whole point of
//! Qobuz Connect.

use qobuz_connect::{Credentials, Error as ConnectError, TokenRequest};
use reqwest::Client;
use serde_json::Value;

const API: &str = "https://www.qobuz.com/api.json/0.2";

#[derive(Clone)]
pub struct Qobuz {
    client: Client,
    app_id: String,
    user_auth_token: String,
}

impl Qobuz {
    pub fn new(app_id: String, user_auth_token: String) -> Self {
        Self {
            client: Client::new(),
            app_id,
            user_auth_token,
        }
    }

    /// Mints a Connect token. The cloud serves one socket per token, so the session asks for a
    /// fresh one before every connection.
    pub async fn token(&self) -> Result<Credentials, ConnectError> {
        let request = TokenRequest::new(&self.app_id, &self.user_auth_token);
        let mut post = self
            .client
            .post(request.url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(request.body);
        for (name, value) in &request.headers {
            post = post.header(*name, value);
        }
        let body = post
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| ConnectError::Token(err.to_string()))?
            .bytes()
            .await
            .map_err(|err| ConnectError::Token(err.to_string()))?;
        Credentials::from_json(&body)
    }

    /// A GET against the API, returned as-is. The browser picks the fields it wants, so a change
    /// in the Qobuz schema does not have to travel through this file.
    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, reqwest::Error> {
        self.client
            .get(format!("{API}/{path}"))
            .header("X-App-Id", &self.app_id)
            .header("X-User-Auth-Token", &self.user_auth_token)
            .query(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
}
