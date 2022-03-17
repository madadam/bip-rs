use super::{socket::Socket, timer::Timer, ScheduledTaskCheck};
use crate::message::{FindNodeRequest, Message, MessageBody, Request};
use crate::routing::node::NodeStatus;
use crate::routing::table::{self, RoutingTable};
use crate::transaction::{ActionID, MIDGenerator};
use std::time::Duration;

const REFRESH_INTERVAL_TIMEOUT: Duration = Duration::from_millis(6000);

pub(crate) struct TableRefresh {
    id_generator: MIDGenerator,
    curr_refresh_bucket: usize,
}

impl TableRefresh {
    pub fn new(id_generator: MIDGenerator) -> TableRefresh {
        TableRefresh {
            id_generator,
            curr_refresh_bucket: 0,
        }
    }

    pub fn action_id(&self) -> ActionID {
        self.id_generator.action_id()
    }

    pub async fn continue_refresh(
        &mut self,
        table: &mut RoutingTable,
        socket: &Socket,
        timer: &mut Timer<ScheduledTaskCheck>,
    ) {
        if self.curr_refresh_bucket == table::MAX_BUCKETS {
            self.curr_refresh_bucket = 0;
        }
        let target_id = table.node_id().flip_bit(self.curr_refresh_bucket);

        log::info!(
            "Performing a refresh for bucket {}",
            self.curr_refresh_bucket
        );
        // Ping the closest questionable node
        if let Some(node) = table
            .closest_nodes(target_id)
            .find(|n| n.status() == NodeStatus::Questionable)
            .map(|node| *node.handle())
        {
            // Generate a transaction id for the request
            let trans_id = self.id_generator.generate();

            // Construct the message
            let find_node_req = FindNodeRequest {
                id: table.node_id(),
                target: target_id,
                want: None,
            };
            let find_node_msg = Message {
                transaction_id: trans_id.as_ref().to_vec(),
                body: MessageBody::Request(Request::FindNode(find_node_req)),
            };
            let find_node_msg = find_node_msg.encode();

            // Send the message
            if let Err(error) = socket.send(&find_node_msg, node.addr).await {
                log::error!("TableRefresh failed to send a refresh message: {}", error);
            }

            // Mark that we requested from the node
            if let Some(node) = table.find_node_mut(&node) {
                node.local_request();
            }
        }

        // Start a timer for the next refresh
        timer.schedule_in(REFRESH_INTERVAL_TIMEOUT, ScheduledTaskCheck::TableRefresh);

        self.curr_refresh_bucket += 1;
    }
}
