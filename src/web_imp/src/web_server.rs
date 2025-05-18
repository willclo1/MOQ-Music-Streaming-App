use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Extension,
    response::IntoResponse,
    routing::get,
    Router,
};

use std::{net::SocketAddr, sync::Arc};
use axum::response::Html;
use tokio::sync::{broadcast, Mutex};
use futures::{SinkExt, StreamExt};
use tokio::fs;
use tokio::net::TcpListener;
use std::collections::HashMap;
use serde::{Serialize, Deserialize};

pub type AudioTx = broadcast::Sender<Vec<u8>>;
static BUFFER_TARGET: u32 = 1000;

// data for synchronization
#[derive(Clone, Serialize, Deserialize)]
struct StreamInfo {
    timestamp: u64, //use timestamp to calculate time offsets
    sequence: u32, //to detect dropped packets and proper playback order
    buffer_target: u32, //prevent congestion
    metadata: Option<String>, // song info, etc.
}

/*
Starts websocket server for clients to connect to
 */
pub async fn run_ws_server(tx: AudioTx, ws_pathname: String, port: u16) {
    //will be used to keep track of clients connected to the station
    let clients = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
    let clients_clone = clients.clone();


    // Only set up the route for the specified WebSocket path
    let app = Router::new()
        .route("/", get(serve_index))
        .route(&format!("/ws/{ws_pathname}"), get(move |ws, ext| {
            ws_handler(ws, ext, clients_clone.clone())
        }))
        .route("/stats", get(move || check_client_count(clients.clone())))
        .layer(Extension(Arc::new(tx)));
    println!("Starting WebSocket server on port {}, pathname {}", port, ws_pathname);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/*
just for testing and checking if i actually have more than one client on the website
 */
async fn check_client_count(clients: Arc<Mutex<HashMap<String, u64>>>) -> impl IntoResponse {
    let clients = clients.lock().await;
    Html(format!("<h1>Connected Clients: {}</h1>", clients.len()))
}

async fn ws_handler(ws: WebSocketUpgrade, Extension(tx): Extension<Arc<AudioTx>>, clients: Arc<Mutex<HashMap<String, u64>>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, tx, clients))
}

/*
TO-DO: Add receiving on client add for the info this function is sending
 */
async fn handle_socket(mut socket: WebSocket, tx: Arc<AudioTx>, clients: Arc<Mutex<HashMap<String, u64>>>) {
    //use uuid to make unique ids for each client
    let client_id = uuid::Uuid::new_v4().to_string();

    // add client to map, alongside timestamp to compare against
    let mut clients_map = clients.lock().await;
    clients_map.insert(client_id.clone(), chrono::Utc::now().timestamp_millis() as u64);
    println!("Client connected: {}", client_id);

    // msg channel
    let mut rx = tx.subscribe();

    // initial sync msg
    let sync_info = StreamInfo {
        timestamp: chrono::Utc::now().timestamp_millis() as u64,
        sequence: 0,
        buffer_target: BUFFER_TARGET, // Recommend 1 second buffer
        metadata: Some("Starting Stream".to_string()),
    };

    if socket.send(Message::Text(serde_json::to_string(&sync_info).unwrap())).await.is_err() {
        println!("Failed to send initial sync message");
        return;
    }

    // project like 3 stuff, split up socket so we can provide bidirectional comms (real time apps need this)
    let (mut sender, mut receiver) = socket.split();

    // spawn task for handling client msgs abt their info
    let client_id_clone = client_id.clone();
    let clients_clone = clients.clone();

    //TO-DO: make each client send every once in a while since this will prob clog up terminal w/ multiple clients
    // might also help performance(?)
    tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(buffer_ms) = data.get("bufferedMs").and_then(|v| v.as_u64()) {
                        println!("Client {} buffer: {}ms", client_id_clone, buffer_ms);
                    }
                }
            }
        }

        //client disconnect
        let mut clients_map = clients_clone.lock().await;
        clients_map.remove(&client_id_clone);
        println!("Client disconnected: {}", client_id_clone);
    });

    // audio stream
    let mut sequence: u32 = 0; //initialize sequence number
    tokio::spawn(async move {
        while let Ok(pcm) = rx.recv().await {
            // send synchronization info every 24-60
            // *********(CHANGE THIS NUMBER LATER ON idk what it should rlly be)***********
            if sequence % 60 == 0 {
                let sync_info = StreamInfo {
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    sequence,
                    buffer_target: BUFFER_TARGET,
                    metadata: Some("SYNCING MSG BEEPBOOP".to_string()),
                };

                if sender.send(Message::Text(serde_json::to_string(&sync_info).unwrap())).await.is_err() {
                    break;
                }
            }

            // send dat audio data
            if sender.send(Message::Binary(pcm)).await.is_err() {
                break;
            }

            sequence += 1;
        }
    });

    //this is a god damn problem dont uncomment for now lmao
    // tokio::select! {
    //     _ = client_handler => {},
    //     _ = audio_stream_handler => {},
    // }
}

async fn serve_index() -> impl IntoResponse {
    match fs::read_to_string("index.html").await {
        Ok(content) => Html(content),
        Err(_) => Html("<h1>index.html not found</h1>".to_string()),
    }
}