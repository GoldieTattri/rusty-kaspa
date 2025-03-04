use crate::connection::Connection;
use kaspa_core::debug;
use kaspa_notify::connection::Connection as ConnectionT;
use parking_lot::RwLock;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

#[derive(Clone, Debug)]
pub struct Manager {
    connections: Arc<RwLock<HashMap<SocketAddr, Connection>>>,
    max_connections: usize,
}

impl Manager {
    pub fn new(max_connections: usize) -> Self {
        Self { connections: Arc::new(RwLock::new(HashMap::new())), max_connections }
    }

    pub fn register(&self, connection: Connection) {
        debug!("gRPC: Register a new connection from {connection}");
        self.connections.write().insert(connection.identity(), connection).map(|x| x.close());
    }

    pub fn is_full(&self) -> bool {
        self.connections.read().len() >= self.max_connections
    }

    pub fn unregister(&self, net_address: SocketAddr) {
        if let Some(connection) = self.connections.write().remove(&net_address) {
            debug!("gRPC: Unregister the gRPC connection from {connection}");
        }
    }

    /// Terminate all connections
    pub fn terminate_all_connections(&self) {
        let connections = self.connections.write().drain().map(|(_, r)| r).collect::<Vec<_>>();
        for connection in connections {
            connection.close();
        }
    }

    /// Returns a list of all currently active connections
    pub fn active_connections(&self) -> Vec<SocketAddr> {
        self.connections.read().values().map(|r| r.net_address()).collect()
    }

    /// Returns whether there are currently active connections
    pub fn has_connections(&self) -> bool {
        !self.connections.read().is_empty()
    }

    /// Returns whether a connection matching `net_address` is registered
    pub fn has_connection(&self, net_address: SocketAddr) -> bool {
        self.connections.read().contains_key(&net_address)
    }
}
