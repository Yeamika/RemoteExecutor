use crate::ShellManager;
use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use pty_t_core::session::Session;
use pty_t_protocol::{AdminText, ClientText, ServerText};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

pub fn start_listener(addr: String, manager: ShellManager) -> Result<String> {
    let std_listener =
        std::net::TcpListener::bind(&addr).with_context(|| format!("bind {addr}"))?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    let actual_addr = listener.local_addr()?.to_string();

    tokio::spawn(async move {
        accept_loop(listener, manager).await;
    });

    Ok(actual_addr)
}

pub async fn handle_first_text(
    first_text: String,
    mut ws_write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
    ws_read: futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>,
    peer_addr: SocketAddr,
    manager: ShellManager,
) -> Result<()> {
    if let Ok(admin) = serde_json::from_str::<AdminText>(&first_text) {
        let response = handle_admin(&manager, admin);
        send_response(&mut ws_write, response).await?;
        return Ok(());
    }

    let Ok(ClientText::Hello {
        id,
        pty,
        cols,
        rows,
    }) = serde_json::from_str::<ClientText>(&first_text)
    else {
        send_response(
            &mut ws_write,
            Ok(ServerText::Error {
                message: "expected ptyt hello or read-only admin request".to_string(),
            }),
        )
        .await?;
        return Ok(());
    };

    handle_terminal_client(manager, ws_write, ws_read, peer_addr, id, pty, cols, rows).await
}

async fn accept_loop(listener: TcpListener, manager: ShellManager) {
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let manager = manager.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, peer_addr, manager).await {
                        eprintln!("remote-executor websocket error: {err:#}");
                    }
                });
            }
            Err(err) => {
                eprintln!("remote-executor accept error: {err:#}");
                break;
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    manager: ShellManager,
) -> Result<()> {
    let ws = accept_async(stream).await?;
    let (ws_write, mut ws_read) = ws.split();
    let first = ws_read
        .next()
        .await
        .ok_or_else(|| anyhow!("client disconnected before hello"))??;
    let first_text = first.into_text().context("first frame must be text")?;
    handle_first_text(
        first_text.to_string(),
        ws_write,
        ws_read,
        peer_addr,
        manager,
    )
    .await
}

async fn handle_terminal_client(
    manager: ShellManager,
    ws_write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
    mut ws_read: futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>,
    peer_addr: SocketAddr,
    id: String,
    pty: String,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let Some(session) = manager.core().session(&pty) else {
        return Err(anyhow!("pty {pty} does not exist"));
    };

    let token = rand_token();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let output_rx = session.subscribe_live_output();
    let writer_task = tokio::spawn(async move {
        let mut ws_write = ws_write;
        while let Some(msg) = rx.recv().await {
            if ws_write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let id = session.register_client(id, token, cols, rows)?;
    manager.register_client(&pty, id.clone(), token, tx.clone(), peer_addr);
    let _ = tx.send(Message::Binary(session.snapshot_formatted().into()));
    manager.broadcast_meta(&pty);

    let output_tx = tx.clone();
    let output_task = tokio::spawn(async move {
        let mut output_rx = output_rx;
        while let Some(data) = output_rx.recv().await {
            if output_tx.send(Message::Binary(data.into())).is_err() {
                break;
            }
        }
    });

    let mut result = Ok(());
    while let Some(msg) = ws_read.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                eprintln!("remote-executor client {id} disconnected: {err}");
                break;
            }
        };

        result = handle_client_message(&manager, &session, &tx, &pty, &id, token, msg).await;
        if result.is_err() {
            break;
        }
    }

    session.unregister_client(&id, token);
    manager.remove_client(&pty, &id, token);
    manager.broadcast_meta(&pty);
    writer_task.abort();
    output_task.abort();
    let _ = writer_task.await;
    let _ = output_task.await;
    result
}

fn handle_admin(manager: &ShellManager, msg: AdminText) -> Result<ServerText> {
    match msg {
        AdminText::List => Ok(ServerText::Sessions {
            sessions: manager.list(),
        }),
        AdminText::Detail { pty } => Ok(ServerText::Session {
            session: manager.detail(&pty)?,
        }),
        _ => Ok(ServerText::Error {
            message: "remote admin mutation is disabled".to_string(),
        }),
    }
}

async fn handle_client_message(
    manager: &ShellManager,
    session: &std::sync::Arc<Session>,
    tx: &mpsc::UnboundedSender<Message>,
    pty: &str,
    id: &str,
    token: u64,
    msg: Message,
) -> Result<()> {
    match msg {
        Message::Binary(data) => session.write_from_client(id, token, &data),
        Message::Text(text) => match serde_json::from_str::<ClientText>(&text) {
            Ok(ClientText::Resize { cols, rows }) => {
                let result = session.set_client_size(id, token, cols, rows);
                if result.is_ok() {
                    manager.broadcast_meta(pty);
                }
                result
            }
            Ok(ClientText::RequestControl) => {
                if manager.is_locked(pty) && id != "0" {
                    send_error_tx(tx, "pty is locked to user 0");
                    Ok(())
                } else {
                    let result = session.set_controller(id);
                    if result.is_ok() {
                        manager.broadcast_meta(pty);
                    }
                    result
                }
            }
            Ok(ClientText::Hello { .. }) => Ok(()),
            Err(err) => {
                send_error_tx(tx, &format!("bad client message: {err}"));
                Ok(())
            }
        },
        Message::Ping(data) => {
            let _ = tx.send(Message::Pong(data));
            Ok(())
        }
        Message::Close(_) | Message::Pong(_) | Message::Frame(_) => Ok(()),
    }
}

async fn send_response(
    ws_write: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
    response: Result<ServerText>,
) -> Result<()> {
    let msg = match response {
        Ok(msg) => msg,
        Err(err) => ServerText::Error {
            message: err.to_string(),
        },
    };
    ws_write
        .send(Message::Text(serde_json::to_string(&msg)?.into()))
        .await?;
    Ok(())
}

fn send_error_tx(tx: &mpsc::UnboundedSender<Message>, message: &str) {
    let msg = ServerText::Error {
        message: message.to_string(),
    };
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = tx.send(Message::Text(text.into()));
    }
}

fn rand_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}
