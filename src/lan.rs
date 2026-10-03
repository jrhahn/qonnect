//! The LAN handshake the native Qobuz apps use, from the app side.
//!
//! HEOS devices never register themselves with the Qobuz cloud. They advertise
//! `_qobuz-connect._tcp` on the LAN and wait for an app to hand them a session and the tokens to
//! join it with. Until that happens they are invisible to the session, which is why a controller
//! has to do this before it can see anything.

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::json;

use crate::qobuz::Qobuz;

const SERVICE: &str = "_qobuz-connect._tcp.local.";
/// Where the device should talk to the Qobuz API with the token we hand it.
const API_ENDPOINT: &str = "https://www.qobuz.com/api.json/0.2/";

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

/// Hands a device the session and the tokens to join it with.
///
/// The device validates the body strictly: both tokens need an endpoint, the expiry is in
/// seconds, and any field it does not know makes it answer `400 Invalid request structure`.
pub async fn hand_over(qobuz: &Qobuz, device: &Device, session_id: &str) -> Result<(), String> {
    let token = qobuz.connect_token().await?;
    let payload = json!({
        "session_id": session_id,
        "jwt_qconnect": {
            "endpoint": token.endpoint,
            "jwt": token.jwt,
            "exp": token.expires,
        },
        "jwt_api": {
            "endpoint": API_ENDPOINT,
            "jwt": qobuz.user_auth_token(),
            "exp": token.expires,
        },
        "become_active": true,
    });

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
        Ok(())
    } else {
        Err(format!("{} refused the session ({status}): {body}", device.name))
    }
}
