use std::sync::Arc;

use crate::{
    AppConfig, Database, payment_service::PaymentService, rate_limit::RateLimiter,
    repository::Repository, social::SocialPublisher,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub database: Database,
    pub repository: Repository,
    pub payments: PaymentService,
    pub social: SocialPublisher,
    pub creates: Arc<RateLimiter>,
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
            creates: payments.creates(),
            payments,
            social,
        }
    }
}
