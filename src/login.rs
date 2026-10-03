//! `qonnect login`: the browser redirect the Qobuz web player uses, run against a listener of
//! our own.
//!
//! Qobuz dropped password logins — `user/login` with an email and an md5 answers 401 for every
//! shape. What is left is `signin/oauth`: send the browser there, it comes back to a redirect
//! url with a code, and the code buys a user auth token. The password never passes through here.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse as _};
use axum::Router;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::oneshot;

const LOGIN_PAGE: &str = "https://play.qobuz.com/login";
const API: &str = "https://www.qobuz.com/api.json/0.2";
const SIGNIN: &str = "https://www.qobuz.com/signin/oauth";
/// Where the production app id sits in the bundle, past the integration and recette ones.
const PRODUCTION_APP_ID: &str = "production:{api:{appId:\"";
/// The web player ships this constant and hands it back with the code.
const PRIVATE_KEY: &str = "6lz8C03UDIC7";
/// Varnish answers some clients with a 403, so look like the browser the bundle is written for.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

pub async fn run(config: &Path) -> Result<(), String> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| err.to_string())?;

    let app_id = app_id(&client).await?;
    println!("app id {app_id}, from the web player bundle");

    let (code, redirect) = authorize(&app_id).await?;
    let token = exchange(&client, &app_id, &code, &redirect).await?;
    write(config, &app_id, &token)?;
    println!("written to {}", config.display());
    Ok(())
}

/// Reads `appId` out of the web player bundle the login page points at.
async fn app_id(client: &Client) -> Result<String, String> {
    let page = fetch(client, LOGIN_PAGE).await?;
    let bundle = between(&page, "/resources/", "/bundle.js")
        .ok_or("no bundle in the login page, the web player has changed")?;
    let bundle = fetch(client, &format!("https://play.qobuz.com/resources/{bundle}/bundle.js")).await?;
    // The bundle carries one app id per environment and the integration one comes first, so
    // anchor on production rather than taking the first match.
    between(&bundle, PRODUCTION_APP_ID, "\"")
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| "no production appId in the bundle, the web player has changed".to_owned())
}

/// Serves one redirect on a port of its own and sends the browser to Qobuz. Returns the code and
/// the redirect url it came back to, which the exchange has to repeat.
async fn authorize(app_id: &str) -> Result<(String, String), String> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .map_err(|err| format!("cannot listen for the redirect: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| err.to_string())?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/oauth/callback");

    let (tx, rx) = oneshot::channel();
    // A fallback, not a route: Qobuz is free to come back to any path, and a 404 would lose the
    // code without a word.
    let router = Router::new()
        .fallback(callback)
        .with_state(std::sync::Arc::new(std::sync::Mutex::new(Some(tx))));

    let url = format!(
        "{SIGNIN}?ext_app_id={app_id}&redirect_url={}",
        encode(&redirect)
    );
    println!("\nOpening {url}\n\nIf no browser opens, paste that into one.");
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    let server = axum::serve(listener, router);
    let code = tokio::select! {
        result = server => return Err(result.map_or_else(
            |err| format!("the redirect listener failed: {err}"),
            |()| "the redirect listener stopped".to_owned(),
        )),
        code = rx => code.map_err(|_| "no code came back".to_owned())?,
        () = sleep(Duration::from_secs(300)) => {
            return Err("no redirect within five minutes".to_owned())
        }
    };
    code.map(|code| (code, redirect))
}

type Sender = std::sync::Arc<std::sync::Mutex<Option<oneshot::Sender<Result<String, String>>>>>;

/// What Qobuz redirects the browser to once the user has signed in. Qobuz calls the code
/// `code_autorisation`; the other names are there in case that changes. When the query holds
/// nothing, ask the browser for the fragment, since a `#code=...` never reaches a server.
async fn callback(
    State(sender): State<Sender>,
    uri: axum::http::Uri,
    Query(query): Query<HashMap<String, String>>,
) -> axum::response::Response {
    println!("redirect: {uri}");

    if let Some(code) = ["code_autorisation", "code", "authorization_code", "auth_code"]
        .iter()
        .find_map(|key| query.get(*key))
    {
        finish(&sender, Ok(code.clone()));
        return Html("<title>qonnect</title><p>Signed in. You can close this tab.").into_response();
    }
    if let Some(error) = query.get("error").or_else(|| query.get("error_description")) {
        finish(&sender, Err(format!("Qobuz sent back an error: {error}")));
        return Html("<title>qonnect</title><p>Qobuz refused. Check the terminal.")
            .into_response();
    }
    if query.contains_key("qonnect_fragment") {
        finish(
            &sender,
            Err(format!("the redirect carried no code, in neither the query nor the fragment: {uri}")),
        );
        return Html("<title>qonnect</title><p>No code in the redirect. Check the terminal.")
            .into_response();
    }
    // Nothing in the query: bounce the fragment back as one, then decide.
    Html(FRAGMENT_BOUNCE).into_response()
}

/// A fragment never reaches the server, so let the browser replay it as a query.
const FRAGMENT_BOUNCE: &str = r"<title>qonnect</title><p>Signing in…<script>
const hash = location.hash.slice(1);
const query = location.search ? location.search + '&' : '?';
location.replace(location.pathname + query + 'qonnect_fragment=1' + (hash ? '&' + hash : ''));
</script>";

fn finish(sender: &Sender, result: Result<String, String>) {
    if let Ok(mut slot) = sender.lock() {
        if let Some(sender) = slot.take() {
            let _ = sender.send(result);
        }
    }
}

/// Trades the code for a user auth token.
async fn exchange(
    client: &Client,
    app_id: &str,
    code: &str,
    redirect: &str,
) -> Result<String, String> {
    let response = client
        .get(format!("{API}/oauth/callback"))
        .header("X-App-Id", app_id)
        .query(&[
            ("code", code),
            ("private_key", PRIVATE_KEY),
            ("app_id", app_id),
            ("redirect_url", redirect),
        ])
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|err| format!("{status}, and the answer was not JSON: {err}"))?;
    // The web player reads a token and a user id out of this; take whichever name it arrives under.
    ["user_auth_token", "token"]
        .iter()
        .find_map(|key| body.get(key).and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| {
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no token and no reason in the answer");
            format!("the code was refused ({status}): {message}")
        })
}

/// Replaces the credentials and leaves every other line of an existing config alone.
fn write(config: &Path, app_id: &str, token: &str) -> Result<(), String> {
    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let kept: String = std::fs::read_to_string(config)
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            let key = line.split('=').next().unwrap_or_default().trim();
            key != "app_id" && key != "user_auth_token"
        })
        .map(|line| format!("{line}\n"))
        .collect();

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(config).map_err(|err| err.to_string())?;
    write!(file, "app_id = {app_id}\nuser_auth_token = {token}\n{kept}")
        .map_err(|err| err.to_string())
}

async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

async fn fetch(client: &Client, url: &str) -> Result<String, String> {
    client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| err.to_string())?
        .text()
        .await
        .map_err(|err| err.to_string())
}

/// Percent encoding for a url that goes in a query parameter.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The text between two markers, searching from the first.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)?.checked_add(open.len())?;
    let rest = haystack.get(start..)?;
    rest.get(..rest.find(close)?)
}

#[cfg(test)]
mod tests {
    use super::{between, encode, PRODUCTION_APP_ID};

    #[test]
    fn reads_markers_and_survives_missing_ones() {
        let bundle = r#"c={integration:{api:{appId:"377257687",appSecret:"f686"},x:1},"#.to_owned()
            + r#"recette:{api:{appId:"724307056"},x:1},production:{api:{appId:"798273057"}}}"#;
        // The integration app id comes first in the real bundle, production is the one we want.
        assert_eq!(between(&bundle, "appId:\"", "\""), Some("377257687"));
        assert_eq!(between(&bundle, PRODUCTION_APP_ID, "\""), Some("798273057"));
        assert_eq!(between(&bundle, "appId:\"", "@"), None);
        assert_eq!(between(&bundle, "nothing", "\""), None);
    }

    #[test]
    fn encodes_a_redirect_url() {
        assert_eq!(
            encode("http://127.0.0.1:7777/oauth/callback"),
            "http%3A%2F%2F127.0.0.1%3A7777%2Foauth%2Fcallback"
        );
    }
}
