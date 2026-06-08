use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::str::FromStr;

use super::{Config, ParseOutcome, Route, ServerConfig};

// Tokenizer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Open,  // {
    Close, // }
    Semi,  // ;
}

struct Lexed {
    tok: Tok,
    line: usize,
}

fn tokenize(input: &str) -> Vec<Lexed> {
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut word = String::new();
    let mut word_line = 1usize;
    let mut in_comment = false;

    macro_rules! flush {
        () => {
            if !word.is_empty() {
                out.push(Lexed {
                    tok: Tok::Word(std::mem::take(&mut word)),
                    line: word_line,
                });
            }
        };
    }

    for c in input.chars() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
                line += 1;
            }
            continue;
        }
        match c {
            '#' => {
                flush!();
                in_comment = true;
            }
            '{' | '}' | ';' => {
                flush!();
                let tok = match c {
                    '{' => Tok::Open,
                    '}' => Tok::Close,
                    _ => Tok::Semi,
                };
                out.push(Lexed { tok, line });
            }
            c if c.is_whitespace() => {
                flush!();
                if c == '\n' {
                    line += 1;
                }
            }
            _ => {
                if word.is_empty() {
                    word_line = line;
                }
                word.push(c);
            }
        }
    }
    flush!();
    out
}

// Parser

struct Parser {
    toks: Vec<Lexed>,
    pos: usize,
    errors: Vec<String>,
}

impl Parser {
    fn new(toks: Vec<Lexed>) -> Self {
        Parser {
            toks,
            pos: 0,
            errors: Vec::new(),
        }
    }

    fn peek(&self) -> Option<&Lexed> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Lexed> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        // reborrow because the borrow above is tied to the increment
        self.toks.get(self.pos - 1)
    }

    fn error(&mut self, line: usize, msg: impl Into<String>) {
        self.errors.push(format!("line {}: {}", line, msg.into()));
    }

    /// Top level: a sequence of `server { ... }` blocks.
    fn parse(&mut self) -> Vec<ServerConfig> {
        let mut servers = Vec::new();
        while let Some(lx) = self.peek() {
            match &lx.tok {
                Tok::Word(w) if w == "server" => {
                    self.advance();
                    if let Some(s) = self.parse_server_block() {
                        servers.push(s);
                    }
                }
                other => {
                    let line = lx.line;
                    let desc = describe(other);
                    self.error(line, format!("expected 'server', found {desc}"));
                    self.advance();
                }
            }
        }
        servers
    }

    /// `{ <directives and routes> }`
    fn parse_server_block(&mut self) -> Option<ServerConfig> {
        if !self.expect_open("server") {
            return None;
        }
        let mut server = ServerConfig::new();
        loop {
            match self.peek() {
                None => {
                    self.error(self.last_line(), "unterminated 'server' block (missing '}')");
                    break;
                }
                Some(lx) if lx.tok == Tok::Close => {
                    self.advance();
                    break;
                }
                Some(lx) => match &lx.tok {
                    Tok::Word(w) if w == "route" => {
                        if let Some(route) = self.parse_route_block() {
                            server.routes.push(route);
                        }
                    }
                    Tok::Word(_) => self.parse_directive(&mut server),
                    other => {
                        let line = lx.line;
                        let desc = describe(other);
                        self.error(line, format!("unexpected {desc} in 'server' block"));
                        self.advance();
                    }
                },
            }
        }
        if server.ports.is_empty() {
            self.error(self.last_line(), "'server' block has no 'port'");
            return None;
        }
        Some(server)
    }

    /// `route <path> { <directives> }`
    fn parse_route_block(&mut self) -> Option<Route> {
        let route_line = self.peek().map(|l| l.line).unwrap_or(0);
        self.advance(); // consume 'route'

        let path = match self.advance() {
            Some(Lexed { tok: Tok::Word(p), .. }) => p.clone(),
            _ => {
                self.error(route_line, "'route' requires a path");
                return None;
            }
        };
        if !self.expect_open("route") {
            return None;
        }

        let mut route = Route::new(path);
        loop {
            match self.peek() {
                None => {
                    self.error(self.last_line(), "unterminated 'route' block (missing '}')");
                    break;
                }
                Some(lx) if lx.tok == Tok::Close => {
                    self.advance();
                    break;
                }
                Some(lx) => match &lx.tok {
                    Tok::Word(_) => self.parse_route_directive(&mut route),
                    other => {
                        let line = lx.line;
                        let desc = describe(other);
                        self.error(line, format!("unexpected {desc} in 'route' block"));
                        self.advance();
                    }
                },
            }
        }
        Some(route)
    }

    /// Read `name arg arg ... ;` and return (name, args, line).
    fn read_directive(&mut self) -> Option<(String, Vec<String>, usize)> {
        let (name, line) = match self.advance() {
            Some(Lexed { tok: Tok::Word(w), line }) => (w.clone(), *line),
            _ => return None,
        };
        let mut args = Vec::new();
        loop {
            match self.peek() {
                Some(lx) if lx.tok == Tok::Semi => {
                    self.advance();
                    break;
                }
                Some(Lexed { tok: Tok::Word(w), .. }) => {
                    args.push(w.clone());
                    self.advance();
                }
                _ => {
                    // Hit a brace or EOF before ';' — report and stop without
                    // consuming the brace so block parsing can recover.
                    self.error(line, format!("missing ';' after '{name}' directive"));
                    break;
                }
            }
        }
        Some((name, args, line))
    }

    fn parse_directive(&mut self, server: &mut ServerConfig) {
        let (name, args, line) = match self.read_directive() {
            Some(d) => d,
            None => return,
        };
        match name.as_str() {
            "host" => match args.first() {
                Some(h) => match Ipv4Addr::from_str(h) {
                    Ok(ip) => {
                        server.host = ip.octets();
                        server.host_str = h.clone();
                    }
                    Err(_) => self.error(line, format!("invalid host '{h}'")),
                },
                None => self.error(line, "'host' requires an address"),
            },
            "port" | "listen" => {
                if args.is_empty() {
                    self.error(line, "'port' requires at least one value");
                }
                for a in &args {
                    match a.parse::<u16>() {
                        Ok(p) => server.ports.push(p),
                        Err(_) => self.error(line, format!("invalid port '{a}'")),
                    }
                }
            }
            "server_name" => {
                if args.is_empty() {
                    self.error(line, "'server_name' requires at least one name");
                }
                server.server_names.extend(args);
            }
            "error_page" => {
                // <code...> <path>
                if args.len() < 2 {
                    self.error(line, "'error_page' requires a code and a path");
                } else {
                    let path = args.last().unwrap().clone();
                    for code in &args[..args.len() - 1] {
                        match code.parse::<u16>() {
                            Ok(c) => {
                                server.error_pages.insert(c, path.clone());
                            }
                            Err(_) => self.error(line, format!("invalid error code '{code}'")),
                        }
                    }
                }
            }
            "client_max_body_size" => match args.first() {
                Some(s) => match parse_size(s) {
                    Ok(n) => server.client_max_body_size = n,
                    Err(e) => self.error(line, e),
                },
                None => self.error(line, "'client_max_body_size' requires a value"),
            },
            other => self.error(line, format!("unknown directive '{other}'")),
        }
    }

    fn parse_route_directive(&mut self, route: &mut Route) {
        let (name, args, line) = match self.read_directive() {
            Some(d) => d,
            None => return,
        };
        match name.as_str() {
            "methods" => {
                if args.is_empty() {
                    self.error(line, "'methods' requires at least one method");
                }
                route.methods = args.into_iter().map(|m| m.to_uppercase()).collect();
            }
            "root" => match args.first() {
                Some(r) => route.root = Some(r.clone()),
                None => self.error(line, "'root' requires a path"),
            },
            "index" => match args.first() {
                Some(i) => route.index = Some(i.clone()),
                None => self.error(line, "'index' requires a filename"),
            },
            "autoindex" => match args.first().map(|s| s.as_str()) {
                Some("on") => route.autoindex = true,
                Some("off") => route.autoindex = false,
                _ => self.error(line, "'autoindex' must be 'on' or 'off'"),
            },
            "cgi" => {
                if args.len() < 2 {
                    self.error(line, "'cgi' requires an extension and an interpreter");
                } else {
                    route.cgi.insert(args[0].clone(), args[1].clone());
                }
            }
            "redirect" => {
                if args.len() < 2 {
                    self.error(line, "'redirect' requires a code and a URL");
                } else {
                    match args[0].parse::<u16>() {
                        Ok(code) => route.redirect = Some((code, args[1].clone())),
                        Err(_) => self.error(line, format!("invalid redirect code '{}'", args[0])),
                    }
                }
            }
            other => self.error(line, format!("unknown route directive '{other}'")),
        }
    }

    fn expect_open(&mut self, ctx: &str) -> bool {
        match self.peek() {
            Some(lx) if lx.tok == Tok::Open => {
                self.advance();
                true
            }
            Some(lx) => {
                let line = lx.line;
                let desc = describe(&lx.tok);
                self.error(line, format!("expected '{{' after '{ctx}', found {desc}"));
                false
            }
            None => {
                self.error(self.last_line(), format!("expected '{{' after '{ctx}'"));
                false
            }
        }
    }

    fn last_line(&self) -> usize {
        self.toks.last().map(|l| l.line).unwrap_or(0)
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Word(w) => format!("'{w}'"),
        Tok::Open => "'{'".to_string(),
        Tok::Close => "'}'".to_string(),
        Tok::Semi => "';'".to_string(),
    }
}

/// Parse a size with an optional `k`/`m`/`g` suffix (case-insensitive) into bytes.
fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size value".to_string());
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        other => return Err(format!("invalid size suffix '{other}'")),
    };
    num.trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid size '{s}'"))
        .and_then(|n| {
            n.checked_mul(mult)
                .ok_or_else(|| format!("size '{s}' overflows"))
        })
}

// Public entry point + cross-server validation

/// Parse a config string. Always returns the surviving servers plus the list
/// of all errors encountered (parse errors and duplicate-binding conflicts).
pub fn parse(input: &str) -> ParseOutcome {
    let toks = tokenize(input);
    let mut parser = Parser::new(toks);
    let servers = parser.parse();
    let mut errors = parser.errors;

    // Reject duplicate (host, port, server_name) bindings. Sharing a host:port
    // across servers with *different* names is legal virtual hosting; a true
    // duplicate (same triple, including the empty default name) is an error.
    let mut seen: HashMap<(String, u16, String), ()> = HashMap::new();
    let mut kept = Vec::new();
    for s in servers {
        let names: Vec<String> = if s.server_names.is_empty() {
            vec![String::new()]
        } else {
            s.server_names.clone()
        };
        let mut conflict = false;
        for &port in &s.ports {
            for name in &names {
                let key = (s.host_str.clone(), port, name.clone());
                if seen.contains_key(&key) {
                    let shown = if name.is_empty() { "(default)" } else { name };
                    errors.push(format!(
                        "duplicate binding {}:{} for server_name {}",
                        s.host_str, port, shown
                    ));
                    conflict = true;
                } else {
                    seen.insert(key, ());
                }
            }
        }
        if conflict {
            // Drop the conflicting server but keep parsing/serving the rest.
            continue;
        }
        kept.push(s);
    }

    ParseOutcome {
        config: Config { servers: kept },
        errors,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_server_block() {
        let src = r#"
            server {
                host 127.0.0.1;
                port 8080;
                server_name example.com;
                error_page 404 /errors/404.html;
                client_max_body_size 2m;
                route /uploads {
                    methods GET POST DELETE;
                    root /var/www/uploads;
                    index index.html;
                    autoindex on;
                    cgi .py /usr/bin/python3;
                    redirect 301 https://example.com/;
                }
            }
        "#;
        let out = parse(src);
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.config.servers.len(), 1);
        let s = &out.config.servers[0];
        assert_eq!(s.host, [127, 0, 0, 1]);
        assert_eq!(s.ports, vec![8080]);
        assert_eq!(s.server_names, vec!["example.com"]);
        assert_eq!(s.error_pages.get(&404).map(String::as_str), Some("/errors/404.html"));
        assert_eq!(s.client_max_body_size, 2 * 1024 * 1024);
        assert_eq!(s.routes.len(), 1);
        let r = &s.routes[0];
        assert_eq!(r.path, "/uploads");
        assert_eq!(r.methods, vec!["GET", "POST", "DELETE"]);
        assert_eq!(r.root.as_deref(), Some("/var/www/uploads"));
        assert!(r.autoindex);
        assert_eq!(r.cgi.get(".py").map(String::as_str), Some("/usr/bin/python3"));
        assert_eq!(r.redirect, Some((301, "https://example.com/".to_string())));
    }

    #[test]
    fn supports_multiple_ports_and_comments() {
        let src = r#"
            # a leading comment
            server {
                port 8080 8081; # inline comment
                port 9090;
            }
        "#;
        let out = parse(src);
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.config.servers[0].ports, vec![8080, 8081, 9090]);
    }

    #[test]
    fn detects_duplicate_binding() {
        let src = r#"
            server { port 8080; }
            server { port 8080; }
        "#;
        let out = parse(src);
        assert_eq!(out.config.servers.len(), 1, "second server should be dropped");
        assert!(out.errors.iter().any(|e| e.contains("duplicate binding")), "{:?}", out.errors);
    }

    #[test]
    fn virtual_hosts_share_a_port() {
        let src = r#"
            server { port 8080; server_name a.com; }
            server { port 8080; server_name b.com; }
        "#;
        let out = parse(src);
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.config.servers.len(), 2);
    }

    #[test]
    fn collects_errors_and_keeps_valid_servers() {
        let src = r#"
            server { port notaport; }
            server { server_name lonely.com; }
            server { port 8080; }
        "#;
        let out = parse(src);
        // first: invalid port -> no ports -> dropped; second: no port -> dropped;
        // third: valid.
        assert_eq!(out.config.servers.len(), 1);
        assert_eq!(out.config.servers[0].ports, vec![8080]);
        assert!(out.errors.iter().any(|e| e.contains("invalid port")));
        assert!(out.errors.iter().any(|e| e.contains("no 'port'")));
    }

    #[test]
    fn reports_missing_semicolon() {
        let src = "server { port 8080 }";
        let out = parse(src);
        assert!(out.errors.iter().any(|e| e.contains("missing ';'")), "{:?}", out.errors);
    }

    #[test]
    fn size_suffixes() {
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("1k"), Ok(1024));
        assert_eq!(parse_size("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("1g"), Ok(1024 * 1024 * 1024));
        assert!(parse_size("abc").is_err());
    }
}
