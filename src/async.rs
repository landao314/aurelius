use std::convert::Infallible;
use std::net::SocketAddr;

use async_tungstenite::tungstenite::error::ProtocolError;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::Role;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use hyper::header::HeaderValue;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use thiserror::Error;
use tokio::select;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use serde::Serialize;
use handlebars::Handlebars;
use url::Url;

use tokio::io;
use tokio::sync::broadcast;

#[derive(Clone, Debug, Serialize)]
pub struct Position {
    line: usize,
    character: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Update {
    start: Position,
    end: Position,
    text: String,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("HTTP server error: {0}")]
    Http(#[from] hyper::Error),
}

pub struct Server {
    local_addr: SocketAddr,
    markdown_tx: broadcast::Sender<Update>,
}

async fn handle(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::new(Body::from("Hello world")))
}

impl Server {
    pub async fn bind(addr: &SocketAddr) -> Result<Self, Error> {
        let (markdown_tx, _) = broadcast::channel::<Update>(16);
        let sender = markdown_tx.clone();

        let make_service = make_service_fn(move |_conn| {
            let markdown_tx = markdown_tx.clone();

            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    println!("got request {:?}", req);

                    let response = if is_websocket_upgrade(&req) {
                        let mut markdown_rx = markdown_tx.subscribe();

                        let websocket_key = req.headers().get("Sec-WebSocket-Key")
                            .ok_or_else(|| ProtocolError::MissingSecWebSocketKey)
                            .unwrap() // FIXME
                            .as_bytes();

                        let res = Response::builder()
                            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
                            .header(hyper::header::CONNECTION, "upgrade")
                            .header(hyper::header::UPGRADE, "websocket")
                            .header("Sec-WebSocket-Accept", derive_accept_key(websocket_key))
                            .body(Body::empty())
                            .unwrap();

                        let upgrade = hyper::upgrade::on(req);

                        // Handle websockets
                        tokio::spawn(async move {
                            let upgraded = upgrade.await.unwrap(); // FIXME
                            let upgraded = upgraded.compat_write();

                            let mut ws = WebSocketStream::from_raw_socket(
                                upgraded,
                                Role::Server,
                                None,
                            ).await;

                            while let Ok(update) = markdown_rx.recv().await {
                                ws.send(Message::Text(format!("sent: {}", update.text))).await.unwrap(); // FIXME
                            }
                        });

                        res
                    } else {
                        #[derive(Debug, Serialize)]
                        struct Data<'a> {
                            remote_custom_css: &'a [Url],
                            local_custom_css: &'a [String],
                            highlight_theme: &'a str,
                        }

                        let template_data = Data {
                            remote_custom_css: &[],
                            local_custom_css: &[],
                            highlight_theme: "github",
                        };

                        let html = Handlebars::new()
                            .render_template(include_str!("../templates/markdown_view.html"), &template_data)
                            .unwrap();

                        Response::builder()
                            .status(hyper::StatusCode::OK)
                            .body(Body::from(html))
                            .unwrap()
                    };

                    async move { Ok::<_, Infallible>(response) }
                }))
            }
        });

        let server = hyper::Server::try_bind(addr)?.serve(make_service);

        let local_addr = server.local_addr();

        tokio::spawn(server);

        Ok(Server {
            local_addr,
            markdown_tx: sender,
        })
    }

    pub fn send(&self, s: &str) {
        let res = self.markdown_tx.send(Update {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
            text: "Foo".to_string(),
        });

        println!("{:?}", res);
    }

    pub fn addr(&self) -> SocketAddr {
        self.local_addr
    }
}

fn is_websocket_upgrade<B>(request: &Request<B>) -> bool {
    let headers = request.headers();

    headers.get(hyper::header::CONNECTION) == Some(&HeaderValue::from_static("Upgrade"))
        && headers.get(hyper::header::UPGRADE) == Some(&HeaderValue::from_static("websocket"))
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::net::SocketAddr;

    use tokio::io;
    use tokio::net::{ToSocketAddrs, lookup_host};

    use super::Server;

    async fn new_server() -> Result<Server, Box<dyn Error>> {
            let addr = lookup_host("localhost:0").await?.next().unwrap();
            Ok(Server::bind(&addr).await?)
    }

    #[tokio::test]
    async fn connect_html() -> Result<(), Box<dyn Error>> {
        let server = new_server().await?;

        let body = reqwest::get(&format!("http://{}", server.addr())).await?
            .text()
            .await?;

        assert!(body.contains("<html>"));

        Ok(())
    }

    #[tokio::test]
    async fn connect_websocket_async() -> Result<(), Box<dyn Error>> {
        let server = new_server().await?;

        async_tungstenite::tokio::connect_async(format!("ws://{}", server.addr())).await.unwrap();

        Ok(())
    }
}
