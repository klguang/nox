/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::io;

use async_std::task;
use futures::channel::{mpsc, oneshot};
use futures::stream::FusedStream;
use futures::{select, stream::StreamExt};
use futures_util::future::FutureExt;
use libp2p::{identity, PeerId, Swarm, TransportError};
use log::{error, trace};
use parity_multiaddr::{Multiaddr, Protocol};

use crate::config::config::NodeServiceConfig;
use crate::node_service::{
    p2p::{build_transport, NodeServiceBehaviour},
    relay::RelayEvent,
};
use crate::peer_service::libp2p::events::{ToNodeMsg, ToPeerMsg};

type NodeServiceSwarm = Swarm<NodeServiceBehaviour>;

/// Responsibilities:
/// - Command swarm to listen for other nodes
/// - Handle events from peers and send them to swarm
/// - Proxy events from swarm to peer service
pub struct NodeService {
    swarm: Box<NodeServiceSwarm>,
    config: NodeServiceConfig,
}

impl NodeService {
    pub fn new(config: NodeServiceConfig) -> Self {
        let NodeServiceConfig {
            socket_timeout,
            key_pair,
            ..
        } = config.clone();

        let local_key = match key_pair {
            Some(kp) => kp,
            None => identity::Keypair::generate_ed25519(),
        };
        let local_peer_id = PeerId::from(local_key.public());
        println!("node service is starting with id = {}", local_peer_id);

        let swarm = {
            let transport = build_transport(local_key.clone(), socket_timeout);
            let behaviour = NodeServiceBehaviour::new(local_peer_id.clone(), local_key.public());

            Box::new(Swarm::new(transport, behaviour, local_peer_id))
        };

        Self { swarm, config }
    }

    /// Starts node service
    /// * `node_inlet`   – channel for receiving events from peer service to node service
    /// * `peer_outlet`  – channel for sending events from node service to peer service
    pub fn start(
        mut self,
        node_inlet: mpsc::UnboundedReceiver<ToNodeMsg>,
        peer_outlet: mpsc::UnboundedSender<ToPeerMsg>,
    ) -> oneshot::Sender<()> {
        let (exit_sender, exit_receiver) = oneshot::channel();

        self.listen().expect("Error on starting listener");
        self.bootstrap();

        task::spawn(self.run_events_coordination(
            peer_outlet,
            node_inlet.fuse(),
            exit_receiver.into_stream().fuse(),
        ));

        exit_sender
    }

    /// Starts node service listener
    #[inline]
    fn listen(&mut self) -> Result<(), TransportError<io::Error>> {
        let mut listen_addr = Multiaddr::from(self.config.listen_ip);
        listen_addr.push(Protocol::Tcp(self.config.listen_port));

        Swarm::listen_on(&mut self.swarm, listen_addr).map(|_| ())
    }

    /// Dials bootstrap nodes, and then commands swarm to bootstrap itself
    #[inline]
    fn bootstrap(&mut self) {
        for addr in &self.config.bootstrap_nodes {
            let dial_result = Swarm::dial_addr(&mut self.swarm, addr.clone());

            if let Err(err) = dial_result {
                error!("Error dialing {}: {}", addr, err)
            }
        }

        self.swarm.bootstrap();
    }

    /// Runs a loop which coordinates events:
    /// peer service => swarm
    /// swarm        => peer service
    ///
    /// Stops when message is received on `exit_receiver`
    // TODO: will move swarm to heap or to stack, call @voronovm!
    #[inline]
    async fn run_events_coordination<U, T>(
        self,
        peer_outlet: mpsc::UnboundedSender<ToPeerMsg>,
        mut node_incoming_events: T,
        mut exit: U,
    ) -> std::result::Result<(), oneshot::Canceled>
    where
        T: Unpin + FusedStream<Item = ToNodeMsg>,
        U: Unpin + FusedStream,
    {
        // stream of RelayEvents
        let mut swarm = self.swarm;

        loop {
            select! {
                // Notice from peer service => swarm
                from_peer = node_incoming_events.next() => {
                    NodeService::handle_peer_event(
                        &mut swarm,
                        from_peer,
                    )
                },

                // swarm stream never ends
                // RelayEvent from swarm => peer_service
                from_node = swarm.select_next_some() => {
                    trace!("node_service/select: sending {:?} to peer_service", from_node);

                    peer_outlet
                        .unbounded_send(ToPeerMsg::Deliver {
                            src_id: PeerId::from_bytes(from_node.src_id).unwrap(),
                            dst_id: PeerId::from_bytes(from_node.dst_id).unwrap(),
                            data: from_node.data,
                        })
                        .unwrap();
                },

                // If any msg received on `exit`, then stop the loop
                _ = exit.next() => {
                    break Ok(())
                }
            }
        }
    }

    /// Handles events from a peer service.
    #[inline]
    fn handle_peer_event(swarm: &mut NodeServiceSwarm, event: Option<ToNodeMsg>) {
        match event {
            Some(ToNodeMsg::PeerConnected { peer_id }) => swarm.add_local_peer(peer_id),

            Some(ToNodeMsg::PeerDisconnected { peer_id }) => swarm.remove_local_peer(peer_id),

            Some(ToNodeMsg::Relay {
                src_id,
                dst_id,
                data,
            }) => swarm.relay(RelayEvent {
                src_id: src_id.into_bytes(),
                dst_id: dst_id.into_bytes(),
                data,
            }),

            // channel is closed when peer service was shut down - does nothing
            // (node service is main service and could run without peer service)
            None => {
                trace!("trying to poll closed channel from the peer service");
            }
        }
    }
}
