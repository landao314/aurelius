use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::path::PathBuf;

use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::error::ProtocolError;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::Role;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures_util::SinkExt;
use hyper::header::{self, HeaderValue};
use hyper::service::Service;
use hyper::{Body, Request, Response, StatusCode};
use log::*;
use tokio::sync::broadcast::Sender;
use url::Url;
use serde::Serialize;
use handlebars::Handlebars;

struct Error {}

/// Service that broadcasts received HTML to any listening WebSocket clients.
pub struct WebsocketBroadcastService {
    pub html_tx: Sender<String>,
}

impl WebsocketBroadcastService {
    fn handle_request(&mut self, req: Request<Body>) -> Response<Body> {
        if is_websocket_upgrade(&req) {
            let websocket_key = match req.headers().get("Sec-WebSocket-Key") {
                Some(key) => key.as_bytes(),
                None => return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(ProtocolError::MissingSecWebSocketKey.to_string().into())
                    .unwrap(),
            };

            let response = Response::builder()
                .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
                .header(header::CONNECTION, "upgrade")
                .header(header::UPGRADE, "websocket")
                .header(
                    header::SEC_WEBSOCKET_ACCEPT,
                    derive_accept_key(websocket_key),
                )
                .body(Body::empty())
                .unwrap();

            let upgrade = hyper::upgrade::on(req);

            let mut html_rx = self.html_tx.subscribe();

            // Handle websockets
            tokio::spawn(async move {
                let upgraded = upgrade.await?;

                let mut ws = WebSocketStream::from_raw_socket(
                    TokioAdapter::new(upgraded),
                    Role::Server,
                    None,
                )
                .await;

                while let Ok(html) = html_rx.recv().await {
                    ws.send(Message::Text(html)).await?;
                }

                Ok::<_, anyhow::Error>(())
            });

            response
        } else {
            let html = Handlebars::new()
                .render_template(
                    include_str!("../templates/markdown_view.html"),
                    &TemplateData {
                        remote_custom_css: &[],
                        local_custom_css: &[],
                        highlight_theme: "github",
                    }
                )
                .unwrap();

            Response::builder()
                .status(hyper::StatusCode::OK)
                .body(Body::from(html))
                .unwrap()
        }
    }
}

impl Service<Request<Body>> for WebsocketBroadcastService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        debug!("incoming request: {:?}", req);

        let response = self.handle_request(req);

        Box::pin(async move { Ok::<_, Infallible>(response) })
    }
}

#[derive(Debug, Serialize)]
struct TemplateData<'a> {
    remote_custom_css: &'a [Url],
    local_custom_css: &'a [PathBuf],
    highlight_theme: &'a str,
}

fn is_websocket_upgrade<B>(request: &Request<B>) -> bool {
    let headers = request.headers();

    headers.get(header::CONNECTION) == Some(&HeaderValue::from_static("Upgrade"))
        && headers.get(header::UPGRADE) == Some(&HeaderValue::from_static("websocket"))
}
