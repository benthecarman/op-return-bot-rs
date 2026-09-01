use std::sync::Arc;

use crate::{
    AppConfig, Database, payment_service::PaymentService, repository::Repository,
    social::SocialPublisher,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub database: Database,
    pub repository: Repository,
    pub payments: PaymentService,
    pub social: SocialPublisher,
}

impl AppState {
    #[must_use]
    pub fn new(
        config: AppConfig,
        database: Database,
        payments: PaymentService,
        social: SocialPublisher,
    ) -> Self {
        let repository = Repository::new(database.clone());
        Self {
            config: Arc::new(config),
            database,
            repository,
            payments,
            social,
        }
    }
}
