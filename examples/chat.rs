use anyhow::Result;
use dashmap::DashMap;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt::Layer, layer::SubscriberExt, util::SubscriberInitExt, Layer as _};

#[tokio::main]
async fn main() -> Result<()> {
    let layer = Layer::new().with_filter(LevelFilter::INFO);
    tracing_subscriber::registry().with(layer).init();

    let address = "0.0.0.0:8080";
    let listener = TcpListener::bind(address).await?;
    info!("start serving on {}", address);
    let state = State::new();
    let state = Arc::new(state);

    loop {
        let (stream, socket_addr) = listener.accept().await?;
        info!("accept connection from: {}", socket_addr);
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, socket_addr, state_clone).await {
                warn!("handler client error {:?}", e)
            }
        });
    }
}

struct Peer {
    socket_addr: SocketAddr,
    username: String,
}

impl Peer {
    async fn receive(
        &self,
        state: Arc<State>,
        mut stream: SplitStream<Framed<TcpStream, LinesCodec>>,
    ) -> Result<()> {
        while let Some(line) = stream.next().await {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    warn!("{:?}", e);
                    continue;
                }
            };
            state
                .broadcast(
                    Arc::new(Message::Chat {
                        name: self.username.clone(),
                        text: line,
                    }),
                    &self.socket_addr,
                )
                .await;
        }
        info!("{} has left the chat", &self.username);
        state.peers.remove(&self.socket_addr);
        state
            .broadcast(
                Arc::new(Message::Leave(self.username.clone())),
                &self.socket_addr,
            )
            .await;
        Ok(())
    }
}

enum Message {
    Join(String),
    Leave(String),
    Chat { name: String, text: String },
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Join(username) => write!(f, "[{}-has joined the chat]", username),
            Message::Leave(username) => write!(f, "[--{}-has left the chat]", username),
            Message::Chat { name, text } => write!(f, "[{}: {}]", name, text),
        }
    }
}

struct State {
    peers: DashMap<SocketAddr, Sender<Arc<Message>>>,
}

impl State {
    fn new() -> Self {
        Self {
            peers: DashMap::new(),
        }
    }

    fn add(&self, addr: SocketAddr, username: String) -> (Arc<Peer>, Receiver<Arc<Message>>) {
        let (tx, rx) = channel(128);
        let peer = Peer {
            socket_addr: addr,
            username,
        };
        self.peers.insert(addr, tx);
        (Arc::new(peer), rx)
    }

    async fn broadcast(&self, message: Arc<Message>, addr: &SocketAddr) {
        for peer in self.peers.iter() {
            if peer.key() == addr {
                continue;
            }
            info!("broadcast message to {:?}", peer.key());
            if let Err(e) = peer.value().send(message.clone()).await {
                warn!("Failed to send message to {}: {}", peer.key(), e);
                // if send failed, peer might be gone, remove peer from state
                self.peers.remove(peer.key());
            }
        }
    }
}

async fn handle_client(
    stream: TcpStream,
    socket_addr: SocketAddr,
    state: Arc<State>,
) -> Result<()> {
    let mut stream = Framed::<TcpStream, LinesCodec>::new(stream, LinesCodec::new());
    stream.send("Enter your name:").await?;
    let username = match stream.next().await {
        Some(Ok(username)) => username,
        Some(Err(e)) => return Err(e.into()),
        None => return Ok(()),
    };
    info!("{} has joined the chat", username);
    state
        .broadcast(Arc::new(Message::Join(username.clone())), &socket_addr)
        .await;

    let (peer, mut receiver) = state.add(socket_addr, username);
    let (mut writer, reader) = stream.split();

    let peer_clone = peer.clone();
    tokio::spawn(async move {
        if let Err(e) = peer_clone.receive(state, reader).await {
            warn!("{:?}", e);
        }
    });

    let peer_clone = peer.clone();
    while let Some(message) = receiver.recv().await {
        info!("{:?} receive message from channel", peer_clone.username);
        if let Err(e) = writer.send(message.to_string()).await {
            warn!("send message to {:?} err {:?}", peer_clone.username, e);
            break;
        } else {
            info!("send message to {:?} success", peer_clone.username);
        }
    }
    Ok(())
}
