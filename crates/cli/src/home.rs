//! The node's home directory: keys, contacts, carried messages, chat history.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ferrymesh_core::{Config, Identity, Node, OsEntropy, PublicIdentity};

pub struct Home(pub PathBuf);

impl Home {
    pub fn resolve(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(p) => p,
            None => PathBuf::from(std::env::var_os("HOME").context("set --home or HOME")?)
                .join(".ferrymesh"),
        };
        Ok(Self(path))
    }

    pub fn key(&self) -> PathBuf {
        self.0.join("identity.key")
    }

    fn contacts_file(&self) -> PathBuf {
        self.0.join("contacts.json")
    }

    pub fn state(&self) -> PathBuf {
        self.0.join("state.bin")
    }

    pub fn history(&self) -> PathBuf {
        self.0.join("history.jsonl")
    }

    pub fn init(&self) -> Result<Identity> {
        if self.key().exists() {
            bail!("{} already has keys", self.0.display());
        }
        std::fs::create_dir_all(&self.0)?;
        let identity = Identity::generate(&mut OsEntropy);
        write_secret(&self.key(), &hex::encode(identity.secret_bytes()))?;
        Ok(identity)
    }

    pub fn identity(&self) -> Result<Identity> {
        let hex = std::fs::read_to_string(self.key())
            .with_context(|| format!("no keys in {}: run `ferry init`", self.0.display()))?;
        let bytes: [u8; 64] = hex::decode(hex.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .context("identity.key is damaged")?;
        Ok(Identity::from_secret_bytes(&bytes))
    }

    /// Name → contact card.
    pub fn contacts(&self) -> Result<BTreeMap<String, String>> {
        match std::fs::read(self.contacts_file()) {
            Ok(data) => Ok(serde_json::from_slice(&data).context("contacts.json is damaged")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn add_contact(&self, name: &str, card: &str) -> Result<PublicIdentity> {
        if name.is_empty()
            || name.len() > 32
            || !name
                .chars()
                .all(|c| c.is_alphanumeric() || "-_.".contains(c))
        {
            bail!("a contact name is 1-32 letters, digits, `-`, `_` or `.`");
        }
        let public = PublicIdentity::from_card(card)?;
        let mut contacts = self.contacts()?;
        contacts.insert(name.to_string(), public.to_card());
        let mut json = serde_json::to_vec_pretty(&contacts)?;
        json.push(b'\n');
        std::fs::write(self.contacts_file(), json)?;
        Ok(public)
    }

    pub fn contact(&self, name: &str) -> Result<PublicIdentity> {
        let contacts = self.contacts()?;
        let card = contacts
            .get(name)
            .with_context(|| format!("no contact {name:?}: `ferry contact add`"))?;
        Ok(PublicIdentity::from_card(card)?)
    }

    pub fn name_of(&self, who: &PublicIdentity) -> Option<String> {
        let card = who.to_card();
        self.contacts()
            .ok()?
            .into_iter()
            .find(|(_, c)| *c == card)
            .map(|(name, _)| name)
    }

    pub fn node(&self) -> Result<Node> {
        let identity = self.identity()?;
        match std::fs::read(self.state()) {
            Ok(snapshot) => Node::restore(identity, Config::default(), &snapshot)
                .context("state.bin is damaged; move it away to start clean"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Node::new(identity, Config::default()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn write_secret(path: &Path, data: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, data)?;
    Ok(())
}
