use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::{Deserialize, Serialize};

use crate::{AppState, payment_service::CreateRequest};

#[derive(Clone)]
pub struct McpServer {
    state: AppState,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CreateInput {
    #[schemars(description = "The UTF-8 message to write, up to 99,000 bytes")]
    message: String,
    #[serde(default)]
    #[schemars(description = "Do not publish the message to Twitter or Nostr")]
    no_twitter: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StatusInput {
    #[schemars(description = "The Lightning payment hash in hexadecimal")]
    r_hash: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ViewInput {
    #[schemars(description = "The Bitcoin transaction ID in hexadecimal")]
    tx_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InvoiceOutput {
    invoice: String,
    r_hash: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct UnifiedOutput {
    invoice: String,
    address: String,
    amount_sats: i64,
    r_hash: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StatusOutput {
    status: String,
    message: String,
    tx_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ViewOutput {
    tx_id: String,
    message: String,
}

#[tool_router]
impl McpServer {
    fn new(state: AppState) -> Self {
        Self { state }
    }

    #[tool(
        name = "create_op_return",
        description = "Create a Lightning invoice to write an OP_RETURN message on Bitcoin"
    )]
    async fn create_op_return(
        &self,
        Parameters(input): Parameters<CreateInput>,
    ) -> Result<Json<InvoiceOutput>, McpError> {
        self.state
            .creates
            .check("mcp")
            .map_err(|error| mcp_error(&error))?;
        let created = self
            .state
            .payments
            .create_invoice(&CreateRequest {
                message: input.message.into_bytes(),
                no_twitter: input.no_twitter,
            })
            .await
            .map_err(|error| mcp_error(&error))?;
        let invoice = created
            .record
            .invoice
            .ok_or_else(|| McpError::internal_error("created payment has no invoice", None))?;
        Ok(Json(InvoiceOutput {
            invoice: invoice.bolt11,
            r_hash: invoice.payment_hash,
        }))
    }

    #[tool(
        name = "create_unified_payment",
        description = "Create a Lightning and on-chain payment request for an OP_RETURN message"
    )]
    async fn create_unified_payment(
        &self,
        Parameters(input): Parameters<CreateInput>,
    ) -> Result<Json<UnifiedOutput>, McpError> {
        self.state
            .creates
            .check("mcp")
            .map_err(|error| mcp_error(&error))?;
        let created = self
            .state
            .payments
            .create_unified(&CreateRequest {
                message: input.message.into_bytes(),
                no_twitter: input.no_twitter,
            })
            .await
            .map_err(|error| mcp_error(&error))?;
        let invoice = created
            .record
            .invoice
            .ok_or_else(|| McpError::internal_error("created payment has no invoice", None))?;
        let on_chain = created.record.on_chain.ok_or_else(|| {
            McpError::internal_error("created payment has no on-chain address", None)
        })?;
        Ok(Json(UnifiedOutput {
            invoice: invoice.bolt11,
            address: on_chain.address,
            amount_sats: on_chain.expected_amount_sats,
            r_hash: invoice.payment_hash,
        }))
    }

    #[tool(
        name = "check_payment_status",
        description = "Check the payment and broadcast status of an OP_RETURN request"
    )]
    async fn check_payment_status(
        &self,
        Parameters(input): Parameters<StatusInput>,
    ) -> Result<Json<StatusOutput>, McpError> {
        let record = match self
            .state
            .repository
            .find_by_payment_hash(&input.r_hash)
            .await
        {
            Ok(record) => record,
            Err(crate::AppError::NotFound(_)) => {
                return Ok(Json(StatusOutput {
                    status: "not_found".to_owned(),
                    message: "Invoice not found".to_owned(),
                    tx_id: None,
                }));
            }
            Err(error) => return Err(mcp_error(&error)),
        };
        let (status, message) = if record.request.txid.is_some() {
            ("confirmed", record.request.message_text())
        } else if record.invoice.is_some_and(|invoice| invoice.paid)
            || record
                .on_chain
                .is_some_and(|payment| payment.txid.is_some())
        {
            ("paid", "Payment received, awaiting broadcast".to_owned())
        } else {
            ("unpaid", "Awaiting payment".to_owned())
        };
        Ok(Json(StatusOutput {
            status: status.to_owned(),
            message,
            tx_id: record.request.txid,
        }))
    }

    #[tool(
        name = "view_message",
        description = "View the OP_RETURN message for a transaction created by OP_RETURN Bot"
    )]
    async fn view_message(
        &self,
        Parameters(input): Parameters<ViewInput>,
    ) -> Result<Json<ViewOutput>, McpError> {
        let request = self
            .state
            .repository
            .find_by_txid(&input.tx_id)
            .await
            .map_err(|error| mcp_error(&error))?;
        Ok(Json(ViewOutput {
            tx_id: input.tx_id,
            message: request.message_text(),
        }))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("op-return-bot", env!("CARGO_PKG_VERSION"))
            .with_title("OP_RETURN Bot")
            .with_description("Write messages to Bitcoin through OP_RETURN outputs");
        info.instructions = Some(
            "Create a payment, pay it, then check status until a Bitcoin txid is returned."
                .to_owned(),
        );
        info
    }
}

#[must_use]
pub fn service(state: AppState) -> StreamableHttpService<McpServer, LocalSessionManager> {
    let server = &state.config.server;
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    let mut origins = Vec::new();
    for url in [&server.public_url, &server.onion_url] {
        if let Some(host) = url.host_str() {
            hosts.push(host.to_owned());
        }
        origins.push(url.origin().ascii_serialization());
    }
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .with_allowed_origins(origins);
    StreamableHttpService::new(
        move || Ok(McpServer::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

fn mcp_error(error: &crate::AppError) -> McpError {
    match error {
        crate::AppError::InvalidRequest(_) | crate::AppError::NotFound(_) => {
            McpError::invalid_params(error.to_string(), None)
        }
        crate::AppError::RateLimited => McpError::invalid_request(error.to_string(), None),
        _ => {
            tracing::error!(%error, "MCP request failed");
            McpError::internal_error("internal error", None)
        }
    }
}
