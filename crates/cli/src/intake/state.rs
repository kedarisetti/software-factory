//! String cursors for MCP sources. `Store::set_cursor` holds integers only, so these live in a small
//! JSON file next to the db (`<db>.intake-state.json`), written atomically (tmp + rename).

use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct State {
    path: PathBuf,
}

impl State {
    pub fn beside(db: &str) -> Self {
        State { path: PathBuf::from(format!("{db}.intake-state.json")) }
    }

    fn read(&self) -> Result<BTreeMap<String, String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.read()?.remove(key))
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut m = self.read()?;
        m.insert(key.into(), value.into());
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &self.path)).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}
