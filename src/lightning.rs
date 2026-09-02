use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use fedimint_tonic_lnd::{invoicesrpc, lnrpc, tonic};
use futures_util::{Stream, StreamExt, stream};
use ldk_server_client::{
    client::LdkServerClient,
    config::{load_config, resolve_api_key, resolve_base_url, resolve_cert_path},
    ldk_server_grpc::{
        api::{Bolt11ReceiveRequest, GetNodeInfoRequest, GetPaymentDetailsRequest},
        events::{EventEnvelope, event_envelope},
        types::{
            Bolt11InvoiceDescription, PaymentStatus, bolt11_invoice_description, payment_kind,
        },
    },
};

use crate::{
    AppError, AppResult,
    config::{LightningBackendKind, LightningConfig},
    domain::LightningBackend,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub backend: LightningBackend,
}

/// The state of an invoice as reported by the Lightning backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvoiceState {
    /// The invoice can still be paid.
    Open,
    /// The invoice was paid in full. The preimage can be empty when the
    /// backend does not expose it.
    Settled { preimage: Vec<u8> },
    /// The invoice expired, was cancelled, or is unknown to the backend, so
    /// it can no longer be paid.
    Canceled,
}

/// An invoice update from a backend subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvoiceEvent {
    /// The invoice was paid in full.
    Settled {
        payment_hash: String,
        preimage: Vec<u8>,
    },
}

pub type InvoiceStream = Pin<Box<dyn Stream<Item = AppResult<InvoiceEvent>> + Send>>;

#[async_trait]
pub trait Lightning: Send + Sync {
    async fn create_invoice(
        &self,
        amount_msats: u64,
        description: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice>;

    async fn create_invoice_with_description_hash(
        &self,
        amount_msats: u64,
        description_hash: [u8; 32],
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice>;

    async fn block_height(&self) -> AppResult<u64>;

    async fn invoice_state(&self, payment_hash: &str) -> AppResult<InvoiceState>;

    /// Streams invoices as they settle. The stream ends when the connection
    /// drops, and the caller reconnects.
    async fn subscribe_invoices(&self) -> AppResult<InvoiceStream>;

    /// Cancels an open invoice so that it can no longer be paid. Backends
    /// that cannot cancel invoices return `Ok` and log the limitation.
    async fn cancel_invoice(&self, payment_hash: &str) -> AppResult<()>;

    async fn node_uri(&self) -> AppResult<String>;

    fn backend(&self) -> LightningBackend;
}

pub async fn connect(config: &LightningConfig) -> AppResult<Arc<dyn Lightning>> {
    match config.backend {
        LightningBackendKind::Lnd => Ok(Arc::new(LndLightning::connect(config.lnd()?).await?)),
        LightningBackendKind::LdkServer => Ok(Arc::new(
            LdkServerLightning::connect(config.ldk_server()?).await?,
        )),
    }
}

struct LndLightning {
    client: fedimint_tonic_lnd::Client,
}

impl LndLightning {
    async fn connect(lnd: &crate::config::LndConfig) -> AppResult<Self> {
        let client = fedimint_tonic_lnd::connect(
            lnd.rpc_url.as_str().to_owned(),
            lnd.tls_cert_file.clone(),
            lnd.macaroon_file.clone(),
        )
        .await
        .map_err(|error| AppError::Upstream(format!("could not connect to LND: {error}")))?;
        Ok(Self { client })
    }

    async fn add_invoice(&self, invoice: lnrpc::Invoice) -> AppResult<CreatedInvoice> {
        let response = self
            .client
            .clone()
            .lightning()
            .add_invoice(invoice)
            .await
            .map_err(|error| AppError::Upstream(format!("LND addinvoice failed: {error}")))?
            .into_inner();
        Ok(CreatedInvoice {
            bolt11: response.payment_request,
            payment_hash: hex::encode(response.r_hash),
            backend: LightningBackend::Lnd,
        })
    }

    async fn get_info(&self) -> AppResult<lnrpc::GetInfoResponse> {
        Ok(self
            .client
            .clone()
            .lightning()
            .get_info(lnrpc::GetInfoRequest {})
            .await
            .map_err(|error| AppError::Upstream(format!("LND getinfo failed: {error}")))?
            .into_inner())
    }
}

fn decode_payment_hash(payment_hash: &str) -> AppResult<Vec<u8>> {
    hex::decode(payment_hash).map_err(|error| {
        AppError::InvalidRequest(format!("payment hash is not valid hex: {error}"))
    })
}

/// Converts an LND invoice update into an event. Only invoices that settled
/// for at least their value produce an event.
fn settled_event(invoice: lnrpc::Invoice) -> Option<InvoiceEvent> {
    if invoice.state != lnrpc::invoice::InvoiceState::Settled as i32 {
        return None;
    }
    let payment_hash = hex::encode(&invoice.r_hash);
    if invoice.amt_paid_msat < invoice.value_msat {
        tracing::warn!(
            payment_hash,
            amt_paid_msat = invoice.amt_paid_msat,
            value_msat = invoice.value_msat,
            "invoice settled below its value; ignoring"
        );
        return None;
    }
    Some(InvoiceEvent::Settled {
        payment_hash,
        preimage: invoice.r_preimage,
    })
}

#[async_trait]
impl Lightning for LndLightning {
    async fn create_invoice(
        &self,
        amount_msats: u64,
        description: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        let value_msat = i64::try_from(amount_msats)
            .map_err(|_| AppError::InvalidRequest("invoice amount is too large".to_owned()))?;
        self.add_invoice(lnrpc::Invoice {
            memo: description.to_owned(),
            value_msat,
            expiry: i64::from(expiry_seconds),
            ..Default::default()
        })
        .await
    }

    async fn create_invoice_with_description_hash(
        &self,
        amount_msats: u64,
        description_hash: [u8; 32],
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        let value_msat = i64::try_from(amount_msats)
            .map_err(|_| AppError::InvalidRequest("invoice amount is too large".to_owned()))?;
        self.add_invoice(lnrpc::Invoice {
            description_hash: description_hash.to_vec(),
            value_msat,
            expiry: i64::from(expiry_seconds),
            ..Default::default()
        })
        .await
    }

    async fn block_height(&self) -> AppResult<u64> {
        Ok(u64::from(self.get_info().await?.block_height))
    }

    async fn invoice_state(&self, payment_hash: &str) -> AppResult<InvoiceState> {
        let lookup = self
            .client
            .clone()
            .lightning()
            .lookup_invoice(lnrpc::PaymentHash {
                r_hash: decode_payment_hash(payment_hash)?,
                ..Default::default()
            })
            .await;
        let response = match lookup {
            Ok(response) => response.into_inner(),
            // An invoice that LND does not know cannot be paid.
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Ok(InvoiceState::Canceled);
            }
            Err(status) => {
                return Err(AppError::Upstream(format!(
                    "LND lookupinvoice failed: {status}"
                )));
            }
        };
        let state = if response.state == lnrpc::invoice::InvoiceState::Settled as i32 {
            InvoiceState::Settled {
                preimage: response.r_preimage,
            }
        } else if response.state == lnrpc::invoice::InvoiceState::Canceled as i32 {
            InvoiceState::Canceled
        } else {
            InvoiceState::Open
        };
        Ok(state)
    }

    async fn subscribe_invoices(&self) -> AppResult<InvoiceStream> {
        let updates = self
            .client
            .clone()
            .lightning()
            .subscribe_invoices(lnrpc::InvoiceSubscription {
                add_index: 0,
                settle_index: 0,
            })
            .await
            .map_err(|error| AppError::Upstream(format!("LND subscribeinvoices failed: {error}")))?
            .into_inner();
        let events = updates.filter_map(|update| async move {
            match update {
                Ok(invoice) => settled_event(invoice).map(Ok),
                Err(status) => Some(Err(AppError::Upstream(format!(
                    "LND invoice stream failed: {status}"
                )))),
            }
        });
        Ok(Box::pin(events))
    }

    async fn cancel_invoice(&self, payment_hash: &str) -> AppResult<()> {
        self.client
            .clone()
            .invoices()
            .cancel_invoice(invoicesrpc::CancelInvoiceMsg {
                payment_hash: decode_payment_hash(payment_hash)?,
            })
            .await
            .map_err(|error| AppError::Upstream(format!("LND cancelinvoice failed: {error}")))?;
        Ok(())
    }

    async fn node_uri(&self) -> AppResult<String> {
        preferred_node_uri(self.get_info().await?.uris).ok_or_else(|| {
            AppError::Upstream("LND does not advertise a public node URI".to_owned())
        })
    }

    fn backend(&self) -> LightningBackend {
        LightningBackend::Lnd
    }
}

struct LdkServerLightning {
    client: LdkServerClient,
}

impl LdkServerLightning {
    async fn connect(ldk: &crate::config::LdkServerConfig) -> AppResult<Self> {
        let path = ldk.config_file.clone();
        let loaded = load_config(&path).map_err(AppError::Config)?;
        let endpoint = ldk_endpoint(&ldk.rpc_url)?;
        let base_url = resolve_base_url(Some(endpoint), Some(&loaded));
        let api_key = resolve_api_key(None, Some(&loaded))
            .ok_or_else(|| AppError::Config("could not find the ldk-server API key".to_owned()))?;
        let cert_path = resolve_cert_path(None, Some(&loaded)).ok_or_else(|| {
            AppError::Config("could not find the ldk-server TLS certificate".to_owned())
        })?;
        let cert = tokio::fs::read(&cert_path).await.map_err(|error| {
            AppError::Config(format!(
                "could not read ldk-server certificate {}: {error}",
                cert_path.display()
            ))
        })?;
        let client = LdkServerClient::new(base_url, api_key, &cert).map_err(|error| {
            AppError::Upstream(format!("could not connect to ldk-server: {error}"))
        })?;
        Ok(Self { client })
    }

    async fn receive(
        &self,
        amount_msats: u64,
        kind: bolt11_invoice_description::Kind,
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        let response = self
            .client
            .bolt11_receive(Bolt11ReceiveRequest {
                amount_msat: Some(amount_msats),
                description: Some(Bolt11InvoiceDescription { kind: Some(kind) }),
                expiry_secs: expiry_seconds,
            })
            .await
            .map_err(|error| {
                AppError::Upstream(format!("ldk-server Bolt11Receive failed: {error}"))
            })?;
        Ok(CreatedInvoice {
            bolt11: response.invoice,
            payment_hash: response.payment_hash,
            backend: LightningBackend::LdkServer,
        })
    }

    async fn node_info(
        &self,
    ) -> AppResult<ldk_server_client::ldk_server_grpc::api::GetNodeInfoResponse> {
        self.client
            .get_node_info(GetNodeInfoRequest {})
            .await
            .map_err(|error| AppError::Upstream(format!("ldk-server GetNodeInfo failed: {error}")))
    }
}

#[async_trait]
impl Lightning for LdkServerLightning {
    async fn create_invoice(
        &self,
        amount_msats: u64,
        description: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        self.receive(
            amount_msats,
            bolt11_invoice_description::Kind::Direct(description.to_owned()),
            expiry_seconds,
        )
        .await
    }

    async fn create_invoice_with_description_hash(
        &self,
        amount_msats: u64,
        description_hash: [u8; 32],
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        self.receive(
            amount_msats,
            bolt11_invoice_description::Kind::Hash(hex::encode(description_hash)),
            expiry_seconds,
        )
        .await
    }

    async fn block_height(&self) -> AppResult<u64> {
        let block = self.node_info().await?.current_best_block.ok_or_else(|| {
            AppError::Upstream("ldk-server did not return its best block".to_owned())
        })?;
        Ok(u64::from(block.height))
    }

    async fn invoice_state(&self, payment_hash: &str) -> AppResult<InvoiceState> {
        let response = self
            .client
            .get_payment_details(GetPaymentDetailsRequest {
                payment_id: payment_hash.to_owned(),
            })
            .await
            .map_err(|error| {
                AppError::Upstream(format!("ldk-server GetPaymentDetails failed: {error}"))
            })?;
        let Some(payment) = response.payment else {
            return Ok(InvoiceState::Open);
        };
        if payment.status == PaymentStatus::Failed as i32 {
            return Ok(InvoiceState::Canceled);
        }
        if payment.status != PaymentStatus::Succeeded as i32 {
            return Ok(InvoiceState::Open);
        }
        let preimage = payment
            .kind
            .and_then(|kind| kind.kind)
            .and_then(|kind| match kind {
                ldk_server_client::ldk_server_grpc::types::payment_kind::Kind::Bolt11(data) => {
                    data.preimage
                }
                _ => None,
            })
            .map(|preimage| hex::decode(preimage).unwrap_or_default())
            .unwrap_or_default();
        Ok(InvoiceState::Settled { preimage })
    }

    async fn subscribe_invoices(&self) -> AppResult<InvoiceStream> {
        let updates = self.client.subscribe_events().await.map_err(|error| {
            AppError::Upstream(format!("ldk-server SubscribeEvents failed: {error}"))
        })?;
        let events = stream::unfold(updates, |mut updates| async move {
            updates.next_message().await.map(|result| {
                let result = result.map_err(|error| {
                    AppError::Upstream(format!("ldk-server event stream failed: {error}"))
                });
                (result, updates)
            })
        })
        .filter_map(|result| async move {
            match result {
                Ok(event) => ldk_settled_event(event).map(Ok),
                Err(error) => Some(Err(error)),
            }
        });
        Ok(Box::pin(events))
    }

    async fn cancel_invoice(&self, payment_hash: &str) -> AppResult<()> {
        tracing::debug!(
            payment_hash,
            "ldk-server cannot cancel a standard invoice; leaving it open"
        );
        Ok(())
    }

    async fn node_uri(&self) -> AppResult<String> {
        preferred_node_uri(self.node_info().await?.node_uris).ok_or_else(|| {
            AppError::Upstream("ldk-server does not advertise a public node URI".to_owned())
        })
    }

    fn backend(&self) -> LightningBackend {
        LightningBackend::LdkServer
    }
}

fn ldk_endpoint(url: &url::Url) -> AppResult<String> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::Config("ldk-server RPC URL has no host".to_owned()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::Config("ldk-server RPC URL has no port".to_owned()))?;
    Ok(format!("{host}:{port}"))
}

fn ldk_settled_event(envelope: EventEnvelope) -> Option<InvoiceEvent> {
    let event_envelope::Event::PaymentReceived(received) = envelope.event? else {
        return None;
    };
    let payment = received.payment?;
    if payment.status != PaymentStatus::Succeeded as i32 {
        return None;
    }
    let payment_kind::Kind::Bolt11(bolt11) = payment.kind?.kind? else {
        return None;
    };
    if bolt11.hash.is_empty() {
        return None;
    }
    let preimage = bolt11
        .preimage
        .and_then(|preimage| hex::decode(preimage).ok())
        .unwrap_or_default();
    Some(InvoiceEvent::Settled {
        payment_hash: bolt11.hash,
        preimage,
    })
}

fn preferred_node_uri(uris: Vec<String>) -> Option<String> {
    uris.iter()
        .find(|uri| uri.contains(".onion"))
        .cloned()
        .or_else(|| uris.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ldk_url_to_client_endpoint() {
        let url = url::Url::parse("https://127.0.0.1:3002").unwrap();
        assert_eq!(ldk_endpoint(&url).unwrap(), "127.0.0.1:3002");
    }

    #[test]
    fn maps_only_fully_paid_settled_invoices_to_events() {
        let settled = lnrpc::Invoice {
            r_hash: vec![0xab; 32],
            r_preimage: vec![7; 32],
            value_msat: 1_000,
            amt_paid_msat: 1_000,
            state: lnrpc::invoice::InvoiceState::Settled as i32,
            ..Default::default()
        };
        assert_eq!(
            settled_event(settled.clone()),
            Some(InvoiceEvent::Settled {
                payment_hash: "ab".repeat(32),
                preimage: vec![7; 32],
            })
        );
        let open = lnrpc::Invoice {
            state: lnrpc::invoice::InvoiceState::Open as i32,
            ..settled.clone()
        };
        assert_eq!(settled_event(open), None);
        let underpaid = lnrpc::Invoice {
            amt_paid_msat: 999,
            ..settled
        };
        assert_eq!(settled_event(underpaid), None);
    }

    #[test]
    fn maps_ldk_payment_received_events_to_invoices() {
        let envelope = EventEnvelope {
            event: Some(event_envelope::Event::PaymentReceived(
                ldk_server_client::ldk_server_grpc::events::PaymentReceived {
                    payment: Some(ldk_server_client::ldk_server_grpc::types::Payment {
                        kind: Some(ldk_server_client::ldk_server_grpc::types::PaymentKind {
                            kind: Some(payment_kind::Kind::Bolt11(
                                ldk_server_client::ldk_server_grpc::types::Bolt11 {
                                    hash: "ab".repeat(32),
                                    preimage: Some("07".repeat(32)),
                                    ..Default::default()
                                },
                            )),
                        }),
                        status: PaymentStatus::Succeeded as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        };
        assert_eq!(
            ldk_settled_event(envelope),
            Some(InvoiceEvent::Settled {
                payment_hash: "ab".repeat(32),
                preimage: vec![7; 32],
            })
        );
    }

    #[test]
    fn prefers_tor_node_uri() {
        assert_eq!(
            preferred_node_uri(vec![
                "node@example.com:9735".to_owned(),
                "node@example.onion:9735".to_owned()
            ])
            .as_deref(),
            Some("node@example.onion:9735")
        );
    }
}
