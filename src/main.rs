use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use log::info;
use std::sync::Arc;
use tokio_tungstenite::{accept_async, WebSocketStream};
use tungstenite::Message as WsMessage;

#[link(name = "c", kind = "dylib")]
extern "C" {
    fn time(output: *mut i64) -> i64;
}

#[derive(Clone, Debug)]
struct ChatMessage {
    created_at: chrono::NaiveDateTime,
    username: String,
    message: String,
}

impl ToString for ChatMessage {
    fn to_string(&self) -> String {
        format!(
            "[{}]({}): {}",
            self.created_at.format("%Y-%m-%d %H:%M:%S"),
            self.username,
            self.message
        )
    }
}

/// app's context like database pool
struct AppState {
    new_msg_notify_sender: tokio::sync::broadcast::Sender<ChatMessage>,
    /// TODO sync to database every 10s?
    chat_history: tokio::sync::RwLock<Vec<ChatMessage>>,
}

struct ConnectionState {
    username: Option<String>,
    // client last pong msg timestamp
    last_heartbeat_timestamp: i64,
    ws_sender: SplitSink<WebSocketStream<tokio::net::TcpStream>, WsMessage>,
    new_msg_notify_sender: tokio::sync::broadcast::Sender<ChatMessage>,
}

impl ConnectionState {
    const HEARTBEAT_TIMEOUT: i64 = 15;

    async fn handle_client_msg(
        &mut self,
        msg: String,
        chat_history: &tokio::sync::RwLock<Vec<ChatMessage>>,
    ) -> tungstenite::Result<()> {
        let (command, arg) = msg.split_once(' ').unwrap();
        match command {
            "login" => {
                if let Some(ref username) = self.username {
                    self.ws_sender
                        .send(WsMessage::Text(format!(
                            "login_failed: you have logged in to {}",
                            username
                        )))
                        .await?;
                } else {
                    self.username = Some(arg.to_string());
                    self.ws_sender
                        .send(WsMessage::Text(format!("login_succeed: username={}", arg)))
                        .await?;
                }
            }
            "send" => {
                if let Some(ref username) = self.username {
                    let chat_message = ChatMessage {
                        created_at: chrono::Local::now().naive_utc(),
                        username: username.clone(),
                        message: arg.to_string(),
                    };
                    self.new_msg_notify_sender
                        .send(chat_message.clone())
                        .unwrap();
                    chat_history.write().await.push(chat_message);
                } else {
                    self.ws_sender
                        .send(WsMessage::Text("Please login!".to_string()))
                        .await?;
                }
            }
            _ => {
                self.ws_sender
                    .send(WsMessage::Text(format!("Unknown command: {}", command)))
                    .await?;
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::new()
        .filter_level("info".parse()?)
        .init();
    const WS_SERVER_ADDR: &str = "127.0.0.1:9007";
    let listener = tokio::net::TcpListener::bind(WS_SERVER_ADDR).await?;
    info!("Listening on: {}", WS_SERVER_ADDR);

    let (new_msg_notify_sender, _) = tokio::sync::broadcast::channel::<ChatMessage>(5000);
    let app_state = Arc::new(AppState {
        new_msg_notify_sender,
        chat_history: tokio::sync::RwLock::new(Vec::new()),
    });
    while let Ok((stream, _addr)) = listener.accept().await {
        tokio::spawn(accept_connection(stream, app_state.clone()));
    }
    Ok(())
}

async fn accept_connection(
    stream: tokio::net::TcpStream,
    app_state: Arc<AppState>,
) -> tungstenite::Result<()> {
    let (ws_sender, mut ws_receiver) = accept_async(stream).await.unwrap().split();
    let mut new_msg_notify_receiver = app_state.new_msg_notify_sender.subscribe();
    let mut connection_state = ConnectionState {
        username: None,
        ws_sender,
        new_msg_notify_sender: app_state.new_msg_notify_sender.clone(),
        last_heartbeat_timestamp: unsafe { time(std::ptr::null_mut()) },
    };

    // send history chat messages when client connect
    for each in &*app_state.chat_history.read().await {
        connection_state
            .ws_sender
            .send(WsMessage::Text(each.to_string()))
            .await?;
    }

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(5000));
    loop {
        tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(msg) => {
                        let msg = msg?;
                        match msg {
                            WsMessage::Text(msg) => connection_state.handle_client_msg(msg, &app_state.chat_history).await?,
                            WsMessage::Ping(_) => {
                                info!("server receive client ping frame");
                                connection_state.ws_sender.send(WsMessage::Pong(Vec::new())).await?;
                            },
                            WsMessage::Pong(_) => {
                                info!("server receive client pong frame");
                                connection_state.last_heartbeat_timestamp = unsafe { time(std::ptr::null_mut()) };
                            },
                            WsMessage::Close(_) => break,
                            WsMessage::Binary(_) => unreachable!()
                        }
                    }
                    None => break,
                }
            }
            // receive new msg notify
            Ok(msg) = new_msg_notify_receiver.recv() => {
                connection_state.ws_sender.send(WsMessage::Text(msg.to_string())).await?;
            }
            // send ping frame to client every 5s to keep alive
            _ = interval.tick() => {
                if unsafe { time(std::ptr::null_mut()) } - connection_state.last_heartbeat_timestamp > ConnectionState::HEARTBEAT_TIMEOUT {
                    info!("server close dead connection");
                    break;
                }
                connection_state.ws_sender.send(WsMessage::Ping(Vec::new())).await?
            }
        }
    }
    Ok(())
}
