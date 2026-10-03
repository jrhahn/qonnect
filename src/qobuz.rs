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

/// Spelled out rather than derived: the jwt is a credential and has no business in a log line.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("endpoint", &self.endpoint)
            .field("jwt", &format_args!("<{} chars>", self.jwt.len()))
            .field("expires", &self.expires)
            .finish()
    }
}

/// Whether the answer carries the endpoint to talk to, as `jwt_qws` does and `jwt_api` does not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Required,
    None,
}

/// Reads one token out of an answer of the token endpoints.
fn token(body: &Value, key: &str, endpoint: Endpoint) -> Result<Token, String> {
    let token = body
        .get(key)
        .ok_or_else(|| format!("no {key} in the answer: {body}"))?;
    let field = |name: &str| {
        token
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no {name} in {key}"))
    };
    Ok(Token {
        endpoint: match endpoint {
            Endpoint::Required => field("endpoint")?.to_owned(),
            Endpoint::None => String::new(),
        },
        jwt: field("jwt")?.to_owned(),
        expires: token
            .get("exp")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("no exp in {key}"))?,
    })
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
        token(&body, "jwt_qws", Endpoint::Required)
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
        token(&body, "jwt_api", Endpoint::None)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{token, Endpoint};

    #[test]
    fn reads_the_answers_of_both_token_endpoints() {
        // qws/createToken, jwt=jwt_qws
        let body = json!({"jwt_qws": {"exp": 1_791_018_075_u64, "jwt": "ey.qws",
                                      "endpoint": "wss://qws-eu-prod.qobuz.com/ws"}});
        let qws = token(&body, "jwt_qws", Endpoint::Required).expect("a token");
        assert_eq!(qws.endpoint, "wss://qws-eu-prod.qobuz.com/ws");
        assert_eq!(qws.jwt, "ey.qws");
        assert_eq!(qws.expires, 1_791_018_075);

        // qws/refreshToken, jwt=jwt_api: a token, and no endpoint of its own
        let body = json!({"jwt_api": {"exp": 1_791_021_401_u64, "jwt": "ey.api"}});
        let api = token(&body, "jwt_api", Endpoint::None).expect("a token");
        assert_eq!(api.jwt, "ey.api");
        assert_eq!(api.expires, 1_791_021_401);
        assert!(api.endpoint.is_empty());
    }

    #[test]
    fn says_what_is_missing() {
        // An error answer rather than a token, as the API gives for a bad argument.
        let error = json!({"status": "error", "code": 400, "message": "Invalid argument: jwt"});
        let err = token(&error, "jwt_qws", Endpoint::Required).expect_err("no token in there");
        assert!(err.contains("no jwt_qws"), "{err}");

        let no_endpoint = json!({"jwt_qws": {"jwt": "ey", "exp": 1_u64}});
        let err = token(&no_endpoint, "jwt_qws", Endpoint::Required).expect_err("no endpoint");
        assert!(err.contains("endpoint"), "{err}");

        // A string expiry is not an expiry: the device wants seconds as a number.
        let string_exp = json!({"jwt_api": {"jwt": "ey", "exp": "1791021401"}});
        let err = token(&string_exp, "jwt_api", Endpoint::None).expect_err("exp is not a number");
        assert!(err.contains("exp"), "{err}");
    }
}
