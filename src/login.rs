//! `qonnect login`: trades an email and a password for the two values the API wants on every
//! call, and writes them to the config file.
//!
//! The app id is not a secret: the web player ships it in its bundle, which is where this reads
//! it from, so a new one is picked up whenever Qobuz rotates it.

use std::io::Write as _;
use std::path::Path;

use reqwest::Client;
use serde_json::Value;

const LOGIN_PAGE: &str = "https://play.qobuz.com/login";
/// Where the production app id sits in the bundle, past the integration and recette ones.
const PRODUCTION_APP_ID: &str = "production:{api:{appId:\"";
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

    let email = prompt("Qobuz email: ")?;
    let password = rpassword::prompt_password("Qobuz password: ").map_err(|err| err.to_string())?;

    let token = user_auth_token(&client, &app_id, &email, &password).await?;
    write(config, &app_id, &token)?;
    println!("written to {}", config.display());
    Ok(())
}

/// Reads `appId` out of the web player bundle the login page points at.
async fn app_id(client: &Client) -> Result<String, String> {
    let page = get(client, LOGIN_PAGE).await?;
    let bundle = between(&page, "/resources/", "/bundle.js")
        .ok_or("no bundle in the login page, the web player has changed")?;
    let bundle = get(client, &format!("https://play.qobuz.com/resources/{bundle}/bundle.js")).await?;
    // The bundle carries one app id per environment and the integration one comes first, so
    // anchor on production rather than taking the first match.
    between(&bundle, PRODUCTION_APP_ID, "\"")
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| "no production appId in the bundle, the web player has changed".to_owned())
}

async fn user_auth_token(
    client: &Client,
    app_id: &str,
    email: &str,
    password: &str,
) -> Result<String, String> {
    let digest = format!("{:x}", md5::compute(password));
    let response = client
        .get("https://www.qobuz.com/api.json/0.2/user/login")
        .header("X-App-Id", app_id)
        .query(&[
            ("app_id", app_id),
            ("email", email),
            ("password", &digest),
        ])
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|err| format!("{status}, and the answer was not JSON: {err}"))?;
    if !status.is_success() {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!("login refused ({status}): {message}"));
    }
    body.get("user_auth_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "no user_auth_token in the answer".to_owned())
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

async fn get(client: &Client, url: &str) -> Result<String, String> {
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

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    std::io::stdout().flush().map_err(|err| err.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| err.to_string())?;
    Ok(line.trim().to_owned())
}

/// The text between two markers, searching from the first.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)?.checked_add(open.len())?;
    let rest = haystack.get(start..)?;
    rest.get(..rest.find(close)?)
}

#[cfg(test)]
mod tests {
    use super::{between, PRODUCTION_APP_ID};

    #[test]
    fn reads_markers_and_survives_missing_ones() {
        let bundle = r#"c={integration:{api:{appId:"377257687",appSecret:"f686"},x:1},"#.to_owned()
            + r#"recette:{api:{appId:"724307056"},x:1},production:{api:{appId:"798273057"}}}"#;
        // The integration app id comes first in the real bundle, production is the one we want.
        assert_eq!(between(&bundle, "appId:\"", "\""), Some("377257687"));
        assert_eq!(between(&bundle, PRODUCTION_APP_ID, "\""), Some("798273057"));
        assert_eq!(between("<a href=/resources/8.2.0-b034/bundle.js>", "/resources/", "/bundle.js"),
                   Some("8.2.0-b034"));
        assert_eq!(between(&bundle, "appId:\"", "@"), None);
        assert_eq!(between(&bundle, "nothing", "\""), None);
    }
}
