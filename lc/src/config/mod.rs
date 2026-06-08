//! Configuration model and parser entry point.
//!
//! The grammar is nginx-flavoured:
//!
//! ```nginx
//! server {
//!     host        127.0.0.1;
//!     port        8080;
//!     server_name example.com;
//!     error_page  404 /errors/404.html;
//!     client_max_body_size 1m;
//!     route /uploads {
//!         methods   GET POST DELETE;
//!         root      /var/www/uploads;
//!         index     index.html;
//!         autoindex on;
//!         cgi       .py /usr/bin/python3;
//!         redirect  301 https://new.example.com/;
//!     }
//! }
//! ```

use std::collections::HashMap;

mod parser;

pub use parser::parse;

/// Default upload limit when `client_max_body_size` is omitted: 1 MiB.
pub const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024;

/// The whole parsed configuration: every valid `server` block.
#[derive(Debug, Default)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

/// One `server { ... }` block.
#[derive(Debug)]
pub struct ServerConfig {
    /// Bind address as four octets (e.g. `[127, 0, 0, 1]`).
    pub host: [u8; 4],
    /// Original textual host, used for duplicate detection and logging.
    pub host_str: String,
    /// One or more ports this server listens on.
    pub ports: Vec<u16>,
    /// Virtual-host names. Empty means this is the default for its `host:port`.
    pub server_names: Vec<String>,
    /// Custom error pages: status code -> file path.
    pub error_pages: HashMap<u16, String>,
    /// Maximum accepted request body size in bytes.
    pub client_max_body_size: usize,
    /// Route blocks, matched longest-prefix-first by the router (M4).
    pub routes: Vec<Route>,
}

impl ServerConfig {
    fn new() -> Self {
        ServerConfig {
            host: [127, 0, 0, 1],
            host_str: "127.0.0.1".to_string(),
            ports: Vec::new(),
            server_names: Vec::new(),
            error_pages: HashMap::new(),
            client_max_body_size: DEFAULT_MAX_BODY_SIZE,
            routes: Vec::new(),
        }
    }
}

/// A `route <path> { ... }` block.
#[derive(Debug)]
pub struct Route {
    /// URL prefix this route matches (e.g. `/uploads`).
    pub path: String,
    /// Accepted HTTP methods. Empty means "no restriction" (router decides).
    pub methods: Vec<String>,
    /// Filesystem directory the path is rooted at.
    pub root: Option<String>,
    /// Default file served when the request targets a directory.
    pub index: Option<String>,
    /// Whether to generate a directory listing when no index file exists.
    pub autoindex: bool,
    /// CGI handlers: file extension (with dot) -> interpreter binary.
    pub cgi: HashMap<String, String>,
    /// Optional redirect: (status code, target URL).
    pub redirect: Option<(u16, String)>,
}

impl Route {
    fn new(path: String) -> Self {
        Route {
            path,
            methods: Vec::new(),
            root: None,
            index: None,
            autoindex: false,
            cgi: HashMap::new(),
            redirect: None,
        }
    }
}

/// Result of parsing: the valid config plus every error encountered.
///
/// We deliberately keep going past errors so the caller can log all of them
/// and decide whether the surviving servers are enough to start (the audit
/// requires that one broken `server` block does not take down the rest).
pub struct ParseOutcome {
    pub config: Config,
    pub errors: Vec<String>,
}
