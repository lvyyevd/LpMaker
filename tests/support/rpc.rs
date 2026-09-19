//! Loopback JSON-RPC only; tests never contact a real chain or load signing secrets.
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{accept_async, tungstenite::Message};

pub struct Mock {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    pub async fn start(
        handler: impl Fn(&Value) -> Result<Value, Value> + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = seen.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut socket = accept_async(stream).await.unwrap();
                    let Some(Ok(Message::Text(text))) = socket.next().await else {
                        return;
                    };
                    let req: Value = serde_json::from_str(&text).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let response = match handler(&req) {
                        Ok(v) => json!({"jsonrpc":"2.0","id":req["id"],"result":v}),
                        Err(e) => json!({"jsonrpc":"2.0","id":req["id"],"error":e}),
                    };
                    let _ = socket
                        .send(Message::Text(response.to_string().into()))
                        .await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
