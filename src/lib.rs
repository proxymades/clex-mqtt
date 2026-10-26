/// PostgreSQL instance lock shared by serving and offline maintenance.
pub const INSTANCE_LOCK: i64 = 0x434c45584d515454;

pub mod accounts;
pub mod acl;
pub mod auth;
pub mod bridge;
pub mod broker;
pub mod config;
pub mod diagnostics;
pub mod domain;
pub mod metrics;
pub mod migrations;
pub mod monitor;
pub mod protocol;
pub mod scram;
pub mod store;
pub mod tls;
pub mod web;
