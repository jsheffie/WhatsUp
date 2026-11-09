pub mod browser;
pub mod config;
pub mod instance;
pub mod probe;
pub mod server;

pub const DEFAULT_CONFIG_JSON: &str = include_str!("../config/whatsup.json");
pub const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
pub const PREFERRED_PORT: u16 = 47654;
