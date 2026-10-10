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
    #[serde(default)]
    pub moderation: ModerationConfig,
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
        self.moderation.validate()?;
        self.lightning.validate()
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
    /// Older configurations name the backend. ldk-server is the only one.
    #[serde(default)]
    pub backend: Option<String>,
    pub ldk_server: LdkServerConfig,
}

impl LightningConfig {
    fn validate(&self) -> AppResult<()> {
        match self.backend.as_deref() {
            None | Some("ldk-server") => Ok(()),
            Some("lnd") => Err(AppError::Config(
                "LND support was removed; use ldk-server".to_owned(),
            )),
            Some(other) => Err(AppError::Config(format!(
                "unknown Lightning backend '{other}'; use ldk-server"
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LdkServerConfig {
    pub rpc_url: Url,
    pub config_file: PathBuf,
    /// A macaroon to use instead of the admin macaroon from the ldk-server
    /// data directory.
    #[serde(default)]
    pub macaroon_file: Option<PathBuf>,
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

/// File uploads are refused until `url` and `api_key_file` are both set.
/// `allow_unscreened` stores files without asking the decision service.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModerationConfig {
    pub allow_unscreened: bool,
    pub url: Option<Url>,
    pub api_key_file: Option<PathBuf>,
    pub confidence_floor: f64,
    pub timeout_seconds: u64,
    pub max_pdf_pages: usize,
}

impl ModerationConfig {
    fn validate(&self) -> AppResult<()> {
        if !(self.confidence_floor > 0.0 && self.confidence_floor <= 1.0) {
            return Err(AppError::Config(
                "moderation.confidence_floor must be greater than 0 and at most 1".to_owned(),
            ));
        }
        if self.timeout_seconds == 0 {
            return Err(AppError::Config(
                "moderation.timeout_seconds must be greater than zero".to_owned(),
            ));
        }
        if self.max_pdf_pages == 0 {
            return Err(AppError::Config(
                "moderation.max_pdf_pages must be greater than zero".to_owned(),
            ));
        }
        if self.allow_unscreened {
            return Ok(());
        }
        match (&self.url, &self.api_key_file) {
            (None, None) | (Some(_), Some(_)) => Ok(()),
            (Some(_), None) => Err(AppError::Config(
                "moderation.api_key_file is required when moderation.url is set".to_owned(),
            )),
            (None, Some(_)) => Err(AppError::Config(
                "moderation.url is required when moderation.api_key_file is set".to_owned(),
            )),
        }
    }
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            allow_unscreened: false,
            url: None,
            api_key_file: None,
            confidence_floor: 0.9,
            timeout_seconds: 15,
            max_pdf_pages: 8,
        }
    }
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

[lightning.ldk_server]
rpc_url = "https://127.0.0.1:3002"
config_file = "/tmp/ldk-server.toml"
"#
    }

    #[test]
    fn parses_main_configuration() {
        let config: AppConfig = toml::from_str(valid_config()).unwrap();
        config.validate().unwrap();

        assert_eq!(config.server.address, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(config.bitcoin.network, Network::Regtest);
        assert!(config.lightning.ldk_server.macaroon_file.is_none());
        assert_eq!(config.payments.message_max_bytes, 99_000);
        assert!(!config.moderation.allow_unscreened);
        assert!(config.moderation.url.is_none());
        assert_eq!(
            config.moderation.confidence_floor.to_bits(),
            0.9_f64.to_bits()
        );
        assert_eq!(config.moderation.max_pdf_pages, 8);
    }

    #[test]
    fn parses_ldk_server_macaroon_file() {
        let text = valid_config().replace(
            "config_file = \"/tmp/ldk-server.toml\"",
            "config_file = \"/tmp/ldk-server.toml\"\nmacaroon_file = \"/run/secrets/ldk-server.macaroon\"",
        );
        let config: AppConfig = toml::from_str(&text).unwrap();

        assert_eq!(
            config.lightning.ldk_server.macaroon_file,
            Some(PathBuf::from("/run/secrets/ldk-server.macaroon"))
        );
    }

    #[test]
    fn accepts_the_old_ldk_server_backend_key() {
        let text = valid_config().replace(
            "[lightning.ldk_server]",
            "[lightning]\nbackend = \"ldk-server\"\n\n[lightning.ldk_server]",
        );
        toml::from_str::<AppConfig>(&text)
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn rejects_the_removed_lnd_backend() {
        let text = valid_config().replace(
            "[lightning.ldk_server]",
            "[lightning]\nbackend = \"lnd\"\n\n[lightning.ldk_server]",
        );
        let error = toml::from_str::<AppConfig>(&text)
            .unwrap()
            .validate()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: LND support was removed; use ldk-server"
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
    fn rejects_a_confidence_floor_outside_the_unit_interval() {
        let text = format!("{}\n\n[moderation]\nconfidence_floor = 0\n", valid_config());
        let error = toml::from_str::<AppConfig>(&text)
            .unwrap()
            .validate()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: moderation.confidence_floor must be greater than 0 and at most 1"
        );
    }

    #[test]
    fn requires_the_moderation_key_when_the_url_is_set() {
        let text = format!(
            "{}\n\n[moderation]\nurl = \"https://example.test/v1/systemone\"\n",
            valid_config()
        );
        let error = toml::from_str::<AppConfig>(&text)
            .unwrap()
            .validate()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: moderation.api_key_file is required when moderation.url is set"
        );
    }

    #[test]
    fn allows_files_without_a_check_only_when_asked() {
        let text = format!(
            "{}\n\n[moderation]\nallow_unscreened = true\n",
            valid_config()
        );
        let config: AppConfig = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        assert!(config.moderation.allow_unscreened);
    }

    #[test]
    fn leaves_absolute_database_path_unchanged() {
        assert_eq!(
            expand_tilde(Path::new("/var/lib/op-return-bot/invoices.sqlite")).unwrap(),
            Path::new("/var/lib/op-return-bot/invoices.sqlite")
        );
    }
}
