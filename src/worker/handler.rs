use super::{
    bootstrap::{BootstrapStatus, TableBootstrap},
    lookup::{LookupStatus, TableLookup},
    refresh::TableRefresh,
    socket::Socket,
    timer::Timer,
    {DhtEvent, OneshotTask, ScheduledTaskCheck},
};
use crate::{
    id::InfoHash,
    message::{
        error_code, Error, GetPeersResponse, Message, MessageBody, OtherResponse, Request,
        Response, Want,
    },
    routing::{
        node::{Node, NodeHandle, NodeStatus},
        table::RoutingTable,
    },
    storage::AnnounceStorage,
    token::{Token, TokenStore},
    transaction::{AIDGenerator, ActionID, TransactionID},
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::convert::AsRef;
use std::net::SocketAddr;
use tokio::{select, sync::mpsc};

const MAX_BOOTSTRAP_ATTEMPTS: usize = 3;
const BOOTSTRAP_GOOD_NODE_THRESHOLD: usize = 10;

/// Actions that we can perform on our RoutingTable.
enum TableAction {
    /// Lookup action.
    Lookup(TableLookup),
    /// Refresh action.
    Refresh(TableRefresh),
    /// Bootstrap action.
    ///
    /// Includes number of bootstrap attempts.
    Bootstrap(TableBootstrap, usize),
}

/// Actions that we want to perform on our RoutingTable after bootstrapping finishes.
#[allow(clippy::large_enum_variant)]
enum PostBootstrapAction {
    /// Future lookup action.
    Lookup(InfoHash, bool),
    /// Future refresh action.
    Refresh(TableRefresh, TransactionID),
}

/// Storage for our EventLoop to invoke actions upon.
pub(crate) struct DhtHandler {
    running: bool,
    command_rx: mpsc::UnboundedReceiver<OneshotTask>,
    timer: Timer<ScheduledTaskCheck>,
    read_only: bool,
    announce_port: Option<u16>,
    socket: Socket,
    token_store: TokenStore,
    aid_generator: AIDGenerator,
    bootstrapping: bool,
    routing_table: RoutingTable,
    active_stores: AnnounceStorage,
    // If future actions is not empty, that means we are still bootstrapping
    // since we will always spin up a table refresh action after bootstrapping.
    future_actions: Vec<PostBootstrapAction>,
    event_tx: mpsc::UnboundedSender<DhtEvent>,
    table_actions: HashMap<ActionID, TableAction>,
}

impl DhtHandler {
    pub fn new(
        table: RoutingTable,
        socket: Socket,
        read_only: bool,
        announce_port: Option<u16>,
        command_rx: mpsc::UnboundedReceiver<OneshotTask>,
        event_tx: mpsc::UnboundedSender<DhtEvent>,
    ) -> Self {
        let mut aid_generator = AIDGenerator::new();

        // Insert the refresh task to execute after the bootstrap
        let mut mid_generator = aid_generator.generate();
        let refresh_trans_id = mid_generator.generate();
        let table_refresh = TableRefresh::new(mid_generator);
        let future_actions = vec![PostBootstrapAction::Refresh(
            table_refresh,
            refresh_trans_id,
        )];

        Self {
            running: true,
            command_rx,
            timer: Timer::new(),
            read_only,
            announce_port,
            socket,
            token_store: TokenStore::new(),
            aid_generator,
            bootstrapping: false,
            routing_table: table,
            active_stores: AnnounceStorage::new(),
            future_actions,
            event_tx,
            table_actions: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        while self.running {
            self.run_once().await
        }
    }

    async fn run_once(&mut self) {
        select! {
            token = self.timer.next(), if !self.timer.is_empty() => {
                // `unwrap` is OK because we checked the timer is non-empty, so it should never
                // return `None`.
                let token = token.unwrap();
                self.handle_timeout(token).await
            }
            command = self.command_rx.recv() => {
                if let Some(command) = command {
                    self.handle_command(command).await
                } else {
                    self.shutdown()
                }
            }
            message = self.socket.recv() => {
                match message {
                    Ok((buffer, addr)) => self.handle_incoming(&buffer, addr).await,
                    Err(error) => warn!("Failed to receive incoming message: {}", error),
                }
            }
        }
    }

    async fn handle_command(&mut self, task: OneshotTask) {
        match task {
            OneshotTask::StartBootstrap(routers, nodes) => {
                self.handle_start_bootstrap(routers, nodes).await;
            }
            OneshotTask::StartLookup(info_hash, should_announce) => {
                self.handle_start_lookup(info_hash, should_announce).await;
            }
        }
    }

    async fn handle_timeout(&mut self, token: ScheduledTaskCheck) {
        match token {
            ScheduledTaskCheck::TableRefresh(trans_id) => {
                self.handle_check_table_refresh(trans_id).await;
            }
            ScheduledTaskCheck::BootstrapTimeout(trans_id) => {
                self.handle_check_bootstrap_timeout(trans_id).await;
            }
            ScheduledTaskCheck::LookupTimeout(trans_id) => {
                self.handle_check_lookup_timeout(trans_id).await;
            }
            ScheduledTaskCheck::LookupEndGame(trans_id) => {
                self.handle_check_lookup_endgame(trans_id).await;
            }
        }
    }

    async fn handle_incoming(&mut self, buffer: &[u8], addr: SocketAddr) {
        let message = match Message::decode(buffer) {
            Ok(message) => message,
            Err(error) => {
                warn!("Received invalid bencode data: {}", error);
                return;
            }
        };

        // Validate response
        if let MessageBody::Response(response) = &message.body {
            // Check if we can interpret the response transaction id as one of ours.
            let trans_id =
                if let Some(trans_id) = TransactionID::from_bytes(&message.transaction_id) {
                    trans_id
                } else {
                    warn!("Received response with invalid transaction id");
                    return;
                };

            // Match the response action id with our current actions
            match (self.table_actions.get(&trans_id.action_id()), response) {
                (Some(TableAction::Lookup(_)), Response::GetPeers(_))
                | (Some(TableAction::Refresh(_)), Response::Other(_))
                | (Some(TableAction::Bootstrap(..)), Response::Other(_)) => (),
                _ => {
                    warn!("Received unsolicited response");
                    return;
                }
            }
        }

        // Do not process requests if we are read only
        // TODO: Add read only flags to messages we send it we are read only!
        // Also, check for read only flags on responses we get before adding nodes
        // to our RoutingTable.
        if self.read_only {
            if let MessageBody::Request(_) = message.body {
                return;
            }
        }

        // Process the given message
        match message.body {
            MessageBody::Request(Request::Ping(p)) => {
                info!("Received a PingRequest");
                let node = NodeHandle::new(p.id, addr);

                // Node requested from us, mark it in the Routingtable
                if let Some(n) = self.routing_table.find_node_mut(&node) {
                    n.remote_request()
                }

                let ping_rsp = OtherResponse {
                    id: self.routing_table.node_id(),
                    nodes_v4: vec![],
                    nodes_v6: vec![],
                };
                let ping_msg = Message {
                    transaction_id: message.transaction_id,
                    body: MessageBody::Response(Response::Other(ping_rsp)),
                };
                let ping_msg = ping_msg.encode();

                if let Err(error) = self.socket.send(&ping_msg, addr).await {
                    error!("Failed to send a ping response: {}", error);
                }
            }
            MessageBody::Request(Request::FindNode(f)) => {
                info!("Received a FindNodeRequest");
                let node = NodeHandle::new(f.id, addr);

                // Node requested from us, mark it in the Routingtable
                if let Some(n) = self.routing_table.find_node_mut(&node) {
                    n.remote_request()
                }

                let want = match f.want {
                    Some(want) => want,
                    None => match self.socket.local_addr() {
                        Ok(SocketAddr::V4(_)) => Want::V4,
                        Ok(SocketAddr::V6(_)) => Want::V6,
                        Err(error) => {
                            error!("Failed to retrieve local socket address: {}", error);
                            return;
                        }
                    },
                };

                // Grab the closest nodes
                let nodes_v4 = if matches!(want, Want::V4 | Want::Both) {
                    self.routing_table
                        .closest_nodes(f.target)
                        .filter(|node| node.addr().is_ipv4())
                        .take(8)
                        .map(|node| *node.handle())
                        .collect()
                } else {
                    vec![]
                };

                let nodes_v6 = if matches!(want, Want::V6 | Want::Both) {
                    self.routing_table
                        .closest_nodes(f.target)
                        .filter(|node| node.addr().is_ipv6())
                        .take(8)
                        .map(|node| *node.handle())
                        .collect()
                } else {
                    vec![]
                };

                let find_node_rsp = OtherResponse {
                    id: self.routing_table.node_id(),
                    nodes_v4,
                    nodes_v6,
                };
                let find_node_msg = Message {
                    transaction_id: message.transaction_id,
                    body: MessageBody::Response(Response::Other(find_node_rsp)),
                };
                let find_node_msg = find_node_msg.encode();

                if let Err(error) = self.socket.send(&find_node_msg, addr).await {
                    error!("Failed to send a find node response: {}", error);
                }
            }
            MessageBody::Request(Request::GetPeers(g)) => {
                info!("Received a GetPeersRequest");
                let node = NodeHandle::new(g.id, addr);

                // Node requested from us, mark it in the Routingtable
                if let Some(n) = self.routing_table.find_node_mut(&node) {
                    n.remote_request()
                }

                // TODO: Check what the maximum number of values we can give without overflowing a udp packet
                // Also, if we arent going to give all of the contacts, we may want to shuffle which ones we give
                let values: Vec<_> = self
                    .active_stores
                    .find_items(&g.info_hash)
                    .filter(|addr| match addr {
                        SocketAddr::V4(_) => true,
                        SocketAddr::V6(_) => {
                            error!("AnnounceStorage contained an IPv6 Address...");
                            false
                        }
                    })
                    .collect();

                // Grab the closest nodes
                let nodes = self
                    .routing_table
                    .closest_nodes(g.info_hash)
                    .take(8)
                    .map(|node| *node.handle())
                    .collect();

                let token = self.token_store.checkout(addr.ip());

                let get_peers_rsp = GetPeersResponse {
                    id: self.routing_table.node_id(),
                    values,
                    nodes,
                    token: token.as_ref().to_vec(),
                };
                let get_peers_msg = Message {
                    transaction_id: message.transaction_id,
                    body: MessageBody::Response(Response::GetPeers(get_peers_rsp)),
                };
                let get_peers_msg = get_peers_msg.encode();

                if let Err(error) = self.socket.send(&get_peers_msg, addr).await {
                    error!("Failed to send a get peers response: {}", error);
                }
            }
            MessageBody::Request(Request::AnnouncePeer(a)) => {
                info!("Received an AnnouncePeerRequest");
                let node = NodeHandle::new(a.id, addr);

                // Node requested from us, mark it in the Routingtable
                if let Some(n) = self.routing_table.find_node_mut(&node) {
                    n.remote_request()
                }

                // Validate the token
                let is_valid = match Token::new(&a.token) {
                    Ok(t) => self.token_store.checkin(addr.ip(), t),
                    Err(_) => false,
                };

                // Create a socket address based on the implied/explicit port number
                let connect_addr = match a.port {
                    None => addr,
                    Some(port) => {
                        let mut addr = addr;
                        addr.set_port(port);
                        addr
                    }
                };

                // Resolve type of response we are going to send
                let response_msg = if !is_valid {
                    // Node gave us an invalid token
                    warn!(
                        "bip_dht: Remote node sent us an invalid token for an AnnounceRequest..."
                    );
                    Message {
                        transaction_id: message.transaction_id,
                        body: MessageBody::Error(Error {
                            code: error_code::PROTOCOL_ERROR,
                            message: "received an invalid token".to_owned(),
                        }),
                    }
                    .encode()
                } else if self.active_stores.add_item(a.info_hash, connect_addr) {
                    // Node successfully stored the value with us, send an announce response
                    Message {
                        transaction_id: message.transaction_id,
                        body: MessageBody::Response(Response::Other(OtherResponse {
                            id: self.routing_table.node_id(),
                            nodes_v4: vec![],
                            nodes_v6: vec![],
                        })),
                    }
                    .encode()
                } else {
                    // Node unsuccessfully stored the value with us, send them an error message
                    // TODO: Spec doesnt actually say what error message to send, or even if we should send one...
                    warn!("bip_dht: AnnounceStorage failed to store contact information because it is full...");

                    Message {
                        transaction_id: message.transaction_id,
                        body: MessageBody::Error(Error {
                            code: error_code::SERVER_ERROR,
                            message: "announce storage is full".to_owned(),
                        }),
                    }
                    .encode()
                };

                if let Err(error) = self.socket.send(&response_msg, addr).await {
                    error!(
                        "bip_dht: Failed to send an announce peer response: {}",
                        error
                    );
                }
            }
            MessageBody::Response(Response::Other(f)) => {
                info!("Received a OtherResponse");
                let trans_id = TransactionID::from_bytes(&message.transaction_id).unwrap();
                let node = Node::as_good(f.id, addr);

                // Add the payload nodes as questionable
                for node in f.nodes_v4 {
                    self.routing_table
                        .add_node(Node::as_questionable(node.id, node.addr));
                }

                let bootstrap_complete = {
                    let opt_bootstrap = match self.table_actions.get_mut(&trans_id.action_id()) {
                        Some(TableAction::Refresh(_)) => {
                            self.routing_table.add_node(node);
                            None
                        }
                        Some(TableAction::Bootstrap(bootstrap, attempts)) => {
                            if !bootstrap.is_router(&node.addr()) {
                                self.routing_table.add_node(node);
                            }
                            Some((bootstrap, attempts))
                        }
                        Some(TableAction::Lookup(_)) => {
                            error!("Resolved a OtherResponse ActionID to a TableLookup");
                            None
                        }
                        None => {
                            error!(
                                "Resolved a TransactionID to a OtherResponse but no action found"
                            );
                            None
                        }
                    };

                    if let Some((bootstrap, attempts)) = opt_bootstrap {
                        match bootstrap
                            .recv_response(
                                addr,
                                &trans_id,
                                &mut self.routing_table,
                                &self.socket,
                                &mut self.timer,
                            )
                            .await
                        {
                            BootstrapStatus::Idle => true,
                            BootstrapStatus::Bootstrapping => false,
                            BootstrapStatus::Failed => {
                                self.event_tx.send(DhtEvent::BootstrapFailed).unwrap_or(());
                                self.shutdown();
                                false
                            }
                            BootstrapStatus::Completed => {
                                if should_rebootstrap(&self.routing_table) {
                                    match attempt_rebootstrap(
                                        bootstrap,
                                        attempts,
                                        &self.routing_table,
                                        &self.socket,
                                        &mut self.timer,
                                    )
                                    .await
                                    {
                                        Some(bootstrap_started) => !bootstrap_started,
                                        None => {
                                            self.shutdown();
                                            false
                                        }
                                    }
                                } else {
                                    true
                                }
                            }
                        }
                    } else {
                        false
                    }
                };

                if bootstrap_complete {
                    self.broadcast_bootstrap_completed(trans_id.action_id())
                        .await;
                }

                if log_enabled!(log::Level::Info) {
                    let mut total = 0;

                    for (index, bucket) in self.routing_table.buckets().enumerate() {
                        let num_nodes = bucket
                            .iter()
                            .filter(|n| n.status() == NodeStatus::Good)
                            .count();
                        total += num_nodes;

                        if num_nodes != 0 {
                            print!("Bucket {}: {} | ", index, num_nodes);
                        }
                    }

                    print!("\nTotal: {}\n\n\n", total);
                }
            }
            MessageBody::Response(Response::GetPeers(g)) => {
                // info!("bip_dht: Received a GetPeersResponse...");
                let trans_id = TransactionID::from_bytes(&message.transaction_id).unwrap();
                let node = Node::as_good(g.id, addr);

                self.routing_table.add_node(node.clone());

                let opt_lookup = {
                    match self.table_actions.get_mut(&trans_id.action_id()) {
                        Some(TableAction::Lookup(lookup)) => Some(lookup),
                        Some(TableAction::Refresh(_)) => {
                            error!(
                                "bip_dht: Resolved a GetPeersResponse ActionID to a \
                                TableRefresh..."
                            );
                            None
                        }
                        Some(TableAction::Bootstrap(_, _)) => {
                            error!(
                                "bip_dht: Resolved a GetPeersResponse ActionID to a \
                                TableBootstrap..."
                            );
                            None
                        }
                        None => {
                            error!(
                                "bip_dht: Resolved a TransactionID to a GetPeersResponse but no \
                                action found..."
                            );
                            None
                        }
                    }
                };

                if let Some(lookup) = opt_lookup {
                    match lookup
                        .recv_response(
                            node,
                            &trans_id,
                            g,
                            &mut self.routing_table,
                            &self.socket,
                            &mut self.timer,
                        )
                        .await
                    {
                        LookupStatus::Searching => (),
                        LookupStatus::Completed => self
                            .event_tx
                            .send(DhtEvent::LookupCompleted(lookup.info_hash()))
                            .unwrap_or(()),
                        LookupStatus::Values(values) => {
                            for addr in values {
                                self.event_tx
                                    .send(DhtEvent::PeerFound(lookup.info_hash(), addr))
                                    .unwrap_or(());
                            }
                        }
                    }
                }
            }
            MessageBody::Error(e) => {
                warn!("Received an ErrorMessage from {}: {:?}", addr, e);
            }
        }
    }

    async fn handle_start_bootstrap(
        &mut self,
        routers: HashSet<SocketAddr>,
        nodes: HashSet<SocketAddr>,
    ) {
        let mid_generator = self.aid_generator.generate();
        let action_id = mid_generator.action_id();
        let mut table_bootstrap = TableBootstrap::new(mid_generator, nodes, routers);

        // Begin the bootstrap operation
        let bootstrap_status = table_bootstrap
            .start_bootstrap(self.routing_table.node_id(), &self.socket, &mut self.timer)
            .await;

        self.bootstrapping = true;
        self.table_actions
            .insert(action_id, TableAction::Bootstrap(table_bootstrap, 0));

        let bootstrap_complete = match bootstrap_status {
            BootstrapStatus::Idle => true,
            BootstrapStatus::Bootstrapping => false,
            BootstrapStatus::Failed => {
                self.event_tx.send(DhtEvent::BootstrapFailed).unwrap_or(());
                self.shutdown();
                false
            }
            BootstrapStatus::Completed => {
                // Check if our bootstrap was actually good
                if should_rebootstrap(&self.routing_table) {
                    let (bootstrap, attempts) = match self.table_actions.get_mut(&action_id) {
                        Some(&mut TableAction::Bootstrap(ref mut bootstrap, ref mut attempts)) => {
                            (bootstrap, attempts)
                        }
                        _ => panic!("bip_dht: Bug, in DhtHandler..."),
                    };

                    match attempt_rebootstrap(
                        bootstrap,
                        attempts,
                        &self.routing_table,
                        &self.socket,
                        &mut self.timer,
                    )
                    .await
                    {
                        Some(bootstrap_started) => !bootstrap_started,
                        None => {
                            self.shutdown();
                            false
                        }
                    }
                } else {
                    true
                }
            }
        };

        if bootstrap_complete {
            self.broadcast_bootstrap_completed(action_id).await;
        }
    }

    async fn handle_check_bootstrap_timeout(&mut self, trans_id: TransactionID) {
        let bootstrap_complete = {
            let opt_bootstrap_info = match self.table_actions.get_mut(&trans_id.action_id()) {
                Some(TableAction::Bootstrap(bootstrap, attempts)) => Some((
                    bootstrap
                        .recv_timeout(
                            &trans_id,
                            &mut self.routing_table,
                            &self.socket,
                            &mut self.timer,
                        )
                        .await,
                    bootstrap,
                    attempts,
                )),
                Some(TableAction::Lookup(_)) => {
                    error!(
                        "bip_dht: Resolved a TransactionID to a check table bootstrap but \
                        TableLookup found..."
                    );
                    None
                }
                Some(TableAction::Refresh(_)) => {
                    error!(
                        "bip_dht: Resolved a TransactionID to a check table bootstrap but \
                        TableRefresh found..."
                    );
                    None
                }
                None => {
                    error!(
                        "bip_dht: Resolved a TransactionID to a check table bootstrap but no \
                        action found..."
                    );
                    None
                }
            };

            match opt_bootstrap_info {
                None => false,
                Some((BootstrapStatus::Idle, _, _)) => true,
                Some((BootstrapStatus::Bootstrapping, _, _)) => false,
                Some((BootstrapStatus::Failed, _, _)) => {
                    self.event_tx.send(DhtEvent::BootstrapFailed).unwrap_or(());
                    self.shutdown();
                    false
                }
                Some((BootstrapStatus::Completed, bootstrap, attempts)) => {
                    // Check if our bootstrap was actually good
                    if should_rebootstrap(&self.routing_table) {
                        match attempt_rebootstrap(
                            bootstrap,
                            attempts,
                            &self.routing_table,
                            &self.socket,
                            &mut self.timer,
                        )
                        .await
                        {
                            Some(bootstrap_started) => !bootstrap_started,
                            None => {
                                self.shutdown();
                                false
                            }
                        }
                    } else {
                        true
                    }
                }
            }
        };

        if bootstrap_complete {
            self.broadcast_bootstrap_completed(trans_id.action_id())
                .await;
        }
    }

    /// Broadcast that the bootstrap has completed.
    /// IMPORTANT: Should call this instead of just sending the event!
    async fn broadcast_bootstrap_completed(&mut self, action_id: ActionID) {
        // Send notification that the bootstrap has completed.
        self.event_tx
            .send(DhtEvent::BootstrapCompleted)
            .unwrap_or(());

        // Indicates we are out of the bootstrapping phase
        self.bootstrapping = false;

        // Remove the bootstrap action from our table actions
        self.table_actions.remove(&action_id);

        // Start the post bootstrap actions.
        let mut future_actions = self.future_actions.split_off(0);
        for table_action in future_actions.drain(..) {
            match table_action {
                PostBootstrapAction::Lookup(info_hash, should_announce) => {
                    self.handle_start_lookup(info_hash, should_announce).await;
                }
                PostBootstrapAction::Refresh(refresh, trans_id) => {
                    self.table_actions
                        .insert(trans_id.action_id(), TableAction::Refresh(refresh));

                    self.handle_check_table_refresh(trans_id).await;
                }
            }
        }
    }

    async fn handle_start_lookup(&mut self, info_hash: InfoHash, should_announce: bool) {
        let mid_generator = self.aid_generator.generate();
        let action_id = mid_generator.action_id();

        if self.bootstrapping {
            // Queue it up if we are currently bootstrapping
            self.future_actions
                .push(PostBootstrapAction::Lookup(info_hash, should_announce));
        } else {
            // Start the lookup right now if not bootstrapping
            let lookup = TableLookup::new(
                info_hash,
                mid_generator,
                should_announce,
                &mut self.routing_table,
                &self.socket,
                &mut self.timer,
            )
            .await;
            self.table_actions
                .insert(action_id, TableAction::Lookup(lookup));
        }
    }

    async fn handle_check_lookup_timeout(&mut self, trans_id: TransactionID) {
        let opt_lookup_info = match self.table_actions.get_mut(&trans_id.action_id()) {
            Some(TableAction::Lookup(lookup)) => Some((
                lookup
                    .recv_timeout(
                        &trans_id,
                        &mut self.routing_table,
                        &self.socket,
                        &mut self.timer,
                    )
                    .await,
                lookup.info_hash(),
            )),
            Some(TableAction::Bootstrap(_, _)) => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but TableBootstrap \
                    found..."
                );
                None
            }
            Some(TableAction::Refresh(_)) => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but TableRefresh \
                    found..."
                );
                None
            }
            None => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but no action \
                    found..."
                );
                None
            }
        };

        match opt_lookup_info {
            None => (),
            Some((LookupStatus::Searching, _)) => (),
            Some((LookupStatus::Completed, info_hash)) => self
                .event_tx
                .send(DhtEvent::LookupCompleted(info_hash))
                .unwrap_or(()),
            Some((LookupStatus::Values(v), info_hash)) => {
                // Add values to handshaker
                for addr in v {
                    self.event_tx
                        .send(DhtEvent::PeerFound(info_hash, addr))
                        .unwrap_or(());
                }
            }
        }
    }

    async fn handle_check_lookup_endgame(&mut self, trans_id: TransactionID) {
        let opt_lookup_info = match self.table_actions.remove(&trans_id.action_id()) {
            Some(TableAction::Lookup(mut lookup)) => Some((
                lookup
                    .recv_finished(self.announce_port, &mut self.routing_table, &self.socket)
                    .await,
                lookup.info_hash(),
            )),
            Some(TableAction::Bootstrap(_, _)) => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but TableBootstrap \
                    found..."
                );
                None
            }
            Some(TableAction::Refresh(_)) => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but TableRefresh \
                    found..."
                );
                None
            }
            None => {
                error!(
                    "bip_dht: Resolved a TransactionID to a check table lookup but no action \
                    found..."
                );
                None
            }
        };

        match opt_lookup_info {
            None => (),
            Some((LookupStatus::Searching, _)) => (),
            Some((LookupStatus::Completed, info_hash)) => self
                .event_tx
                .send(DhtEvent::LookupCompleted(info_hash))
                .unwrap_or(()),
            Some((LookupStatus::Values(v), info_hash)) => {
                // Add values to handshaker
                for addr in v {
                    self.event_tx
                        .send(DhtEvent::PeerFound(info_hash, addr))
                        .unwrap_or(());
                }
            }
        }
    }

    async fn handle_check_table_refresh(&mut self, trans_id: TransactionID) {
        match self.table_actions.get_mut(&trans_id.action_id()) {
            Some(TableAction::Refresh(refresh)) => {
                refresh
                    .continue_refresh(&mut self.routing_table, &self.socket, &mut self.timer)
                    .await
            }
            Some(TableAction::Lookup(_)) => {
                error!("Resolved a TransactionID to a check table refresh but TableLookup found");
            }
            Some(TableAction::Bootstrap(_, _)) => {
                error!(
                    "Resolved a TransactionID to a check table refresh but TableBootstrap found"
                );
            }
            None => {
                error!("Resolved a TransactionID to a check table refresh but no action found");
            }
        };
    }

    fn shutdown(&mut self) {
        self.running = false;
    }
}

// ----------------------------------------------------------------------------//

/// Number of good nodes in the RoutingTable.
fn num_good_nodes(table: &RoutingTable) -> usize {
    table
        .closest_nodes(table.node_id())
        .filter(|n| n.status() == NodeStatus::Good)
        .count()
}

/// We should rebootstrap if we have a low number of nodes.
fn should_rebootstrap(table: &RoutingTable) -> bool {
    num_good_nodes(table) <= BOOTSTRAP_GOOD_NODE_THRESHOLD
}

/// Attempt to rebootstrap or shutdown the dht if we have no nodes after rebootstrapping multiple time.
/// Returns None if the DHT is shutting down, Some(true) if the rebootstrap process started,
/// Some(false) if a rebootstrap is not necessary.
async fn attempt_rebootstrap(
    bootstrap: &mut TableBootstrap,
    attempts: &mut usize,
    routing_table: &RoutingTable,
    socket: &Socket,
    timer: &mut Timer<ScheduledTaskCheck>,
) -> Option<bool> {
    loop {
        // Increment the bootstrap counter
        *attempts += 1;

        warn!(
            "bip_dht: Bootstrap attempt {} failed, attempting a rebootstrap...",
            *attempts
        );

        // Check if we reached the maximum bootstrap attempts
        if *attempts >= MAX_BOOTSTRAP_ATTEMPTS {
            if num_good_nodes(routing_table) == 0 {
                // Failed to get any nodes in the rebootstrap attempts, shut down
                return None;
            } else {
                return Some(false);
            }
        } else {
            let bootstrap_status = bootstrap
                .start_bootstrap(routing_table.node_id(), socket, timer)
                .await;

            match bootstrap_status {
                BootstrapStatus::Idle => return Some(false),
                BootstrapStatus::Bootstrapping => return Some(true),
                BootstrapStatus::Failed => {
                    return None;
                }
                BootstrapStatus::Completed => {
                    if !should_rebootstrap(routing_table) {
                        return Some(false);
                    }
                }
            }
        }
    }
}
