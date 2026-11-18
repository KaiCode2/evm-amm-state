//! Public Alloy pubsub transport boundary, with every outbound request recorded.
use super::*;
use alloy_pubsub::{ConnectionHandle, PubSubConnect};
use alloy_transport::{TransportErrorKind, TransportResult};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub(super) struct Server {
    requests: Arc<Mutex<Vec<String>>>,
    blocked_reconnects: Arc<std::sync::atomic::AtomicUsize>,
    subscriptions: Arc<Mutex<std::collections::HashMap<String, String>>>,
    incoming: tokio::sync::mpsc::UnboundedSender<Value>,
    receiver: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Value>>>>,
}

impl Server {
    pub(super) fn new() -> Self {
        let (incoming, receiver) = tokio::sync::mpsc::unbounded_channel();
        Self {
            requests: Arc::default(),
            blocked_reconnects: Arc::default(),
            subscriptions: Arc::default(),
            incoming,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        }
    }
    pub(super) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    pub(super) fn blocked_reconnects(&self) -> usize {
        self.blocked_reconnects.load(Ordering::Relaxed)
    }
    pub(super) async fn wait_subscribed(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.subscriptions.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }
    fn emit(&self, kind: &str, value: Value) -> Result<()> {
        let id = self
            .subscriptions
            .lock()
            .unwrap()
            .get(kind)
            .cloned()
            .expect("subscribed");
        self.incoming.send(json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":id,"result":value}}))?;
        Ok(())
    }
    pub(super) fn header(&self, header: &RpcHeader) -> Result<()> {
        self.emit("newHeads", serde_json::to_value(header)?)
    }
    pub(super) fn log(&self, log: &RpcLog) -> Result<()> {
        self.emit("logs", serde_json::to_value(log)?)
    }
    pub(super) fn malformed_log(&self) -> Result<()> {
        self.emit("logs", json!({}))
    }
    pub(super) fn disconnect(&self) -> Result<()> {
        self.incoming.send(Value::Null)?;
        Ok(())
    }
}

impl PubSubConnect for Server {
    fn is_local(&self) -> bool {
        true
    }
    async fn try_reconnect(&self) -> TransportResult<ConnectionHandle> {
        self.blocked_reconnects.fetch_add(1, Ordering::Relaxed);
        Err(TransportErrorKind::custom_str(
            "explicit cold restart required",
        ))
    }
    async fn connect(&self) -> TransportResult<ConnectionHandle> {
        let Some(mut incoming) = self.receiver.lock().unwrap().take() else {
            self.requests
                .lock()
                .unwrap()
                .push("unexpected_reconnect".to_owned());
            return Err(TransportErrorKind::custom_str("reconnect forbidden"));
        };
        let (handle, mut interface) = ConnectionHandle::new();
        let requests = Arc::clone(&self.requests);
        let subscriptions = Arc::clone(&self.subscriptions);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = incoming.recv() => {
                        let Some(frame) = frame else { break; };
                        if frame.is_null() { break; }
                        if interface.send_to_frontend(serde_json::from_str(&frame.to_string()).unwrap()).is_err() { break; }
                    }
                    request = interface.recv_from_frontend() => {
                        let Some(request) = request else { break; };
                        let value: Value = serde_json::from_str(request.get()).unwrap();
                        let method = value["method"].as_str().unwrap();
                        requests.lock().unwrap().push(method.to_owned());
                        let result = match method {
                            "eth_chainId" => json!("0x1"),
                            "eth_subscribe" => {
                                let mut entries = subscriptions.lock().unwrap();
                                let id = format!("0x{:064x}", entries.len() + 1);
                                entries.insert(value["params"][0].as_str().unwrap().to_owned(), id.clone());
                                json!(id)
                            }
                            "eth_unsubscribe" => json!(true),
                            "eth_getLogs" => json!([]), // explicitly measured cold owner setup only
                            _ => panic!("unexpected request: {method}"),
                        };
                        let response = json!({"jsonrpc":"2.0","id":value["id"],"result":result});
                        if interface.send_to_frontend(serde_json::from_str(&response.to_string()).unwrap()).is_err() { break; }
                    }
                }
            }
        });
        Ok(handle.with_max_retries(0))
    }
}
