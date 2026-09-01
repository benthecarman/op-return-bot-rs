pub mod agent_content;
pub mod bitcoin_rpc;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod lightning;
pub mod mcp;
pub mod payment_service;
pub mod pricing;
pub mod repository;
pub mod social;
pub mod state;
pub mod web;

pub use config::AppConfig;
pub use db::Database;
pub use error::{AppError, AppResult};
pub use state::AppState;
