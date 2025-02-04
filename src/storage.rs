use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::id::InfoHash;

const MAX_ITEMS_STORED: usize = 500;

/// Manages storage and expiration of contact information for a number of InfoHashs.
pub struct AnnounceStorage {
    storage: HashMap<InfoHash, Vec<AnnounceItem>>,
    expires: Vec<ItemExpiration>,
}

impl AnnounceStorage {
    /// Create a new AnnounceStorage object.
    pub fn new() -> AnnounceStorage {
        AnnounceStorage {
            storage: HashMap::new(),
            expires: Vec::new(),
        }
    }

    /// Returns true if the item was added/it's existing expiration updated, false otherwise.
    pub fn add_item(&mut self, info_hash: InfoHash, address: SocketAddr) -> bool {
        self.add(info_hash, address, Instant::now())
    }

    fn add(&mut self, info_hash: InfoHash, address: SocketAddr, curr_time: Instant) -> bool {
        // Clear out any old contacts that we have stored
        self.remove_expired_items(curr_time);
        let item = AnnounceItem::new(info_hash, address);
        let item_expiration = item.expiration();

        // Check if we already have the item and want to update it's expiration
        match self.insert_contact(item) {
            Some(true) => {
                self.expires.retain(|i| i != &item_expiration);
                self.expires.push(item_expiration);

                true
            }
            Some(false) => {
                self.expires.push(item_expiration);

                true
            }
            None => false,
        }
    }

    /// Returns an iterator over all contacts for the given info hash.
    pub fn find_items<'a>(
        &'a mut self,
        info_hash: &'_ InfoHash,
    ) -> impl Iterator<Item = SocketAddr> + 'a {
        self.find(info_hash, Instant::now())
    }

    fn find<'a>(
        &'a mut self,
        info_hash: &'_ InfoHash,
        curr_time: Instant,
    ) -> impl Iterator<Item = SocketAddr> + 'a {
        // Clear out any old contacts that we have stored
        self.remove_expired_items(curr_time);

        self.storage
            .get(info_hash)
            .into_iter()
            .flatten()
            .map(|item| item.address())
    }

    /// Returns None if the contact could not be inserted, else, returns Some(true) if the contact was already
    /// in the table (and was replaced by the new entry) or Some(false) if the contact was not already in the
    /// table but was inserted.
    fn insert_contact(&mut self, item: AnnounceItem) -> Option<bool> {
        let item_info_hash = item.info_hash();

        // Check if the contact is already in our list
        let already_in_list = if let Some(items) = self.storage.get_mut(&item_info_hash) {
            items.iter().any(|a| a == &item)
        } else {
            false
        };

        // Check if we need to insert it into the list and if we have room
        match (already_in_list, self.expires.len() < MAX_ITEMS_STORED) {
            (false, true) => {
                // Place it into the appropriate list
                match self.storage.entry(item_info_hash) {
                    Entry::Occupied(mut occ) => occ.get_mut().push(item),
                    Entry::Vacant(vac) => {
                        vac.insert(vec![item]);
                    }
                };

                Some(false)
            }
            (false, false) => None,
            (true, false) => Some(true),
            (true, true) => Some(true),
        }
    }

    /// Prunes all expired items from the internal list.
    fn remove_expired_items(&mut self, curr_time: Instant) {
        let num_expired_items = self
            .expires
            .iter()
            .take_while(|i| i.is_expired(curr_time))
            .count();

        // Remove the numbers of expired elements from the head of the list
        for item_expiration in self.expires.drain(0..num_expired_items) {
            let info_hash = item_expiration.info_hash();

            // Get a mutable reference to the list of contacts and remove all contacts that
            // are associated with the expiration (should only be one such contact).
            let remove_info_hash = if let Some(items) = self.storage.get_mut(&info_hash) {
                items.retain(|a| a.expiration() != item_expiration);

                items.is_empty()
            } else {
                false
            };

            // If we drained the list of contacts completely, remove the info hash entry
            if remove_info_hash {
                self.storage.remove(&info_hash);
            }
        }
    }
}

// ----------------------------------------------------------------------------//

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnnounceItem {
    expiration: ItemExpiration,
}

impl AnnounceItem {
    pub fn new(info_hash: InfoHash, address: SocketAddr) -> AnnounceItem {
        AnnounceItem {
            expiration: ItemExpiration::new(info_hash, address),
        }
    }

    pub fn expiration(&self) -> ItemExpiration {
        self.expiration.clone()
    }

    pub fn address(&self) -> SocketAddr {
        self.expiration.address()
    }

    pub fn info_hash(&self) -> InfoHash {
        self.expiration.info_hash()
    }
}

// ----------------------------------------------------------------------------//

const EXPIRATION_TIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
struct ItemExpiration {
    address: SocketAddr,
    inserted: Instant,
    info_hash: InfoHash,
}

impl ItemExpiration {
    pub fn new(info_hash: InfoHash, address: SocketAddr) -> ItemExpiration {
        ItemExpiration {
            address,
            inserted: Instant::now(),
            info_hash,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now - self.inserted >= EXPIRATION_TIME
    }

    pub fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl PartialEq for ItemExpiration {
    fn eq(&self, other: &ItemExpiration) -> bool {
        self.address() == other.address() && self.info_hash() == other.info_hash()
    }
}

impl Eq for ItemExpiration {}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::id::INFO_HASH_LEN;
    use crate::storage::{self, AnnounceStorage};
    use crate::test;

    #[test]
    fn positive_add_and_retrieve_contact() {
        let mut announce_store = AnnounceStorage::new();
        let info_hash = [0u8; INFO_HASH_LEN].into();
        let sock_addr = test::dummy_socket_addr_v4();

        assert!(announce_store.add_item(info_hash, sock_addr));

        let items: Vec<_> = announce_store.find_items(&info_hash).collect();
        assert_eq!(items.len(), 1);

        assert_eq!(items[0], sock_addr);
    }

    #[test]
    fn positive_add_and_retrieve_contacts() {
        let mut announce_store = AnnounceStorage::new();
        let info_hash = [0u8; INFO_HASH_LEN].into();
        let sock_addrs = test::dummy_block_socket_addrs(storage::MAX_ITEMS_STORED as u16);

        for sock_addr in sock_addrs.iter() {
            assert!(announce_store.add_item(info_hash, *sock_addr));
        }

        let items: Vec<_> = announce_store.find_items(&info_hash).collect();
        assert_eq!(items.len(), storage::MAX_ITEMS_STORED);

        for item in items.iter() {
            assert!(sock_addrs.iter().any(|s| s == item));
        }
    }

    #[test]
    fn positive_renew_contacts() {
        let mut announce_store = AnnounceStorage::new();
        let info_hash = [0u8; INFO_HASH_LEN].into();
        let sock_addrs = test::dummy_block_socket_addrs((storage::MAX_ITEMS_STORED + 1) as u16);

        for sock_addr in sock_addrs.iter().take(storage::MAX_ITEMS_STORED) {
            assert!(announce_store.add_item(info_hash, *sock_addr));
        }

        // Try to add a new item
        let other_info_hash = [1u8; INFO_HASH_LEN].into();

        // Returns false because it wasnt added
        assert!(!announce_store.add_item(other_info_hash, sock_addrs[sock_addrs.len() - 1]));
        // Iterator is empty because it wasnt added
        let count = announce_store.find_items(&other_info_hash).count();
        assert_eq!(count, 0);

        // Try to add all of the initial nodes again (renew)
        for sock_addr in sock_addrs.iter().take(storage::MAX_ITEMS_STORED) {
            assert!(announce_store.add_item(info_hash, *sock_addr));
        }
    }

    #[test]
    fn positive_full_storage_expire_one_infohash() {
        let mut announce_store = AnnounceStorage::new();
        let info_hash = [0u8; INFO_HASH_LEN].into();
        let sock_addrs = test::dummy_block_socket_addrs((storage::MAX_ITEMS_STORED + 1) as u16);

        // Fill up the announce storage completely
        for sock_addr in sock_addrs.iter().take(storage::MAX_ITEMS_STORED) {
            assert!(announce_store.add_item(info_hash, *sock_addr));
        }

        // Try to add a new item into the storage (under a different info hash)
        let other_info_hash = [1u8; INFO_HASH_LEN].into();

        // Returned false because it wasnt added
        assert!(!announce_store.add_item(other_info_hash, sock_addrs[sock_addrs.len() - 1]));
        // Iterator is empty because it wasnt added
        let count = announce_store.find_items(&other_info_hash).count();
        assert_eq!(count, 0);

        // Try to add a new item into the storage mocking the current time
        let mock_current_time = Instant::now() + storage::EXPIRATION_TIME;
        assert!(announce_store.add(
            other_info_hash,
            sock_addrs[sock_addrs.len() - 1],
            mock_current_time
        ));
        // Iterator is not empty because it was added
        let count = announce_store.find_items(&other_info_hash).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn positive_full_storage_expire_two_infohash() {
        let mut announce_store = AnnounceStorage::new();
        let info_hash_one = [0u8; INFO_HASH_LEN].into();
        let info_hash_two = [1u8; INFO_HASH_LEN].into();
        let sock_addrs = test::dummy_block_socket_addrs((storage::MAX_ITEMS_STORED + 1) as u16);

        // Fill up first info hash
        let num_contacts_first = storage::MAX_ITEMS_STORED / 2;
        for sock_addr in sock_addrs.iter().take(num_contacts_first) {
            assert!(announce_store.add_item(info_hash_one, *sock_addr));
        }

        // Fill up second info hash
        let num_contacts_second = storage::MAX_ITEMS_STORED - num_contacts_first;
        for sock_addr in sock_addrs
            .iter()
            .skip(num_contacts_first)
            .take(num_contacts_second)
        {
            assert!(announce_store.add_item(info_hash_two, *sock_addr));
        }

        // Try to add a third info hash with a contact
        let info_hash_three = [2u8; INFO_HASH_LEN].into();
        assert!(!announce_store.add_item(info_hash_three, sock_addrs[sock_addrs.len() - 1]));
        // Iterator is empty because it was not added
        let count = announce_store.find_items(&info_hash_three).count();
        assert_eq!(count, 0);

        // Try to add a new item into the storage mocking the current time
        let mock_current_time = Instant::now() + storage::EXPIRATION_TIME;
        assert!(announce_store.add(
            info_hash_three,
            sock_addrs[sock_addrs.len() - 1],
            mock_current_time
        ));
        // Iterator is not empty because it was added
        let count = announce_store.find_items(&info_hash_three).count();
        assert_eq!(count, 1);
    }
}
