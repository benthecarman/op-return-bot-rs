use std::path::PathBuf;

use clap::Parser;
use op_return_bot::{
    AppConfig, AppState, Database, bitcoin_rpc::BitcoinClient, lightning,
    payment_service::PaymentService, repository::Repository, social::SocialPublisher, web,
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "op-return-bot.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Both rustls crypto providers are in the dependency tree, so one must
    // be chosen before any TLS client is built.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("op_return_bot=info,tower_http=info")),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let config = AppConfig::load(&args.config)?;
    let database = Database::connect(&config.database).await?;
    database.migrate().await?;

    let bind_address = config.server.bind_address();
    let bitcoin = BitcoinClient::connect(&config.bitcoin).await?;
    let lightning = lightning::connect(&config.lightning).await?;
    let social = SocialPublisher::connect(&config).await?;
    let config = std::sync::Arc::new(config);
    let repository = Repository::new(database.clone());
    let creates = std::sync::Arc::new(op_return_bot::rate_limit::RateLimiter::new(
        config.payments.create_per_ip_per_minute,
        config.payments.create_global_per_minute,
        std::time::Duration::from_mins(1),
    ));
    let payments = PaymentService::new(
        config.clone(),
        repository.clone(),
        bitcoin,
        lightning,
        social.clone(),
        creates.clone(),
    )?;
    tokio::spawn(payments.clone().run_reconciler());
    tokio::spawn(payments.clone().run_lightning_watch());
    tokio::spawn(social.clone().run_telegram_bot(
        payments.clone(),
        repository,
        config.server.public_url.clone(),
    ));
    let state = AppState::new((*config).clone(), database, payments, social);
    let router = web::router(state);
    let listener = TcpListener::bind(bind_address).await?;

    info!(%bind_address, "OP_RETURN Bot listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
