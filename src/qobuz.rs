//! The bits of the Qobuz HTTP API this controller needs: a Connect token and the catalogue.
//!
//! Only endpoints that a logged-in user may call with an app id and a user auth token. Nothing
//! here resolves stream URLs: the renderer fetches the audio itself, which is the whole point of
//! Qobuz Connect.

use qobuz_connect::{Credentials, Error as ConnectError, TokenRequest};
use reqwest::Client;
use serde_json::Value;

const API: &str = "https://www.qobuz.com/api.json/0.2";

/// A Qobuz Connect token with the expiry a device needs to be told about.
pub struct Token {
    pub endpoint: String,
    pub jwt: String,
    pub expires: u64,
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("no {key} in the token"))
}

#[derive(Clone)]
pub struct Qobuz {
    client: Client,
    app_id: String,
    user_auth_token: String,
}

impl Qobuz {
    pub fn new(app_id: String, user_auth_token: String) -> Self {
        Self {
            // Qobuz sits behind a Varnish that answers some clients with a 403; look like the
            // browser its web player runs in.
            client: Client::builder()
                .user_agent(crate::login::USER_AGENT)
                .build()
                .unwrap_or_default(),
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

    /// The same token, with the expiry the LAN handshake has to pass on.
    pub async fn connect_token(&self) -> Result<Token, String> {
        let request = TokenRequest::new(&self.app_id, &self.user_auth_token);
        let mut post = self
            .client
            .post(request.url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(request.body);
        for (name, value) in &request.headers {
            post = post.header(*name, value);
        }
        let body: Value = post
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| err.to_string())?
            .json()
            .await
            .map_err(|err| err.to_string())?;
        let token = body
            .get("jwt_qws")
            .ok_or_else(|| format!("no jwt_qws in the answer: {body}"))?;
        Ok(Token {
            endpoint: field(token, "endpoint")?.to_owned(),
            jwt: field(token, "jwt")?.to_owned(),
            expires: token
                .get("exp")
                .and_then(Value::as_u64)
                .ok_or("no exp in the token")?,
        })
    }

    /// The bearer token a device needs to talk to the Qobuz API on its own. The user auth token
    /// is not it: a device handed that one holds the queue and reports itself as playing, but
    /// never resolves a stream and sits at position zero.
    pub async fn api_token(&self) -> Result<Token, String> {
        let body: Value = self
            .client
            .post(format!("{API}/qws/refreshToken"))
            .header("X-App-Id", &self.app_id)
            .header("X-User-Auth-Token", &self.user_auth_token)
            .form(&[("jwt", "jwt_api")])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| err.to_string())?
            .json()
            .await
            .map_err(|err| err.to_string())?;
        let token = body
            .get("jwt_api")
            .ok_or_else(|| format!("no jwt_api in the answer: {body}"))?;
        Ok(Token {
            endpoint: String::new(),
            jwt: field(token, "jwt")?.to_owned(),
            expires: token
                .get("exp")
                .and_then(Value::as_u64)
                .ok_or("no exp in the token")?,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
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
