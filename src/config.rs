use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

use bitcoin::Network;
use serde::Deserialize;
use url::Url;

use crate::{AppError, AppResult};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub bitcoin: BitcoinConfig,
    pub lightning: LightningConfig,
    #[serde(default)]
    pub nostr: NostrConfig,
    #[serde(default)]
    pub twitter: TwitterConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub payments: PaymentConfig,
    #[serde(default)]
    pub external: ExternalConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> AppResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            AppError::Config(format!("could not read {}: {error}", path.display()))
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|error| {
            AppError::Config(format!("could not parse {}: {error}", path.display()))
        })?;
        config.database.path = expand_tilde(&config.database.path)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> AppResult<()> {
        if self.bitcoin.sending_wallet_name == self.bitcoin.receiving_wallet_name {
            return Err(AppError::Config(
                "sending and receiving wallets must be different".to_owned(),
            ));
        }
        if self.database.max_connections == 0 {
            return Err(AppError::Config(
                "database.max_connections must be greater than zero".to_owned(),
            ));
        }
        if self.payments.message_max_bytes == 0 {
            return Err(AppError::Config(
                "payments.message_max_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.payments.invoice_expiry_seconds == 0 {
            return Err(AppError::Config(
                "payments.invoice_expiry_seconds must be greater than zero".to_owned(),
            ));
        }
        if self.payments.reconcile_interval_seconds == 0 {
            return Err(AppError::Config(
                "payments.reconcile_interval_seconds must be greater than zero".to_owned(),
            ));
        }
        if self.nostr.private_key_file.is_some() && self.nostr.relays.is_empty() {
            return Err(AppError::Config(
                "nostr.relays must not be empty when Nostr is enabled".to_owned(),
            ));
        }
        self.lightning.validate()?;
        Ok(())
    }
}

fn expand_tilde(path: &Path) -> AppResult<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir()
            .ok_or_else(|| AppError::Config("could not determine the home directory".to_owned()));
    }
    if let Some(suffix) = text.strip_prefix("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| AppError::Config("could not determine the home directory".to_owned()))?;
        return Ok(home.join(suffix));
    }
    Ok(path.to_path_buf())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub address: IpAddr,
    pub port: u16,
    pub public_url: Url,
    pub onion_url: Url,
}

impl ServerConfig {
    #[must_use]
    pub fn bind_address(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    #[serde(default = "default_database_connections")]
    pub max_connections: u32,
    #[serde(default = "default_busy_timeout")]
    pub busy_timeout_seconds: u64,
}

const fn default_database_connections() -> u32 {
    4
}

const fn default_busy_timeout() -> u64 {
    30
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BitcoinConfig {
    #[serde(deserialize_with = "deserialize_network")]
    pub network: Network,
    pub rpc_url: Url,
    pub rpc_user: String,
    pub rpc_password_file: PathBuf,
    pub sending_wallet_name: String,
    pub receiving_wallet_name: String,
    pub wallet_notify_key_file: PathBuf,
}

fn deserialize_network<'de, D>(deserializer: D) -> Result<Network, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "mainnet" | "bitcoin" => Ok(Network::Bitcoin),
        "regtest" => Ok(Network::Regtest),
        _ => Err(serde::de::Error::custom(
            "network must be 'mainnet' or 'regtest'",
        )),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightningConfig {
    pub backend: LightningBackendKind,
    pub lnd: Option<LndConfig>,
    pub ldk_server: Option<LdkServerConfig>,
}

impl LightningConfig {
    fn validate(&self) -> AppResult<()> {
        match self.backend {
            LightningBackendKind::Lnd => {
                self.lnd()?;
            }
            LightningBackendKind::LdkServer => {
                self.ldk_server()?;
            }
        }
        Ok(())
    }

    pub(crate) fn lnd(&self) -> AppResult<&LndConfig> {
        self.lnd.as_ref().ok_or_else(|| {
            AppError::Config("lightning.lnd is required when backend is 'lnd'".to_owned())
        })
    }

    pub(crate) fn ldk_server(&self) -> AppResult<&LdkServerConfig> {
        self.ldk_server.as_ref().ok_or_else(|| {
            AppError::Config(
                "lightning.ldk_server is required when backend is 'ldk-server'".to_owned(),
            )
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum LightningBackendKind {
    Lnd,
    LdkServer,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LndConfig {
    pub rpc_url: Url,
    pub macaroon_file: PathBuf,
    pub tls_cert_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LdkServerConfig {
    pub rpc_url: Url,
    pub config_file: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NostrConfig {
    pub private_key_file: Option<PathBuf>,
    pub relays: Vec<Url>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TwitterConfig {
    pub enabled: bool,
    pub consumer_key_file: Option<PathBuf>,
    pub consumer_secret_file: Option<PathBuf>,
    pub access_token_file: Option<PathBuf>,
    pub access_secret_file: Option<PathBuf>,
    pub banned_words: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub token_file: Option<PathBuf>,
    pub admin_chat_id: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaymentConfig {
    pub message_max_bytes: usize,
    pub application_fee_sats: u64,
    pub private_fee_sats: u64,
    pub non_standard_fee_sats: u64,
    pub invoice_expiry_seconds: u32,
    pub on_chain_expiry_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub create_per_ip_per_minute: u32,
    pub create_global_per_minute: u32,
}

impl Default for PaymentConfig {
    fn default() -> Self {
        Self {
            message_max_bytes: 99_000,
            application_fee_sats: 1_337,
            private_fee_sats: 1_000,
            non_standard_fee_sats: 1_000,
            invoice_expiry_seconds: 300,
            on_chain_expiry_seconds: 7 * 24 * 60 * 60,
            reconcile_interval_seconds: 15,
            create_per_ip_per_minute: 10,
            create_global_per_minute: 120,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalConfig {
    pub mempool_url: Url,
    pub bitcoiner_live_url: Url,
    pub coinbase_url: Url,
    pub esplora_url: Url,
    pub slipstream_url: Url,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            mempool_url: Url::parse("https://mempool.space").expect("static URL is valid"),
            bitcoiner_live_url: Url::parse("https://bitcoiner.live").expect("static URL is valid"),
            coinbase_url: Url::parse("https://api.coinbase.com").expect("static URL is valid"),
            esplora_url: Url::parse("https://mempool.space/api/").expect("static URL is valid"),
            slipstream_url: Url::parse("https://slipstream.mara.com/api/transactions")
                .expect("static URL is valid"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, path::Path};

    use super::*;

    fn valid_config() -> &'static str {
        r#"
[server]
address = "127.0.0.1"
port = 9000
public_url = "https://opreturnbot.com"
onion_url = "http://example.onion"

[database]
path = "/tmp/invoices.sqlite"

[bitcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18443"
rpc_user = "user"
rpc_password_file = "/run/secrets/rpc-password"
sending_wallet_name = "sending"
receiving_wallet_name = "receiving"
wallet_notify_key_file = "/run/secrets/wallet-notify-key"

[lightning]
backend = "lnd"

[lightning.lnd]
rpc_url = "https://127.0.0.1:10009"
macaroon_file = "/run/secrets/macaroon"
tls_cert_file = "/run/secrets/tls.cert"
"#
    }

    #[test]
    fn parses_main_configuration() {
        let config: AppConfig = toml::from_str(valid_config()).unwrap();
        config.validate().unwrap();

        assert_eq!(config.server.address, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(config.bitcoin.network, Network::Regtest);
        assert_eq!(config.lightning.backend, LightningBackendKind::Lnd);
        assert!(config.lightning.lnd.is_some());
        assert!(config.lightning.ldk_server.is_none());
        assert_eq!(config.payments.message_max_bytes, 99_000);
    }

    #[test]
    fn parses_ldk_server_without_lnd() {
        let text = valid_config()
            .replace("backend = \"lnd\"", "backend = \"ldk-server\"")
            .replace(
                r#"[lightning.lnd]
rpc_url = "https://127.0.0.1:10009"
macaroon_file = "/run/secrets/macaroon"
tls_cert_file = "/run/secrets/tls.cert""#,
                r#"[lightning.ldk_server]
rpc_url = "https://127.0.0.1:3002"
config_file = "/tmp/ldk-server.toml""#,
            );
        let config: AppConfig = toml::from_str(&text).unwrap();
        config.validate().unwrap();

        assert_eq!(config.lightning.backend, LightningBackendKind::LdkServer);
        assert!(config.lightning.lnd.is_none());
        assert!(config.lightning.ldk_server.is_some());
    }

    #[test]
    fn rejects_missing_selected_lightning_backend() {
        let text = valid_config().replace(
            r#"[lightning.lnd]
rpc_url = "https://127.0.0.1:10009"
macaroon_file = "/run/secrets/macaroon"
tls_cert_file = "/run/secrets/tls.cert""#,
            "",
        );
        let config: AppConfig = toml::from_str(&text).unwrap();

        let error = config.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: lightning.lnd is required when backend is 'lnd'"
        );
    }

    #[test]
    fn rejects_equal_wallet_names() {
        let text = valid_config().replace(
            "receiving_wallet_name = \"receiving\"",
            "receiving_wallet_name = \"sending\"",
        );
        let config: AppConfig = toml::from_str(&text).unwrap();

        assert!(config.validate().is_err());
    }

    #[test]
    fn leaves_absolute_database_path_unchanged() {
        assert_eq!(
            expand_tilde(Path::new("/var/lib/op-return-bot/invoices.sqlite")).unwrap(),
            Path::new("/var/lib/op-return-bot/invoices.sqlite")
        );
    }
}
