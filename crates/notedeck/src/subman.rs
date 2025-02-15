use futures::{channel::mpsc, FutureExt, StreamExt};
use std::collections::BTreeMap;
use std::fmt;
use std::{cell::RefCell, cmp::Ordering, rc::Rc};
use thiserror::Error;
use tracing::{error, info, trace, warn};
use uuid::Uuid;

use enostr::{Filter, PoolRelay, RelayEvent, RelayMessage, RelayPool};
use nostrdb::{self, Ndb, Subscription, SubscriptionStream};

/// The Subscription Manager
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
///         .constraint(SubConstraint::OnlyLocal)
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
///
/// Supported Operational Modes:
///
/// | mode            | Constraints        | lcl | rmt | end mechanism         |
/// |-----------------+--------------------+-----+-----+-----------------------|
/// | normal          |                    | sub | sub | client-closes         |
/// | local           | OnlyLocal          | sub |     | client-closes         |
/// | normal one-shot | OneShot            | sub | sub | EOSE -> end-of-stream |
/// | local one-shot  | OneShot+OnlyLocal  | qry |     | query,  end-of-stream |
/// | "prefetch"      | OneShot+OnlyRemote |     | sub | EOSE -> end-of-stream |

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
pub struct LclId(nostrdb::Subscription);

impl From<Subscription> for LclId {
    fn from(subscription: Subscription) -> Self {
        LclId(subscription)
    }
}

impl Ord for LclId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.id().cmp(&other.0.id())
    }
}

impl PartialOrd for LclId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for LclId {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for LclId {}

// nostr remote sub id
pub type RmtId = String;

#[derive(Debug, Clone)]
pub enum SubConstraint {
    OneShot,                    // terminate subscription after initial query and EOSE
    OnlyLocal,                  // only query the local db, no remote subs
    OnlyRemote,                 // prefetch from remote, nothing returned
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
        let mut is_onlylocal = false;
        let mut is_onlyremote = false;

        for constraint in self.constraints {
            match constraint {
                SubConstraint::OneShot => is_oneshot = true,
                SubConstraint::OnlyLocal => is_onlylocal = true,
                SubConstraint::OnlyRemote => is_onlyremote = true,
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
            is_onlylocal,
            is_onlyremote,
        }
    }
}

#[derive(Clone)]
pub struct SubSpec {
    rmtid: String, // unused if is_onlylocal
    filters: Vec<Filter>,
    outbox_relays: Vec<String>,
    allowed_relays: Vec<String>,
    blocked_relays: Vec<String>,
    is_oneshot: bool,
    is_onlylocal: bool,
    is_onlyremote: bool,
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
            .field("is_onlylocal", &self.is_onlylocal)
            .field("is_onlyremote", &self.is_onlyremote)
            .finish()
    }
}

pub struct SubMan {
    ndb: Ndb,
    pool: RelayPool,
    lcl: BTreeMap<LclId, Rc<RefCell<SubSpec>>>,
    rmt: BTreeMap<RmtId, Rc<RefCell<SubSpec>>>,
}

impl SubMan {
    pub fn new(ndb: Ndb, pool: RelayPool) -> Self {
        SubMan {
            ndb,
            pool,
            lcl: BTreeMap::new(),
            rmt: BTreeMap::new(),
        }
    }

    // deprecated, use SubMan directly instead
    pub fn pool(&mut self) -> &mut RelayPool {
        &mut self.pool
    }

    pub fn subscribe(&mut self, spec: SubSpec) -> SubResult<SubReceiver> {
        let (_maybe_tx_eose, rcvr) = self.make_subscription(&spec)?;
        let shared_spec = Rc::new(RefCell::new(spec));

        if let Some(lclid) = rcvr.lclid() {
            self.lcl.insert(lclid, Rc::clone(&shared_spec));
        }
        if let Some(rmtid) = rcvr.rmtid() {
            self.rmt.insert(rmtid, Rc::clone(&shared_spec));
        }
        Ok(rcvr)
    }

    pub fn unsubscribe(&mut self, rcvr: &SubReceiver) -> SubResult<()> {
        if let Some(lclid) = rcvr.lclid() {
            self.lcl.remove(&lclid);
        }
        if let Some(rmtid) = rcvr.rmtid() {
            self.rmt.remove(&rmtid);
        }
        Ok(())
    }

    fn make_subscription(
        &mut self,
        sub: &SubSpec,
    ) -> SubResult<(Option<mpsc::Sender<()>>, SubReceiver)> {
        // Setup local ndb subscription state
        let lclsub = if sub.is_onlyremote {
            None
        } else {
            let subscription = self.ndb.subscribe(&sub.filters)?;
            let lclstrm = subscription.stream(&self.ndb).notes_per_await(1);
            Some(LclSub {
                ndb: self.ndb.clone(),
                lclid: subscription.into(),
                lclstrm,
            })
        };

        // Setup remote nostr relay subscription state
        let (maybe_tx_eose, rmtsub) = if sub.is_onlylocal {
            (None, None)
        } else {
            let (tx_eose, rx_eose) = mpsc::channel(1);
            (
                Some(tx_eose),
                Some(RmtSub {
                    rmtid: sub.rmtid.clone(),
                    rmteose: rx_eose,
                }),
            )
        };

        Ok((maybe_tx_eose, SubReceiver { lclsub, rmtsub }))
    }

    pub fn process_relays<H: LegacyRelayHandler>(
        &mut self,
        legacy_relay_handler: &mut H,
    ) -> SubResult<()> {
        let wakeup = move || {
            // ignore
        };
        self.pool.keepalive_ping(wakeup);

        // NOTE: we don't use the while let loop due to borrow issues
        #[allow(clippy::while_let_loop)]
        loop {
            let ev = if let Some(ev) = self.pool.try_recv() {
                ev.into_owned()
            } else {
                break;
            };

            match (&ev.event).into() {
                RelayEvent::Opened => {
                    legacy_relay_handler.handle_opened(&ev.relay);
                }
                // TODO: handle reconnects
                RelayEvent::Closed => warn!("{} connection closed", &ev.relay),
                RelayEvent::Error(e) => error!("{}: {}", &ev.relay, e),
                RelayEvent::Other(msg) => trace!("other event {:?}", &msg),
                RelayEvent::Message(msg) => {
                    self.process_message(legacy_relay_handler, &ev.relay, &msg);
                }
            }
        }
        Ok(())
    }

    pub fn process_message<H: LegacyRelayHandler>(
        &mut self,
        legacy_relay_handler: &mut H,
        relay: &str,
        msg: &RelayMessage,
    ) {
        match msg {
            RelayMessage::Event(_subid, ev) => {
                let relay = if let Some(relay) = self.pool.relays.iter().find(|r| r.url() == relay)
                {
                    relay
                } else {
                    error!("couldn't find relay {} for note processing!?", relay);
                    return;
                };

                match relay {
                    PoolRelay::Websocket(_) => {
                        //info!("processing event {}", event);
                        if let Err(err) = self.ndb.process_event(ev) {
                            error!("error processing event {ev}: {err}");
                        }
                    }
                    PoolRelay::Multicast(_) => {
                        // multicast events are client events
                        if let Err(err) = self.ndb.process_client_event(ev) {
                            error!("error processing multicast event {ev}: {err}");
                        }
                    }
                }
            }
            RelayMessage::Notice(msg) => warn!("Notice from {}: {}", relay, msg),
            RelayMessage::OK(cr) => info!("OK {:?}", cr),
            RelayMessage::Eose(sid) => {
                legacy_relay_handler.handle_eose(sid, relay);
            }
        }
    }
}

pub trait LegacyRelayHandler {
    fn handle_opened(&mut self, relay: &str);
    fn handle_eose(&mut self, sid: &str, relay: &str);
}

struct LclSub {
    ndb: Ndb,
    lclid: LclId, // ndb id
    lclstrm: SubscriptionStream,
}

#[allow(unused)]
struct RmtSub {
    rmtid: RmtId,                // remote nostr sub id
    rmteose: mpsc::Receiver<()>, // all EOSE seen
}

pub struct SubReceiver {
    lclsub: Option<LclSub>,
    rmtsub: Option<RmtSub>,
}

impl SubReceiver {
    pub fn idstr(&self) -> String {
        let mut idstr = "".to_string();
        if let Some(lsub) = &self.lclsub {
            idstr.push_str(&format!("lcl:{:?}", lsub.lclid));
        }
        if let Some(rsub) = &self.rmtsub {
            if !idstr.is_empty() {
                idstr.push_str(", ");
            }
            idstr.push_str(&format!("rmt:{}", rsub.rmtid));
        }
        if idstr.is_empty() {
            "query".to_string()
        } else {
            idstr
        }
    }

    pub fn lclid(&self) -> Option<LclId> {
        self.lclsub.as_ref().map(|lsub| lsub.lclid)
    }

    pub fn rmtid(&self) -> Option<RmtId> {
        self.rmtsub.as_ref().map(|rsub| rsub.rmtid.clone())
    }

    pub async fn next(&mut self) -> SubResult<Vec<nostrdb::NoteKey>> {
        if let (Some(lsub), Some(rsub)) = (&mut self.lclsub, &mut self.rmtsub) {
            // local and remote subs
            futures::select! {
                notes = lsub.lclstrm.next().fuse() => {
                    match notes {
                        Some(notes) => Ok(notes),
                        None => Err(SubError::StreamEnded),
                    }
                },
                _ = rsub.rmteose.next().fuse() => {
                    Err(SubError::StreamEnded)
                }
            }
        } else if let Some(lsub) = &mut self.lclsub {
            // only local sub
            lsub.lclstrm.next().await.ok_or(SubError::StreamEnded)
        } else if let Some(rsub) = &mut self.rmtsub {
            // only remote sub (prefetch only, values not returned)
            match rsub.rmteose.next().await {
                Some(_) => Err(SubError::StreamEnded),
                None => Err(SubError::InternalError(
                    "trouble reading from rmteose".to_string(),
                )),
            }
        } else {
            // query case
            Err(SubError::InternalError("unimplmented".to_string()))
        }
    }

    pub fn poll(&mut self, max_notes: u32) -> Vec<nostrdb::NoteKey> {
        assert!(self.lclsub.is_some()); // FIXME - local only
        let lclsub = self.lclsub.as_mut().unwrap();
        lclsub.ndb.poll_for_notes(lclsub.lclid.0, max_notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testdbs_path_async;
    use crate::util::test_util::{raw_msg, test_keypair, ManagedNdb};
    use nostrdb::{NoteKey, Transaction};

    // test basic subscription functionality
    #[tokio::test]
    async fn test_subman_sub() -> Result<(), Box<dyn std::error::Error>> {
        // setup an ndb and subman to test
        let (_mndb, ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::OnlyLocal)
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
        let (_mndb, ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::OnlyLocal)
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
        let (_mndb, ndb) = ManagedNdb::setup(&testdbs_path_async!());
        let mut subman = SubMan::new(ndb.clone(), RelayPool::new());

        // subscribe to some stuff
        let mut receiver = subman.subscribe(
            SubSpecBuilder::new()
                .filters(vec![Filter::new().kinds(vec![1]).build()])
                .constraint(SubConstraint::OnlyLocal)
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
