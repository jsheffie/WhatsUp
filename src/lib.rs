pub mod browser;
pub mod cli;
pub mod config;
pub mod herdr;
pub mod http;
pub mod instance;
pub mod probe;
pub mod server;
pub mod socks;
pub mod start;
pub mod stop;
pub mod tunnel;

pub const DEFAULT_CONFIG_JSON: &str = include_str!("../config/whatsup.json");
pub const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
pub const PREFERRED_PORT: u16 = 47654;
