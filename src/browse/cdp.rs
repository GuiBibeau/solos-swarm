use super::ACTION_WAIT;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// A page's whole HTML, the largest answer, is one response escaped as JSON.
const MESSAGE_BYTES: usize = 2 * super::RESPONSE_BYTES;
const EVENTS: usize = 256;

type Socket = WebSocketStream<UnixStream>;
type Waiting = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>>;

#[derive(Debug)]
pub(super) enum CdpError {
    Refused(String),
    Gone(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) | Self::Gone(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CdpError {}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Started(String),
    Stopped(String),
}

impl Event {
    fn of(message: &Value) -> Option<Self> {
        let frame = message["params"]["frameId"].as_str()?.to_owned();
        match message["method"].as_str()? {
            "Page.frameStartedNavigating"
            | "Page.frameScheduledNavigation"
            | "Page.frameRequestedNavigation"
            | "Page.frameStartedLoading" => Some(Self::Started(frame)),
            "Page.frameStoppedLoading" => Some(Self::Stopped(frame)),
            _ => None,
        }
    }
}

pub(super) struct Cdp {
    sink: SplitSink<Socket, Message>,
    waiting: Waiting,
    events: mpsc::Receiver<Event>,
    next: u64,
    reader: JoinHandle<()>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Cdp {
    pub(super) async fn connect(socket: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MESSAGE_BYTES))
            .max_frame_size(Some(MESSAGE_BYTES));
        let (socket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
                .await?;
        let (sink, stream) = socket.split();
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events_in, events) = mpsc::channel(EVENTS);
        let reader = tokio::spawn(read(stream, Arc::clone(&waiting), events_in));
        Ok(Self {
            sink,
            waiting,
            events,
            next: 0,
            reader,
        })
    }

    pub(super) async fn call(
        &mut self,
        session: Option<&str>,
        method: &'static str,
        params: Value,
    ) -> Result<Value, CdpError> {
        self.next += 1;
        let id = self.next;
        let (answer, answered) = oneshot::channel();
        match self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            Some(waiting) => waiting.insert(id, answer),
            None => return Err(CdpError::Gone("the browser's connection is closed".into())),
        };
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        self.sink
            .send(Message::text(message.to_string()))
            .await
            .map_err(|error| CdpError::Gone(format!("cannot send {method}: {error}")))?;
        match tokio::time::timeout(ACTION_WAIT, answered).await {
            Err(_) => Err(CdpError::Gone(format!(
                "the browser did not answer {method} within {} s",
                ACTION_WAIT.as_secs()
            ))),
            Ok(Err(_)) => Err(CdpError::Gone("the browser's connection closed".into())),
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(why))) => Err(CdpError::Refused(format!("{method}: {why}"))),
        }
    }

    pub(super) fn clear_events(&mut self) {
        while self.events.try_recv().is_ok() {}
    }

    pub(super) async fn event_until(&mut self, deadline: Instant) -> Option<Event> {
        tokio::time::timeout_at(deadline, self.events.recv())
            .await
            .ok()
            .flatten()
    }
}

async fn read(mut stream: SplitStream<Socket>, waiting: Waiting, events: mpsc::Sender<Event>) {
    while let Some(Ok(message)) = stream.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if let Some(id) = message["id"].as_u64() {
            let answer = match message.get("error") {
                Some(error) => Err(error["message"].as_str().unwrap_or("an error").to_owned()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let caller = waiting
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
                .and_then(|waiting| waiting.remove(&id));
            if let Some(caller) = caller {
                let _ = caller.send(answer);
            }
        } else if let Some(event) = Event::of(&message) {
            let _ = events.try_send(event);
        }
    }
    *waiting.lock().unwrap_or_else(PoisonError::into_inner) = None;
}
