//! Inspector TCP server.
//!
//! Streams incremental cell updates to connected clients over JSON-lines.
//! First frame is a full snapshot; subsequent frames contain only diffs.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::Serialize;
use tokio::{io::AsyncWriteExt, net::TcpListener};
use uuid::Uuid;

use crate::registry::{CellSnapshot, registry};

const DEFAULT_INTERVAL: Duration = Duration::from_millis(16); // ~60Hz

/// A frame sent over the wire as JSON-lines.
#[derive(Serialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upsert: Vec<CellSnapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remove: Vec<Uuid>,
}

/// Handle to a running inspector server. Shuts down on drop.
pub struct InspectorServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
}

impl InspectorServer {
    /// The TCP port the server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Signal the server to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for InspectorServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Start a new inspector server.
///
/// Port is read from `HYPHA_INSPECTOR_PORT` env var, falling back to OS-assigned.
/// Begins accepting TCP connections on `127.0.0.1`. Each connected client receives
/// an initial full snapshot, then incremental diffs as newline-delimited JSON
/// frames at ~60Hz.
///
/// # Example
///
/// ```no_run
/// let server = hyphae::server::start_server("my-app");
/// println!("Inspector on port {}", server.port());
/// // Server runs until dropped or shutdown() called
/// ```
pub fn start_server(_name: impl Into<String>) -> InspectorServer {
    let port = std::env::var("HYPHA_INSPECTOR_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let shutdown = Arc::new(AtomicBool::new(false));

    // Bind synchronously to get the port before returning
    let listener = {
        let addr = format!("127.0.0.1:{port}");
        let std_listener =
            std::net::TcpListener::bind(&addr).expect("failed to bind inspector server");
        std_listener
            .set_nonblocking(true)
            .expect("failed to set non-blocking");
        TcpListener::from_std(std_listener).expect("failed to convert to tokio listener")
    };
    let port = listener.local_addr().unwrap().port();

    // Spawn the accept loop on the tokio runtime
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        accept_loop(listener, shutdown_clone).await;
    });

    InspectorServer { port, shutdown }
}

async fn accept_loop(listener: TcpListener, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_client(stream, shutdown).await;
                        });
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // Periodic check for shutdown
            }
        }
    }
}

/// Compute a fingerprint for diffing. Includes all fields that the client cares about.
fn fingerprint(cell: &CellSnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cell.subscriber_count.hash(&mut hasher);
    cell.owned_count.hash(&mut hasher);
    cell.name.hash(&mut hasher);
    cell.display_name.hash(&mut hasher);
    cell.owner_id.hash(&mut hasher);
    cell.value.hash(&mut hasher);
    cell.dep_ids.hash(&mut hasher);
    hasher.finish()
}

async fn handle_client(mut stream: tokio::net::TcpStream, shutdown: Arc<AtomicBool>) {
    let mut prev: HashMap<Uuid, u64> = HashMap::new();
    let mut interval = tokio::time::interval(DEFAULT_INTERVAL);
    let mut is_first = true;

    loop {
        interval.tick().await;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let cells = registry().snapshot();

        let mut upsert = Vec::new();
        let mut current_ids: HashMap<Uuid, u64> = HashMap::with_capacity(cells.len());

        for cell in cells {
            let fp = fingerprint(&cell);
            current_ids.insert(cell.id, fp);

            if is_first || prev.get(&cell.id) != Some(&fp) {
                upsert.push(cell);
            }
        }

        let remove: Vec<Uuid> = if is_first {
            Vec::new()
        } else {
            prev.keys()
                .filter(|id| !current_ids.contains_key(id))
                .copied()
                .collect()
        };

        prev = current_ids;
        is_first = false;

        // Skip empty diffs
        if upsert.is_empty() && remove.is_empty() {
            continue;
        }

        let frame = Frame {
            kind: "diff",
            upsert,
            remove,
        };

        let mut line = match serde_json::to_string(&frame) {
            Ok(s) => s,
            Err(_) => break,
        };
        line.push('\n');

        if stream.write_all(line.as_bytes()).await.is_err() {
            break; // Client disconnected
        }
    }
}
