//! Chat history: an append-only JSON-lines log in the home directory. A message is one
//! line; its delivery receipt is another line that refers to it by id.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ferrymesh_core::{MsgId, PublicIdentity};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The other side's contact card.
    pub peer: String,
    pub outgoing: bool,
    pub text: String,
    /// When the sender wrote it (sender's clock).
    pub sent_at_ms: u64,
    /// Our clock: when it was sent from here or arrived here.
    pub at_ms: u64,
    #[serde(default)]
    pub delivered: bool,
}

impl Entry {
    pub fn outgoing(id: MsgId, to: &PublicIdentity, text: &str, now_ms: u64) -> Self {
        Self {
            id: id.to_string(),
            peer: to.to_card(),
            outgoing: true,
            text: text.to_string(),
            sent_at_ms: now_ms,
            at_ms: now_ms,
            delivered: false,
        }
    }

    pub fn incoming(
        id: MsgId,
        from: &PublicIdentity,
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            id: id.to_string(),
            peer: from.to_card(),
            outgoing: false,
            text: String::from_utf8_lossy(body).into_owned(),
            sent_at_ms,
            at_ms: now_ms,
            delivered: true,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Record {
    Message(Entry),
    Delivered { id: String },
}

pub struct History {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl History {
    pub fn load(path: PathBuf) -> Result<Self> {
        let mut history = Self {
            path,
            entries: Vec::new(),
        };
        let text = match std::fs::read_to_string(&history.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(history),
            Err(e) => return Err(e).context("reading the chat history"),
        };
        // A line cut short by a crash is skipped rather than losing the whole history,
        // and closed so the next record starts on a line of its own.
        if !text.is_empty() && !text.ends_with('\n') {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&history.path)?
                .write_all(b"\n")?;
        }
        for record in text.lines().filter_map(|l| serde_json::from_str(l).ok()) {
            match record {
                Record::Message(entry) => history.entries.push(entry),
                Record::Delivered { id } => {
                    history.mark(&id);
                }
            }
        }
        Ok(history)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn add(&mut self, entry: Entry) -> Result<()> {
        self.append(&Record::Message(entry.clone()))?;
        self.entries.push(entry);
        Ok(())
    }

    /// Records a delivery receipt; returns whether it was for a message in the history.
    pub fn delivered(&mut self, id: MsgId) -> Result<bool> {
        let id = id.to_string();
        if !self.mark(&id) {
            return Ok(false);
        }
        self.append(&Record::Delivered { id })?;
        Ok(true)
    }

    fn mark(&mut self, id: &str) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id && e.outgoing) {
            Some(entry) => {
                entry.delivered = true;
                true
            }
            None => false,
        }
    }

    fn append(&self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(&line)
            .context("writing the chat history")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrymesh_core::{Identity, SeededEntropy};

    #[test]
    fn survives_reload_with_receipts() {
        let dir = std::env::temp_dir().join(format!("ferry-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let _ = std::fs::remove_file(&path);
        let bob = Identity::generate(&mut SeededEntropy::new(1)).public();

        let mut history = History::load(path.clone()).unwrap();
        let sent = MsgId([1; 16]);
        history.add(Entry::outgoing(sent, &bob, "hi", 10)).unwrap();
        history
            .add(Entry::incoming(MsgId([2; 16]), &bob, b"hey", 11, 12))
            .unwrap();
        assert!(history.delivered(sent).unwrap());
        assert!(!history.delivered(MsgId([9; 16])).unwrap());
        // A torn last line from a crash.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"kind\":\"mess")
            .unwrap();

        let back = History::load(path).unwrap();
        assert_eq!(back.entries(), history.entries());
        assert!(back.entries()[0].delivered);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
