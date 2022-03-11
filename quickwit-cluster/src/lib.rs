// Copyright (C) 2021 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

mod cluster;
mod error;
mod service;

use std::sync::Arc;

use quickwit_config::QuickwitConfig;
use tracing::debug;

pub use crate::cluster::{create_cluster_for_test, Cluster, Member};
pub use crate::error::{ClusterError, ClusterResult};
pub use crate::service::ClusterService;

pub async fn start_cluster_service(
    quickwit_config: &QuickwitConfig,
) -> anyhow::Result<Arc<Cluster>> {
    let cluster = Arc::new(Cluster::new(
        quickwit_config.node_id.clone(),
        quickwit_config.gossip_socket_addr()?,
    )?);
    for seed_socket_addr in quickwit_config.seed_socket_addrs()? {
        // If the peer seed address is specified,
        // it joins the cluster in which that node participates.
        debug!(peer_seed_addr = %seed_socket_addr, "Add peer seed node.");
        cluster.add_peer_node(seed_socket_addr).await;
    }
    Ok(cluster)
}
