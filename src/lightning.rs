use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use ldk_server_client::{
    client::LdkServerClient,
    config::{load_config, resolve_base_url, resolve_cert_path, resolve_macaroon},
    ldk_server_grpc::{
        api::{
            Bolt11ReceiveRequest, Bolt12ReceiveRequest, GetNodeInfoRequest,
            GetPaymentDetailsRequest, ListPaymentsRequest,
        },
        events::{EventEnvelope, event_envelope},
        types::{
            Bolt11InvoiceDescription, Payment, PaymentDirection, PaymentStatus,
            bolt11_invoice_description, payment_kind,
        },
    },
};

use crate::{AppError, AppResult, config::LightningConfig, domain::LightningBackend};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub backend: LightningBackend,
}

/// A fixed-amount BOLT12 offer for one request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedOffer {
    pub offer: String,
    pub offer_id: String,
}

/// A received payment for a BOLT12 offer. The payment hash is only known
/// once the payer requests an invoice, so offers are matched by ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfferPayment {
    pub offer_id: String,
    pub payment_hash: String,
    pub amount_msats: u64,
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
    /// A BOLT12 offer was paid.
    OfferPaid(OfferPayment),
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

    async fn create_offer(
        &self,
        amount_msats: u64,
        description: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedOffer>;

    /// Lists successful BOLT12 offer payments last updated at or after
    /// `since`, a Unix time in seconds.
    async fn offer_payments_since(&self, since: u64) -> AppResult<Vec<OfferPayment>>;

    async fn block_height(&self) -> AppResult<u64>;

    async fn invoice_state(&self, payment_hash: &str) -> AppResult<InvoiceState>;

    /// Streams invoices and offers as they are paid. The stream ends when the
    /// connection drops, and the caller reconnects.
    async fn subscribe_invoices(&self) -> AppResult<InvoiceStream>;

    async fn node_uri(&self) -> AppResult<String>;

    fn backend(&self) -> LightningBackend;
}

pub async fn connect(config: &LightningConfig) -> AppResult<Arc<dyn Lightning>> {
    Ok(Arc::new(
        LdkServerLightning::connect(&config.ldk_server).await?,
    ))
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
        let macaroon_override = match &ldk.macaroon_file {
            Some(path) => Some(
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(|error| {
                        AppError::Config(format!(
                            "could not read ldk-server macaroon {}: {error}",
                            path.display()
                        ))
                    })?
                    .trim()
                    .to_owned(),
            ),
            None => None,
        };
        let macaroon = resolve_macaroon(macaroon_override, Some(&loaded))
            .map_err(AppError::Config)?
            .ok_or_else(|| AppError::Config("could not find the ldk-server macaroon".to_owned()))?;
        let cert_path = resolve_cert_path(None, Some(&loaded)).ok_or_else(|| {
            AppError::Config("could not find the ldk-server TLS certificate".to_owned())
        })?;
        let cert = tokio::fs::read(&cert_path).await.map_err(|error| {
            AppError::Config(format!(
                "could not read ldk-server certificate {}: {error}",
                cert_path.display()
            ))
        })?;
        let client = LdkServerClient::new(base_url, macaroon, &cert).map_err(|error| {
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

    async fn create_offer(
        &self,
        amount_msats: u64,
        description: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedOffer> {
        let response = self
            .client
            .bolt12_receive(Bolt12ReceiveRequest {
                description: description.to_owned(),
                amount_msat: Some(amount_msats),
                expiry_secs: Some(expiry_seconds),
                quantity: None,
            })
            .await
            .map_err(|error| {
                AppError::Upstream(format!("ldk-server Bolt12Receive failed: {error}"))
            })?;
        Ok(CreatedOffer {
            offer: response.offer,
            offer_id: response.offer_id,
        })
    }

    async fn offer_payments_since(&self, since: u64) -> AppResult<Vec<OfferPayment>> {
        let mut found = Vec::new();
        let mut page_token = None;
        // ldk-server lists payments newest first, so stop at the first page
        // that holds nothing as recent as `since`.
        for _ in 0..MAX_PAYMENT_PAGES {
            let response = self
                .client
                .list_payments(ListPaymentsRequest { page_token })
                .await
                .map_err(|error| {
                    AppError::Upstream(format!("ldk-server ListPayments failed: {error}"))
                })?;
            let recent = response
                .payments
                .iter()
                .any(|payment| payment.latest_update_timestamp >= since);
            found.extend(
                response
                    .payments
                    .into_iter()
                    .filter(|payment| payment.latest_update_timestamp >= since)
                    .filter_map(offer_payment),
            );
            match response.next_page_token {
                Some(token) if recent => page_token = Some(token),
                _ => return Ok(found),
            }
        }
        tracing::warn!(
            pages = MAX_PAYMENT_PAGES,
            "stopped listing ldk-server payments at the page limit"
        );
        Ok(found)
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

/// Pages of ldk-server payments read in one scan for offer payments.
const MAX_PAYMENT_PAGES: usize = 50;

fn ldk_settled_event(envelope: EventEnvelope) -> Option<InvoiceEvent> {
    let event_envelope::Event::PaymentReceived(received) = envelope.event? else {
        return None;
    };
    let payment = received.payment?;
    if payment.status != PaymentStatus::Succeeded as i32 {
        return None;
    }
    let kind = payment.kind.as_ref()?.kind.as_ref()?;
    if matches!(kind, payment_kind::Kind::Bolt12Offer(_)) {
        return offer_payment(payment).map(InvoiceEvent::OfferPaid);
    }
    let payment_kind::Kind::Bolt11(bolt11) = kind else {
        return None;
    };
    if bolt11.hash.is_empty() {
        return None;
    }
    let preimage = bolt11
        .preimage
        .as_deref()
        .and_then(|preimage| hex::decode(preimage).ok())
        .unwrap_or_default();
    Some(InvoiceEvent::Settled {
        payment_hash: bolt11.hash.clone(),
        preimage,
    })
}

/// A successful inbound payment for a BOLT12 offer, or `None` for any
/// other payment.
fn offer_payment(payment: Payment) -> Option<OfferPayment> {
    if payment.status != PaymentStatus::Succeeded as i32
        || payment.direction != PaymentDirection::Inbound as i32
    {
        return None;
    }
    let payment_kind::Kind::Bolt12Offer(offer) = payment.kind?.kind? else {
        return None;
    };
    if offer.offer_id.is_empty() {
        return None;
    }
    Some(OfferPayment {
        offer_id: offer.offer_id,
        payment_hash: offer.hash.unwrap_or_default(),
        amount_msats: payment.amount_msat.unwrap_or_default(),
    })
}

/// Prefers a clearnet address, which more peers can reach, over an onion.
fn preferred_node_uri(uris: Vec<String>) -> Option<String> {
    uris.iter()
        .find(|uri| !uri.contains(".onion"))
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

    fn offer_received(direction: PaymentDirection, status: PaymentStatus) -> EventEnvelope {
        EventEnvelope {
            event: Some(event_envelope::Event::PaymentReceived(
                ldk_server_client::ldk_server_grpc::events::PaymentReceived {
                    payment: Some(Payment {
                        kind: Some(ldk_server_client::ldk_server_grpc::types::PaymentKind {
                            kind: Some(payment_kind::Kind::Bolt12Offer(
                                ldk_server_client::ldk_server_grpc::types::Bolt12Offer {
                                    hash: Some("ab".repeat(32)),
                                    offer_id: "cd".repeat(32),
                                    ..Default::default()
                                },
                            )),
                        }),
                        amount_msat: Some(5_000_000),
                        direction: direction as i32,
                        status: status as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }
    }

    #[test]
    fn maps_ldk_offer_payments_by_offer_id() {
        assert_eq!(
            ldk_settled_event(offer_received(
                PaymentDirection::Inbound,
                PaymentStatus::Succeeded
            )),
            Some(InvoiceEvent::OfferPaid(OfferPayment {
                offer_id: "cd".repeat(32),
                payment_hash: "ab".repeat(32),
                amount_msats: 5_000_000,
            }))
        );
        assert_eq!(
            ldk_settled_event(offer_received(
                PaymentDirection::Inbound,
                PaymentStatus::Pending
            )),
            None
        );
        assert_eq!(
            ldk_settled_event(offer_received(
                PaymentDirection::Outbound,
                PaymentStatus::Succeeded
            )),
            None
        );
    }

    #[test]
    fn prefers_clearnet_node_uri() {
        assert_eq!(
            preferred_node_uri(vec![
                "node@example.onion:9735".to_owned(),
                "node@example.com:9735".to_owned()
            ])
            .as_deref(),
            Some("node@example.com:9735")
        );
        assert_eq!(
            preferred_node_uri(vec!["node@example.onion:9735".to_owned()]).as_deref(),
            Some("node@example.onion:9735")
        );
    }
}
