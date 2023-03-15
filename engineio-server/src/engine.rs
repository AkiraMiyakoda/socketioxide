use std::{collections::HashMap, ops::ControlFlow, sync::Arc};

use crate::{
    errors::Error,
    futures::ResponseFuture,
    layer::EngineIoHandler,
    packet::{OpenPacket, Packet, TransportType},
    socket::Socket,
    utils::generate_sid,
};
use futures::{SinkExt, StreamExt, TryStreamExt};
use http::{request::Parts, Request};
use hyper::upgrade::Upgraded;
use std::fmt::Debug;
use tokio::sync::RwLock;
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Message},
    WebSocketStream,
};

#[derive(Debug, Clone)]
pub struct EngineIoConfig {
    pub req_path: String,
    pub ping_interval: u32,
    pub ping_timeout: u32,
}

impl Default for EngineIoConfig {
    fn default() -> Self {
        Self {
            req_path: "/engine.io".to_string(),
            ping_interval: 300,
            ping_timeout: 200,
        }
    }
}

type SocketMap<T> = RwLock<HashMap<i64, T>>;
/// Abstract engine implementation for Engine.IO server for http polling and websocket
#[derive(Debug)]
pub struct EngineIo<H>
where
    H: EngineIoHandler,
{
    sockets: SocketMap<Socket>,
    handler: H,
    pub config: EngineIoConfig,
}

impl<H> EngineIo<H>
where
    H: EngineIoHandler,
{
    pub fn from_config(handler: H, config: EngineIoConfig) -> Self {
        Self {
            sockets: RwLock::new(HashMap::new()),
            config,
            handler,
        }
    }
}

impl<H> EngineIo<H>
where
    H: EngineIoHandler,
{
    pub fn on_polling_req<F, B>(self: Arc<Self>, req: Request<B>) -> ResponseFuture<F> {
        let (parts, _) = req.into_parts();
        let sid = extract_sid(&parts);
        if sid.is_none() {
            return ResponseFuture::empty_response(400);
        }
        let (tx, body) = hyper::Body::channel();
        tokio::task::spawn(async move {
            let mut sockets = self.sockets.write().await;
            // If a socket with this SID already exists we close the connection
            if let Some(socket) = sockets.remove(&sid.unwrap()) {
                socket.close().await.unwrap();
                tx.abort();
            } else {
                sockets.insert(sid.unwrap(), Socket::new_http(sid.unwrap(), tx));
            }
        });

        ResponseFuture::streaming_response(body)
    }

    pub fn on_send_packet_req<B, F>(self: Arc<Self>, req: Request<B>) -> ResponseFuture<F>
    where
        B: http_body::Body + std::marker::Send + 'static,
        <B as http_body::Body>::Error: Debug,
        <B as http_body::Body>::Data: std::marker::Send,
    {
        let (parts, body) = req.into_parts();
        let sid = extract_sid(&parts);
        if sid.is_none() {
            return ResponseFuture::empty_response(400);
        }
        tokio::task::spawn(async move {
            if let Some(socket) = self.sockets.write().await.get_mut(&sid.unwrap()) {
                let body = hyper::body::to_bytes(body).await.unwrap();
                let packet = Packet::try_from(body).unwrap();
                socket.handle_packet(packet, &self.handler).await;
            }
        });
        // let mut sockets = self.ws_sockets.lock().unwrap;
        // if let tx = sockets.get_mut(&sid.unwrap()) {
        // let packet = Packet::from_request(req);
        // } else {
        // return ResponseFuture::empty_response(400);
        // }
        ResponseFuture::empty_response(200)
    }

    /// Upgrade a websocket request to create a websocket connection.
    ///
    /// If a sid is provided in the query it means that is is upgraded from an existing HTTP polling request. In this case
    /// the http polling request is closed and the SID is kept for the websocket
    pub fn upgrade_ws_req<B, F>(self: Arc<Self>, req: Request<B>) -> ResponseFuture<F>
    where
        B: std::marker::Send,
    {
        println!("WS Upgrade req {:?}", req.uri());

        let (parts, _) = req.into_parts();
        let ws_key = match parts.headers.get("Sec-WebSocket-Key") {
            Some(key) => key.clone(),
            None => return ResponseFuture::empty_response(500),
        };

        tokio::task::spawn(async move {
            let sid = extract_sid(&parts);
            let req = Request::from_parts(parts, ());
            match hyper::upgrade::on(req).await {
                Ok(conn) => self.on_ws_conn_upgrade(conn, sid).await,
                Err(e) => println!("WS upgrade error: {}", e),
            }
        });
        ResponseFuture::upgrade_response(ws_key)
    }

    /// Handle a websocket connection upgrade
    ///
    /// Sends an open packet if it is not an upgrade from a polling request
    ///
    /// Read packets from the websocket and handle them
    async fn on_ws_conn_upgrade(self: Arc<Self>, conn: Upgraded, sid: Option<i64>) {
        let ws = WebSocketStream::from_raw_socket(conn, Role::Server, None).await;
        println!("WS upgrade comming from polling: {}", sid.is_some());

        let (mut tx, mut rx) = ws.split();

        let sid = if sid.is_none() || !self.sockets.read().await.contains_key(&sid.unwrap()) {
            let sid = generate_sid();
            let msg: String =
                Packet::Open(OpenPacket::new(TransportType::Websocket, sid, &self.config))
                    .try_into()
                    .expect("Failed to serialize open packet");
            if let Err(e) = tx.send(Message::Text(msg)).await {
                println!("Error sending open packet: {}", e);
                return;
            }
            self.sockets
                .write()
                .await
                .insert(sid, Socket::new_ws(sid, tx));
            sid
        } else {
            let sid = sid.unwrap();
            self.sockets
                .write()
                .await
                .get_mut(&sid)
                .unwrap()
                .upgrade_from_http(tx);
            sid
        };

        while let Ok(msg) = rx.try_next().await {
            let Some(msg) = msg else { continue };
            let res = match msg {
                Message::Text(msg) => match Packet::try_from(msg) {
                    Ok(packet) => {
                        self.clone()
                            .sockets
                            .write()
                            .await
                            .get_mut(&sid)
                            .unwrap()
                            .handle_packet(packet, &self.handler)
                            .await
                    }
                    Err(err) => ControlFlow::Break(Err(err)),
                },
                Message::Binary(msg) => {
                    match self
                        .clone()
                        .sockets
                        .write()
                        .await
                        .get_mut(&sid)
                        .unwrap()
                        .handle_binary(msg, &self.handler)
                        .await
                    {
                        Ok(_) => ControlFlow::Continue(Ok(())),
                        Err(e) => ControlFlow::Continue(Err(e)),
                    }
                }
                Message::Ping(_) => unreachable!(),
                Message::Pong(_) => unreachable!(),
                Message::Close(_) => break,
                Message::Frame(_) => unreachable!(),
            };
            match res {
                ControlFlow::Break(Ok(())) => break,
                ControlFlow::Break(Err(e)) => {
                    println!("Error handling websocket message, closing conn: {:?}", e);
                    break;
                }
                ControlFlow::Continue(Ok(())) => continue,
                ControlFlow::Continue(Err(e)) => {
                    println!("Error handling websocket message: {:?}", e);
                }
            }
        }
        if let Some(socket) = self.sockets.write().await.remove(&sid) {
            if let Err(e) = socket.close().await {
                println!("Error closing websocket: {:?}", e);
            }
        }
    }
}

fn extract_sid(req: &Parts) -> Option<i64> {
    let uri = req.uri.query()?;
    let sid = uri
        .split("&")
        .find(|s| s.starts_with("sid="))?
        .split("=")
        .nth(1)?;
    Some(sid.parse().ok()?)
}
