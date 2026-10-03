//! The LAN handshake the native Qobuz apps use, from the app side.
//!
//! HEOS devices never register themselves with the Qobuz cloud. They advertise
//! `_qobuz-connect._tcp` on the LAN and wait for an app to hand them a session and the tokens to
//! join it with. Until that happens they are invisible to the session, which is why a controller
//! has to do this before it can see anything.

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::json;

use crate::qobuz::{Qobuz, Token};

const SERVICE: &str = "_qobuz-connect._tcp.local.";
/// Where the device should talk to the Qobuz API with the token we hand it.
const API_ENDPOINT: &str = "https://www.qobuz.com/api.json/0.2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    /// Already carries the path from the advertisement, so only the call is appended.
    pub base: String,
}

/// Collects the devices that answer within `window`. Devices announce on their own schedule, so
/// this always waits the whole window rather than stopping at the first answer.
pub async fn browse(window: Duration) -> Result<Vec<Device>, String> {
    let daemon = ServiceDaemon::new().map_err(|err| format!("mDNS failed: {err}"))?;
    let receiver = daemon
        .browse(SERVICE)
        .map_err(|err| format!("browsing for {SERVICE} failed: {err}"))?;

    let mut devices: Vec<Device> = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, receiver.recv_async()).await {
        let ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let Some(address) = info.get_addresses().iter().find(|address| address.is_ipv4()) else {
            continue;
        };
        // The advertisement says where on the device the Qobuz endpoints live.
        let path = info
            .get_property_val_str("path")
            .unwrap_or("/qobuz")
            .trim_end_matches('/')
            .to_owned();
        let device = Device {
            name: info
                .get_property_val_str("Name")
                .unwrap_or_else(|| info.get_fullname().split('.').next().unwrap_or("device"))
                .to_owned(),
            base: format!("http://{address}:{}{path}", info.get_port()),
        };
        if !devices.contains(&device) {
            tracing::info!(name = %device.name, base = %device.base, "found a device on the LAN");
            devices.push(device);
        }
    }
    let _ = daemon.shutdown();
    Ok(devices)
}

/// What the device wants to hear. Both tokens carry an endpoint, the expiry is in seconds, and
/// there is nothing else: it answers `400 Invalid request structure` to any field it does not
/// know.
fn payload(session_id: &str, qconnect: &Token, api: &Token) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "jwt_qconnect": {
            "endpoint": qconnect.endpoint,
            "jwt": qconnect.jwt,
            "exp": qconnect.expires,
        },
        "jwt_api": {
            "endpoint": API_ENDPOINT,
            "jwt": api.jwt,
            "exp": api.expires,
        },
        "become_active": true,
    })
}

/// The session the device says it is in, if it says anything.
async fn in_session(qobuz: &Qobuz, device: &Device) -> Option<String> {
    let body: serde_json::Value = qobuz
        .client()
        .get(format!("{}/get-connect-info", device.base))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let id = body.get("current_session_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// Hands a device the session and the tokens to join it with, and answers with the session the
/// device is in instead, when it is in another one.
///
/// The device validates the body strictly: both tokens need an endpoint, the expiry is in
/// seconds, and any field it does not know makes it answer `400 Invalid request structure`.
pub async fn hand_over(
    qobuz: &Qobuz,
    device: &Device,
    session_id: &str,
) -> Result<Option<String>, String> {
    // Never take a device that is already in a session. Our tokens come from the web player,
    // which is no Qobuz Connect controller: a device holding them joins, fails every stream with
    // "Too many playback errors" and drops out. A device that an official app has handed a
    // session to holds tokens that do stream, and this session is the account's either way, so
    // leave it alone and control it as it is.
    if let Some(current) = in_session(qobuz, device).await {
        tracing::debug!(name = %device.name, session = %current, "already in a session");
        return Ok((current != session_id).then_some(current));
    }
    let token = qobuz.connect_token().await?;
    let api = qobuz.api_token().await?;
    let payload = payload(session_id, &token, &api);

    let response = qobuz
        .client()
        .post(format!("{}/connect-to-qconnect", device.base))
        .json(&payload)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| format!("{} did not answer: {err}", device.name))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.is_success() {
        tracing::info!(name = %device.name, "handed the session over");
        Ok(None)
    } else {
        Err(format!("{} refused the session ({status}): {body}", device.name))
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::{json, Value};

    use super::{hand_over, payload, Device};
    use crate::qobuz::{Qobuz, Token};

    fn token(endpoint: &str, jwt: &str) -> Token {
        Token {
            endpoint: endpoint.to_owned(),
            jwt: jwt.to_owned(),
            expires: 1_791_018_336,
        }
    }

    /// A device that only answers `get-connect-info`, saying which session it is in.
    async fn device_in(session: &'static str) -> Device {
        let router = Router::new().route(
            "/qobuz/get-connect-info",
            get(move || async move {
                Json(json!({"app_id": "120805696", "current_session_id": session}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Device {
            name: "Marantz PM7000N".to_owned(),
            base: format!("http://127.0.0.1:{port}/qobuz"),
        }
    }

    #[test]
    fn builds_the_body_the_device_accepts() {
        let body = payload(
            "ec13a53a-e766-4fbc-8d2a-9b7179ddace0",
            &token("wss://qws-eu-prod.qobuz.com/ws", "ey.qws"),
            &token("", "ey.api"),
        );

        // Exactly these four fields: the device refuses a body carrying anything else.
        let fields: Vec<&String> = body.as_object().expect("an object").keys().collect();
        assert_eq!(
            fields,
            ["become_active", "jwt_api", "jwt_qconnect", "session_id"]
        );

        // Both tokens need an endpoint, and jwt_api does not bring one of its own.
        assert_eq!(body["jwt_qconnect"]["endpoint"], "wss://qws-eu-prod.qobuz.com/ws");
        assert_eq!(body["jwt_api"]["endpoint"], "https://www.qobuz.com/api.json/0.2");
        assert_eq!(body["jwt_api"]["jwt"], "ey.api");
        assert_eq!(body["become_active"], Value::Bool(true));

        // Seconds, as a number: milliseconds are refused.
        assert_eq!(body["jwt_qconnect"]["exp"], 1_791_018_336_u64);
        assert!(body["jwt_api"]["exp"].is_u64());
    }

    #[tokio::test]
    async fn reports_a_device_that_is_in_another_session() {
        let device = device_in("5494e0ac-f123-42a8-b2b9-9666e5a53f9c").await;
        // Credentials are never used: the handover stops before minting anything.
        let qobuz = Qobuz::new("app".to_owned(), "token".to_owned());

        let other = hand_over(&qobuz, &device, "ec13a53a-e766-4fbc-8d2a-9b7179ddace0")
            .await
            .expect("the device answered");
        assert_eq!(other.as_deref(), Some("5494e0ac-f123-42a8-b2b9-9666e5a53f9c"));
    }

    #[tokio::test]
    async fn leaves_a_device_that_is_already_in_this_session_alone() {
        let session = "ec13a53a-e766-4fbc-8d2a-9b7179ddace0";
        let device = device_in(session).await;
        let qobuz = Qobuz::new("app".to_owned(), "token".to_owned());

        // Nothing to report and nothing done: handing it over again would cost it a reconnection,
        // and would replace tokens that may be the only ones able to stream.
        let other = hand_over(&qobuz, &device, session).await.expect("answered");
        assert_eq!(other, None);
    }
}
