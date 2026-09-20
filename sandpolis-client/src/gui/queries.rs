use anyhow::Result;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::network::NetworkManager;

/// Instance metadata returned from database queries
#[derive(Clone, Debug)]
pub struct InstanceMetadata {
    pub instance_id: InstanceId,
    pub os_type: os_info::Type,
    pub hostname: Option<String>,
    pub is_server: bool,
}

/// Network edge between two instances
#[derive(Clone, Debug)]
pub struct NetworkEdge {
    pub from: InstanceId,
    pub to: InstanceId,
}

/// Query all instances from the database
/// This is the initial query run on startup to spawn all nodes
///
/// Both sources matter: the identity rows say which instances *exist* in the
/// estate, including every agent this client has no connection to, while the
/// connection rows cover a server that's dialed but whose identity row hasn't
/// replicated in yet.
pub fn query_all_instances(
    instance_manager: &InstanceManager,
    network_manager: &NetworkManager,
) -> Result<Vec<InstanceId>> {
    let mut instance_ids = vec![instance_manager.instance_id];

    for instance in instance_manager.instances().iter() {
        let id = instance.read()._instance_id;
        if !instance_ids.contains(&id) {
            instance_ids.push(id);
        }
    }

    for connection in network_manager.connections.iter() {
        let conn = connection.read();
        if let Some(remote) = conn.remote_instance
            && !instance_ids.contains(&remote)
        {
            instance_ids.push(remote);
        }
    }

    Ok(instance_ids)
}

/// Query metadata for a specific instance
pub fn query_instance_metadata(id: InstanceId) -> Result<InstanceMetadata> {
    // TODO: Query instance data from database
    // For now, return current system's OS info
    let os_info = os_info::get();
    Ok(InstanceMetadata {
        instance_id: id,
        os_type: os_info.os_type(),
        hostname: None, // TODO: Get hostname from database or gethostname crate
        is_server: id.is_server(),
    })
}

/// Query network topology (edges between instances)
/// Returns list of connections for the current layer
pub fn query_network_topology(network_manager: &NetworkManager) -> Result<Vec<NetworkEdge>> {
    let mut edges = Vec::new();

    // Get all connections and build edges
    for connection in network_manager.connections.iter() {
        let conn = connection.read();
        // Create edge from local instance to remote instance; an unidentified
        // peer has no node to draw an edge to.
        if let Some(remote) = conn.remote_instance {
            edges.push(NetworkEdge {
                from: conn._instance_id,
                to: remote,
            });
        }
    }

    Ok(edges)
}

/// Network statistics for an instance
#[derive(Clone, Debug)]
pub struct NetworkStats {
    pub latency_ms: Option<u64>,
    pub throughput_bps: Option<u64>,
}

/// Query network stats for a specific instance
pub fn query_network_stats(
    _network_manager: &NetworkManager,
    _id: InstanceId,
) -> Result<NetworkStats> {
    // TODO: Query from network resident
    Ok(NetworkStats {
        latency_ms: None,
        throughput_bps: None,
    })
}

/// Active file transfer
#[derive(Clone, Debug)]
pub struct FileTransfer {
    pub from: InstanceId,
    pub to: InstanceId,
    pub filename: String,
    pub progress: f32, // 0.0 to 1.0
}

/// Query active file transfers
pub fn query_active_transfers() -> Result<Vec<FileTransfer>> {
    // TODO: Query from filesystem subsystem
    Ok(vec![])
}
