#![allow(unused)]

use futures::StreamExt;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

use enostr::{Filter, RelayPool};
use nostrdb::{self, Config, Ndb, NoteKey, Subscription, SubscriptionStream};

/// The Subscription Manager
///
/// NOTE - This interface wishes it was called Subscriptions but there
/// already is one.  Using a lame (but short) placeholder name instead
/// for now ...
///
/// ```no_run
/// use std::error::Error;
///
/// use nostrdb::{Config, Ndb};
/// use enostr::{Filter, RelayPool};
/// use notedeck::subman::{SubConstraint, SubMan, SubSpecBuilder, SubError};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn Error>> {
///     let mut ndb = Ndb::new("the/db/path/", &Config::new())?;
///     let mut subman = SubMan::new(ndb.clone(), RelayPool::new());
///
///     // Define a filter and build the subscription specification
///     let filter = Filter::new().kinds(vec![1, 2, 3]).build();
///     let spec = SubSpecBuilder::new()
///         .filters(vec![filter])
///         .constraint(SubConstraint::Local)
///         .build();
///
///     // Subscribe and obtain a SubReceiver
///     let mut receiver = subman.subscribe(spec)?;
///
///     // Process incoming note keys
///     loop {
///         match receiver.next().await {
///             Ok(note_keys) => {
///                 // Process the note keys
///                 println!("Received note keys: {:?}", note_keys);
///             },
///             Err(SubError::StreamEnded) => {
///                 // Not really an error; we should clean up
///                 break;
///             },
///             Err(err) => {
///                 // Handle other errors
///                 eprintln!("Error: {:?}", err);
///                 break;
///             },
///         }
///     }
///
///     // Unsubscribe when the subscription is no longer needed
///     subman.unsubscribe(&receiver)?;
///
///     Ok(())
/// }
/// ```

#[derive(Debug, Error)]
pub enum SubError {
    #[error("Stream ended")]
    StreamEnded,

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("nostrdb error: {0}")]
    NdbError(#[from] nostrdb::Error),
}

pub type SubResult<T> = Result<T, SubError>;

#[derive(Debug, Clone, Copy)]
pub struct SubId(nostrdb::Subscription);

impl From<Subscription> for SubId {
    fn from(subscription: Subscription) -> Self {
        SubId(subscription)
    }
}

impl Ord for SubId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.id().cmp(&other.0.id())
    }
}

impl PartialOrd for SubId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SubId {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for SubId {}

#[derive(Debug, Clone)]
pub enum SubConstraint {
    OneShot,                    // terminate subscription after initial query and EOSE
    Local,                      // only query the local db, no remote subs
    OutboxRelays(Vec<String>),  // ensure one of these is in the active relay set
    AllowedRelays(Vec<String>), // if not empty, only use these relays
    BlockedRelays(Vec<String>), // if not empty, don't use these relays
}

#[derive(Debug, Default, Clone)]
pub struct SubSpecBuilder {
    rmtid: Option<String>,
    filters: Vec<Filter>,
    constraints: Vec<SubConstraint>,
}

impl SubSpecBuilder {
    pub fn new() -> Self {
        SubSpecBuilder::default()
    }
    pub fn rmtid(mut self, id: String) -> Self {
        self.rmtid = Some(id);
        self
    }
    pub fn filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters.extend(filters);
        self
    }
    pub fn constraint(mut self, constraint: SubConstraint) -> Self {
        self.constraints.push(constraint);
        self
    }
    pub fn build(self) -> SubSpec {
        let mut outbox_relays = Vec::new();
        let mut allowed_relays = Vec::new();
        let mut blocked_relays = Vec::new();
        let mut is_oneshot = false;
        let mut is_local = false;

        for constraint in self.constraints {
            match constraint {
                SubConstraint::OneShot => is_oneshot = true,
                SubConstraint::Local => is_local = true,
                SubConstraint::OutboxRelays(relays) => outbox_relays.extend(relays),
                SubConstraint::AllowedRelays(relays) => allowed_relays.extend(relays),
                SubConstraint::BlockedRelays(relays) => blocked_relays.extend(relays),
            }
        }

        let rmtid = self.rmtid.unwrap_or_else(|| Uuid::new_v4().to_string());

        SubSpec {
            rmtid,
            filters: self.filters,
            outbox_relays,
            allowed_relays,
            blocked_relays,
            is_oneshot,
            is_local,
        }
    }
}

#[derive(Clone)]
pub struct SubSpec {
    rmtid: String,
    filters: Vec<Filter>,
    outbox_relays: Vec<String>,
    allowed_relays: Vec<String>,
    blocked_relays: Vec<String>,
    is_oneshot: bool,
    is_local: bool,
}

impl fmt::Debug for SubSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Convert each Filter to its JSON representation.
        let filters_json: Vec<_> = self
            .filters
            .iter()
            .map(|filter| filter.json().unwrap())
            .collect();
        f.debug_struct("SubSpec")
            .field("rmtid", &self.rmtid)
            .field("filters", &filters_json)
            .field("outbox_relays", &self.outbox_relays)
            .field("allowed_relays", &self.allowed_relays)
            .field("blocked_relays", &self.blocked_relays)
            .field("is_oneshot", &self.is_oneshot)
            .field("is_local", &self.is_local)
            .finish()
    }
}

pub struct SubMan {
    ndb: Ndb,
    pool: RelayPool,
    subs: BTreeMap<SubId, SubSpec>,
}

impl SubMan {
    pub fn new(ndb: Ndb, pool: RelayPool) -> Self {
        SubMan {
            ndb,
            pool,
            subs: BTreeMap::new(),
        }
    }

    // deprecated, convert to SubMan directly
    pub fn pool(&mut self) -> &mut RelayPool {
        &mut self.pool
    }

    pub fn subscribe(&mut self, spec: SubSpec) -> SubResult<SubReceiver> {
        let receiver = self.make_subscription(&spec)?;
        self.subs.insert(receiver.lclid, spec);
        Ok(receiver)
    }

    pub fn unsubscribe(&mut self, rcvr: &SubReceiver) -> SubResult<()> {
        self.subs.remove(&rcvr.lclid);
        Ok(())
    }

    fn make_subscription(&mut self, sub: &SubSpec) -> SubResult<SubReceiver> {
        let subscription = self.ndb.subscribe(&sub.filters)?;
        let mut stream = subscription.stream(&self.ndb).notes_per_await(1);
        Ok(SubReceiver::new(
            self.ndb.clone(),
            subscription.into(),
            sub.rmtid.clone(),
            stream,
        ))
    }
}

pub struct SubReceiver {
    ndb: Ndb,      // if the streams's ndb was accessible we could use that instead
    lclid: SubId,  // ndb id
    rmtid: String, // remote sub id
    stream: SubscriptionStream,
}

impl SubReceiver {
    pub fn new(ndb: Ndb, lclid: SubId, rmtid: String, stream: SubscriptionStream) -> Self {
        SubReceiver {
            ndb,
            lclid,
            rmtid,
            stream,
        }
    }

    pub fn rmtid(&self) -> &String {
        &self.rmtid
    }

    pub async fn next(&mut self) -> SubResult<Vec<nostrdb::NoteKey>> {
        self.stream.next().await.ok_or(SubError::StreamEnded)
    }

    pub fn poll(&mut self, max_notes: u32) -> Vec<nostrdb::NoteKey> {
        self.ndb.poll_for_notes(self.lclid.0, max_notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testdbs_path_async;
    use crate::util::test_util::{raw_msg, test_keypair, ManagedNdb};
    use nostrdb::Transaction;

    // test basic subscription functionality
    #[tokio::test]
    async fn test_subman_sub() -> Result<(), Box<dyn std::error::Error>> {
        // setup an ndb and subman to test
        let (mndb, mut ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::Local)
                .build(),
        )?;

        // nothing should be available yet
        assert_eq!(receiver.poll(1), vec![]);

        // process a test event that matches the subscription
        let keys1 = test_keypair(1);
        let kind = 1;
        let content = "abc";
        ndb.process_event(&raw_msg("subid", &keys1, kind, content))?;

        // receiver should now see the msg
        let nks = receiver.next().await?;
        assert_eq!(nks.len(), 1);
        let txn = Transaction::new(&ndb)?;
        let note = ndb.get_note_by_key(&txn, nks[0])?;
        assert_eq!(note.pubkey(), keys1.pubkey.bytes());
        assert_eq!(note.kind(), kind);
        assert_eq!(note.content(), content);

        // now nothing should be available again
        assert_eq!(receiver.poll(1), vec![]);

        subman.unsubscribe(&receiver)?;
        Ok(())
    }

    // ensure that the subscription works when it is waiting before the event
    #[tokio::test]
    async fn test_subman_sub_with_waiting_thread() -> Result<(), Box<dyn std::error::Error>> {
        // setup an ndb and subman to test
        let (mndb, mut ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::Local)
                .build(),
        )?;

        // spawn a task to wait for the next message
        let handle = tokio::spawn(async move {
            let nks = receiver.next().await.unwrap();
            assert_eq!(nks.len(), 1); // Ensure one message is received
            (receiver, nks) // return the receiver as well
        });

        // process a test event that matches the subscription
        let keys1 = test_keypair(1);
        let kind = 1;
        let content = "abc";
        ndb.process_event(&raw_msg("subid", &keys1, kind, content))?;

        // await the spawned task to ensure it completes
        let (mut receiver, nks) = handle.await?;

        // validate the received message
        let txn = Transaction::new(&ndb)?;
        let note = ndb.get_note_by_key(&txn, nks[0])?;
        assert_eq!(note.pubkey(), keys1.pubkey.bytes());
        assert_eq!(note.kind(), kind);
        assert_eq!(note.content(), content);

        // ensure no additional messages are available
        assert_eq!(receiver.poll(1), vec![]);

        subman.unsubscribe(&receiver)?;
        Ok(())
    }

    // test subscription poll and next interaction
    #[tokio::test]
    async fn test_subman_poll_and_next() -> Result<(), Box<dyn std::error::Error>> {
        // setup an ndb and subman to test
        let (mndb, mut ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::Local)
                .build(),
        )?;

        // nothing should be available yet
        assert_eq!(receiver.poll(1), vec![]);

        // process a test event that matches the subscription
        let keys1 = test_keypair(1);
        let kind = 1;
        let content = "abc";
        ndb.process_event(&raw_msg("subid", &keys1, kind, content))?;
        std::thread::sleep(std::time::Duration::from_millis(150));

        // now poll should consume the note
        assert_eq!(receiver.poll(1), vec![NoteKey::new(1)]);

        // nothing more available
        assert_eq!(receiver.poll(1), vec![]);

        // process a second event
        let content = "def";
        ndb.process_event(&raw_msg("subid", &keys1, kind, content))?;

        // now receiver should now see the second note
        assert_eq!(receiver.next().await?, vec![NoteKey::new(2)]);

        subman.unsubscribe(&receiver)?;
        Ok(())
    }
}
