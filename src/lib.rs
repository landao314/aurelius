//! [aurelius](https://github.com/euclio/aurelius) is a complete solution for live-previewing
//! markdown as HTML.
//!
//! This crate provides a server that can render and update an HTML preview of markdown without a
//! client-side refresh. Upon receiving an HTTP request, the server responds with an HTML page
//! containing a rendering of supplied markdown. Client-side JavaScript then initiates a WebSocket
//! connection which allows the server to push changes to the client.
//!
//! This crate was designed to power [vim-markdown-composer], a markdown preview plugin for
//! [Neovim](http://neovim.io), but it may be used to implement similar plugins for any editor.
//! See [vim-markdown-composer] for a real-world usage example.
//!
//! # Example
//!
//! ```no_run
//! use aurelius::Server;
//!
//! let mut server = Server::bind("localhost:0")?;
//! println!("listening on {}", server.addr());
//!
//! server.open_browser()?;
//!
//! server.send(String::from("# Hello, world"));
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```
//!
//! # Acknowledgments
//! This crate is inspired by suan's
//! [instant-markdown-d](https://github.com/suan/instant-markdown-d).
//!
//! # Why the name?
//! "Aurelius" is a Roman *gens* (family name) shared by many famous Romans, including emperor
//! Marcus Aurelius, one of the "Five Good Emperors." The gens itself originates from the Latin
//! *aureus* meaning "golden." Also, tell me that "Markdown Aurelius" isn't a great pun.
//!
//! <cite>[Aurelia (gens) on Wikipedia](https://en.wikipedia.org/wiki/Aurelia_(gens))</cite>.
//!
//! [vim-markdown-composer]: https://github.com/euclio/vim-markdown-composer

#![warn(missing_debug_implementations)]
#![warn(missing_docs)]

use std::convert::Infallible;
use std::fs;
use std::io::{self, prelude::*};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::thread::JoinHandle;

use buf_redux::BufReader;
use crossbeam_utils::thread as crossbeam_thread;
use handlebars::Handlebars;
use httparse::{Request, Status, EMPTY_HEADER};
use hyper::service::{make_service_fn, service_fn};
use include_dir::{include_dir, Dir};
use log::*;
use pulldown_cmark::{Options, Parser};
use serde::Serialize;
use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::sync::broadcast::{self, Sender};
use tungstenite::{protocol::Role, Message, WebSocket};
use url::Url;

use crate::id_map::IdMap;

pub mod r#async;
mod id_map;
mod renderer;
mod service;

use renderer::Renderer;
use service::WebsocketBroadcastService;

const STATIC_FILES: Dir = include_dir!("static");

/// Markdown preview server.
///
/// Listens for HTTP connections and serves a page containing a live markdown preview. The page
/// contains JavaScript to open a websocket connection back to the server for rendering updates.
#[derive(Debug)]
pub struct Server<R> {
    local_addr: SocketAddr,
    tx: Sender<String>,
    renderer: R,
}

#[derive(Error, Debug)]
pub enum Error {

}

impl<R> Server<R>
where
    R: Renderer,
{
    /// Binds the server to a specified address.
    ///
    /// Binding to port 0 will request a port assignment from the OS. Use `addr()` to query the
    /// assigned port.
    pub async fn bind(addr: &SocketAddr, renderer: R) -> anyhow::Result<Self> {
        let (tx, _) = broadcast::channel::<String>(16);

        let html_tx = tx.clone();
        let make_service = make_service_fn(move |_conn| {
            let html_tx = html_tx.clone();

            async move {
                Ok::<_, Infallible>(WebsocketBroadcastService {
                    html_tx,
                })
            }
        });

        let http_server = hyper::Server::try_bind(addr)?.serve(make_service);

        let local_addr = http_server.local_addr();
        info!("listening on {:?}", local_addr);

        tokio::spawn(http_server);

        Ok(Server {
            local_addr,
            tx,
            renderer,
        })
    }

    /// Returns the socket address that the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Publish a new buffer to be rendered by the server.
    ///
    /// The buffer will be rendered with the configured renderer and then broadcasted to all
    /// connected websocket clients.
    ///
    /// Returns errors from the underlying renderer.
    pub async fn send(&self, buffer: &str) -> anyhow::Result<()> {
        let mut output = Vec::with_capacity(buffer.len());

        self.renderer.render(&buffer, &mut output)?;

        let _ = self.tx.send(String::from_utf8(output).unwrap());

        Ok(())
    }
}

enum Signal {
    NewMarkdown,
    Close,
}

#[derive(Debug)]
struct Config {
    static_root: Option<PathBuf>,
    highlight_theme: String,
    css_links: Vec<Url>,
    custom_styles: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            static_root: None,
            highlight_theme: String::from("github"),
            css_links: vec![],
            custom_styles: vec![],
        }
    }
}

fn url_path_to_file_path(path: &str) -> PathBuf {
    path.trim_start_matches('/').split('/').collect()
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    use matches::assert_matches;
    use tungstenite::handshake::client::Request;
    use tungstenite::Message;
    use tungstenite::WebSocket;
    use tokio::net::{lookup_host, ToSocketAddrs};

    use crate::renderer::MarkdownRenderer;

    use super::Server;

    fn assert_websocket_closed<S: Read + Write>(websocket: &mut WebSocket<S>) {
        loop {
            match websocket.read_message() {
                Ok(Message::Close(_)) => (),
                Err(tungstenite::Error::ConnectionClosed) => break,
                other => panic!("unexpected connection state: {:?}", other),
            }
        }
    }

    #[test]
    fn uri_path_to_file_path() {
        assert_eq!(
            super::url_path_to_file_path("/file.txt"),
            Path::new("file.txt")
        );
        assert_eq!(
            super::url_path_to_file_path("/a/b/c/d"),
            vec!["a", "b", "c", "d"].iter().collect::<PathBuf>(),
        );
    }

    async fn new_server() -> anyhow::Result<Server<MarkdownRenderer>> {
        let addr = lookup_host("localhost:0").await?.next().unwrap();
        Ok(Server::bind(&addr, MarkdownRenderer).await?)
    }

    #[tokio::test]
    async fn connect_http() -> anyhow::Result<()> {
        let server = new_server().await?;

        let body = reqwest::get(&format!("http://{}", server.addr()))
            .await?
            .text()
            .await?;

        assert!(body.contains("<html>"));

        Ok(())
    }

    #[tokio::test]
    async fn connect_websocket() -> anyhow::Result<()> {
        let server = new_server().await?;

        async_tungstenite::tokio::connect_async(format!("ws://{}", server.addr())).await?;

        Ok(())
    }

    #[tokio::test]
    async fn send_with_no_clients() -> anyhow::Result<()> {
        let server = new_server().await?;

        server.send("This shouldn't hang").await?;

        Ok(())
    }

    /*

    #[test]
    fn send_html() -> Result<(), Box<dyn Error>> {
        let mut server = Server::bind("localhost:0")?;
        let addr = server.addr();

        let req = Request {
            url: format!("ws://{}", addr).parse()?,
            extra_headers: None,
        };

        let (mut websocket, _) = tungstenite::connect(req)?;

        server.send(String::from("<p>Hello, world!</p>"))?;
        let message = websocket.read_message()?;
        assert_eq!(message.to_text()?, "<p>Hello, world!</p>");

        server.send(String::from("<p>Goodbye, world!</p>"))?;
        let message = websocket.read_message()?;
        assert_eq!(message.to_text()?, "<p>Goodbye, world!</p>");

        Ok(())
    }

    #[test]
    fn send_markdown() -> Result<(), Box<dyn Error>> {
        let mut server = Server::bind("localhost:0")?;
        let addr = server.addr();

        let req = Request {
            url: format!("ws://{}", addr).parse()?,
            extra_headers: None,
        };

        let (mut websocket, _) = tungstenite::connect(req)?;

        server.send(String::from("*Hello*"))?;
        let message = websocket.read_message()?;
        assert_eq!(message.to_text()?.trim(), "<p><em>Hello</em></p>");

        Ok(())
    }

    #[test]
    fn close_websockets_on_drop() -> Result<(), Box<dyn Error>> {
        let server = Server::bind("localhost:0")?;
        let addr = server.addr();

        let req = Request {
            url: format!("ws://{}", addr).parse()?,
            extra_headers: None,
        };

        let (mut websocket, _) = tungstenite::connect(req).unwrap();

        drop(server);

        assert_websocket_closed(&mut websocket);

        Ok(())
    }

    #[test]
    fn queue_html_if_no_clients() -> Result<(), Box<dyn Error>> {
        let mut server = Server::bind("localhost:0")?;
        let addr = server.addr();

        server.send(String::from("# Markdown"))?;

        let req = Request {
            url: format!("ws://{}", addr).parse()?,
            extra_headers: None,
        };

        let (mut websocket, _) = tungstenite::connect(req).unwrap();

        let message = websocket.read_message().unwrap();
        assert!(message.is_text(), "message was not text: {:?}", message);
        assert_eq!(message.to_text().unwrap().trim(), "<h1>Markdown</h1>");
        websocket.close(None).unwrap();

        assert_websocket_closed(&mut websocket);

        Ok(())
    }

    #[test]
    fn closed_websocket_removed_from_clients() -> Result<(), Box<dyn Error>> {
        let mut server = Server::bind("localhost:0")?;
        let addr = server.addr();

        let req = Request {
            url: format!("ws://{}", addr).parse()?,
            extra_headers: None,
        };

        let (mut websocket, _) = tungstenite::connect(req)?;

        websocket.close(None)?;
        websocket.write_pending().unwrap();

        assert_websocket_closed(&mut websocket);

        server.send(String::from("# Markdown")).unwrap();

        assert_matches!(
            websocket.read_message(),
            Err(tungstenite::Error::AlreadyClosed)
        );

        Ok(())
    }
    */
}
