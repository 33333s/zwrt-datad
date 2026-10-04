//! What the data channel serves. Frames reach `handle` in arrival order; replies
//! go back through `out`. The classic NMS tunnel stays the fallback for anything
//! this build does not serve, so an unsupported request is answered, not dropped.
use super::frame;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct Service {
    /// The one device port this session may reach.
    #[allow(dead_code)]
    port: u16,
    out: mpsc::Sender<Vec<u8>>,
}

impl Service {
    pub fn new(port: u16, out: mpsc::Sender<Vec<u8>>) -> Self {
        Self { port, out }
    }

    pub fn handle(&mut self, header: Value, _payload: &[u8]) {
        self.reply(&header, "unsupported");
    }

    fn reply(&self, header: &Value, error: &str) {
        let Some(id) = header.get("id").filter(|id| id.is_number()) else {
            return;
        };
        if let Some(frame) = frame::encode(&json!({"id":id,"ok":false,"error":error}), b"") {
            let _ = self.out.try_send(frame);
        }
    }
}
