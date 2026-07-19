use std::sync::{Mutex, MutexGuard};

fn env_mutex() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// Snapshot + restore environment variables around a test.
///
/// `ops` entries are `(key, Some(value))` to set or `(key, None)` to remove.
/// Values are always restored on drop, including panic/assertion paths.
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub fn apply(ops: &[(&'static str, Option<&str>)]) -> Self {
        let lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let saved = ops
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in ops {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
    }
}
