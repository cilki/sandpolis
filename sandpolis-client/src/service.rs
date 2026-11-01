//! Client-side access to the estate's background services.
//!
//! Every instance that hosts services writes a
//! [`ServiceData`] row per service, which the sync engine replicates here, so
//! reads are synchronous scans of the client's local database. Enabling,
//! disabling, and prodding a service goes the other way, over the control stream.

use anyhow::Result;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::realm::RealmName;
use sandpolis_instance::service::ServiceData;
use sandpolis_instance::service::control::{ServiceControlRequest, ServiceControlResponse};
use sandpolis_macros::Stream;
use tokio::sync::mpsc::Sender;

/// The sync model id for services.
pub fn service_model_id() -> u32 {
    <ServiceData as Model>::native_model_id()
}

/// Subscribe to live service updates across every instance. Services are few and
/// their rows are small, so there's no reason to narrow this to one instance.
pub fn subscribe() {
    crate::sync::subscribe(service_model_id(), None);
}

/// Drop the subscription created by [`subscribe`].
pub fn unsubscribe() {
    crate::sync::unsubscribe(service_model_id(), None);
}

/// Every service in the client's local database.
pub fn query_services() -> Result<Vec<ServiceData>> {
    let Some(database) = crate::sync::client_database() else {
        return Ok(vec![]);
    };
    let realm = database.realm(RealmName::default())?;
    let r = realm.r_transaction()?;
    Ok(r.scan()
        .primary::<ServiceData>()?
        .all()?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Every service belonging to `layer`, ordered for stable display.
pub fn query_layer_services(layer: &str) -> Result<Vec<ServiceData>> {
    let mut services: Vec<ServiceData> = query_services()?
        .into_iter()
        .filter(|s| s.layer == layer)
        .collect();
    // The scan returns rows in primary-key (random id) order, which would
    // otherwise reshuffle the list under the user every refresh.
    services.sort_by(|a, b| (&a.name, &a._instance_id).cmp(&(&b.name, &b._instance_id)));
    Ok(services)
}

/// Client side of the control stream. Errors are surfaced in the log; the
/// service's new state arrives through the sync subscription.
#[derive(Stream, Default)]
pub struct ServiceControlRequester;

impl sandpolis_instance::network::StreamRequester for ServiceControlRequester {
    type In = ServiceControlResponse;
    type Out = ServiceControlRequest;

    async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
        if let ServiceControlResponse::Error(e) = response {
            tracing::warn!(error = %e, "Service control error");
        }
        Ok(())
    }
}

/// Send a one-shot control request to whichever instance hosts the service.
///
/// `target` is the hosting instance. When it's the server we're connected to,
/// the stream stays local; otherwise it's addressed so the server relays it to
/// the agent.
fn send_request(target: InstanceId, request: ServiceControlRequest) {
    let Some(conn) = crate::sync::connection() else {
        tracing::warn!("Not connected to a server");
        return;
    };
    let relay = (conn.data.read().remote_instance != Some(target)).then_some(target);

    crate::sync::spawn(async move {
        let (id, tx) = conn.streams.register_to(ServiceControlRequester, relay);
        let payload = match serde_cbor::to_vec(&request) {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to encode service control request");
                return;
            }
        };
        let _ = tx.send(StreamMessage::routed(id, payload, relay)).await;
        conn.close_stream(id);
    });
}

/// Enable or disable the service `key` on `target`.
pub fn set_enabled(target: InstanceId, key: String, enabled: bool) {
    send_request(target, ServiceControlRequest::SetEnabled { key, enabled });
}

/// Ask the service `key` on `target` to run its next pass now.
pub fn run_now(target: InstanceId, key: String) {
    send_request(target, ServiceControlRequest::RunNow { key });
}
