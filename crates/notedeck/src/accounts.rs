use tracing::{debug, error, info};

use crate::subman::{RemoteId, SubSpecBuilder};
use crate::{
    KeyStorageResponse, KeyStorageType, MuteFun, Muted, RelaySpec, SingleUnkIdAction, SubError,
    SubMan, UnknownIds, UserAccount,
};
use enostr::{FilledKeypair, Keypair, RelayPool};
use nostrdb::{Filter, Ndb, Note, NoteBuilder, NoteKey, Transaction};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

// TODO: remove this
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct SwitchAccountAction {
    /// Some index representing the source of the action
    pub source: Option<usize>,

    /// The account index to switch to
    pub switch_to: usize,
}

impl SwitchAccountAction {
    pub fn new(source: Option<usize>, switch_to: usize) -> Self {
        SwitchAccountAction { source, switch_to }
    }
}

#[derive(Debug)]
pub enum AccountsAction {
    Switch(SwitchAccountAction),
    Remove(usize),
}

pub struct AccountRelayData {
    filter: Filter,
    sub_remote_id: Option<RemoteId>,
    _local: BTreeSet<RelaySpec>, // used locally but not advertised
    advertised: Arc<Mutex<BTreeSet<RelaySpec>>>, // advertised via NIP-65
}

#[derive(Default)]
pub struct ContainsAccount {
    pub has_nsec: bool,
    pub index: usize,
}

#[must_use = "You must call process_login_action on this to handle unknown ids"]
pub struct AddAccountAction {
    pub accounts_action: Option<AccountsAction>,
    pub unk_id_action: SingleUnkIdAction,
}

impl AccountRelayData {
    pub fn new(ndb: &Ndb, pubkey: &[u8; 32]) -> Self {
        // Construct a filter for the user's NIP-65 relay list
        let filter = Filter::new()
            .authors([pubkey])
            .kinds([10002])
            .limit(1)
            .build();

        // Query the ndb immediately to see if the user list is already there
        let txn = Transaction::new(ndb).expect("transaction");
        let lim = filter.limit().unwrap_or(crate::filter::default_limit()) as i32;
        let nks = ndb
            .query(&txn, &[filter.clone()], lim)
            .expect("query user relays results")
            .iter()
            .map(|qr| qr.note_key)
            .collect::<Vec<NoteKey>>();
        let relays = Self::harvest_nip65_relays(ndb, &txn, &nks);
        debug!(
            "pubkey {}: initial relays {:?}",
            hex::encode(pubkey),
            relays
        );

        AccountRelayData {
            filter,
            sub_remote_id: None,
            _local: BTreeSet::new(),
            advertised: Arc::new(Mutex::new(relays.into_iter().collect())),
        }
    }

    // make this account the current selected account
    pub fn activate(&mut self, subman: &mut SubMan, default_relays: &[RelaySpec]) {
        debug!("activating relay sub {}", self.filter.json().unwrap());
        assert!(self.sub_remote_id.is_none(), "subscription already exists");
        let ndb = subman.ndb();
        let subspec = SubSpecBuilder::new()
            .filters(vec![self.filter.clone()])
            .build();
        debug!(
            "activating account relay sub {}: {}",
            subspec.remote_id,
            self.filter.json().unwrap()
        );
        if let Ok(mut rcvr) = subman.subscribe(subspec, default_relays) {
            let idstr = rcvr.idstr();
            self.sub_remote_id = rcvr.remote_id();
            let advertisedref = self.advertised.clone();
            tokio::spawn(async move {
                loop {
                    match rcvr.next().await {
                        Err(SubError::StreamEnded) => {
                            debug!("account relays: sub {} complete", idstr);
                            break;
                        }
                        Err(err) => {
                            error!("account relays: sub {}: error: {:?}", idstr, err);
                            break;
                        }
                        Ok(nks) => {
                            debug!("account relays: sub {}: note keys: {:?}", idstr, nks);
                            let txn = Transaction::new(&ndb).expect("txn");
                            let relays = Self::harvest_nip65_relays(&ndb, &txn, &nks);
                            debug!("updated relays {:?}", relays);
                            *advertisedref.lock().unwrap() = relays.into_iter().collect();
                        }
                    }
                }
            });
        }
    }

    // this account is no longer the selected account
    pub fn deactivate(&mut self, subman: &mut SubMan) {
        assert!(self.sub_remote_id.is_some(), "subscription doesn't exist");
        let remote_id = self.sub_remote_id.as_ref().unwrap();
        debug!(
            "deactivating account relays sub {}: {}",
            remote_id,
            self.filter.json().unwrap()
        );
        subman.unsubscribe_remote_id(remote_id).ok();
        self.sub_remote_id = None;
    }

    // standardize the format (ie, trailing slashes) to avoid dups
    pub fn canonicalize_url(url: &str) -> String {
        match Url::parse(url) {
            Ok(parsed_url) => parsed_url.to_string(),
            Err(_) => url.to_owned(), // If parsing fails, return the original URL.
        }
    }

    fn harvest_nip65_relays(ndb: &Ndb, txn: &Transaction, nks: &[NoteKey]) -> Vec<RelaySpec> {
        let mut relays = Vec::new();
        for nk in nks.iter() {
            if let Ok(note) = ndb.get_note_by_key(txn, *nk) {
                for tag in note.tags() {
                    match tag.get(0).and_then(|t| t.variant().str()) {
                        Some("r") => {
                            if let Some(url) = tag.get(1).and_then(|f| f.variant().str()) {
                                let has_read_marker = tag
                                    .get(2)
                                    .is_some_and(|m| m.variant().str() == Some("read"));
                                let has_write_marker = tag
                                    .get(2)
                                    .is_some_and(|m| m.variant().str() == Some("write"));
                                relays.push(RelaySpec::new(
                                    Self::canonicalize_url(url),
                                    has_read_marker,
                                    has_write_marker,
                                ));
                            }
                        }
                        Some("alt") => {
                            // ignore for now
                        }
                        Some(x) => {
                            error!("harvest_nip65_relays: unexpected tag type: {}", x);
                        }
                        None => {
                            error!("harvest_nip65_relays: invalid tag");
                        }
                    }
                }
            }
        }
        relays
    }

    pub fn publish_nip65_relays(&self, seckey: &[u8; 32], pool: &mut RelayPool) {
        let mut builder = NoteBuilder::new().kind(10002).content("");
        for rs in self.advertised.lock().unwrap().iter() {
            builder = builder.start_tag().tag_str("r").tag_str(&rs.url);
            if rs.has_read_marker {
                builder = builder.tag_str("read");
            } else if rs.has_write_marker {
                builder = builder.tag_str("write");
            }
        }
        let note = builder.sign(seckey).build().expect("note build");
        pool.send(&enostr::ClientMessage::event(note).expect("note client message"));
    }
}

pub struct AccountMutedData {
    filter: Filter,
    sub_remote_id: Option<RemoteId>,
    muted: Arc<Mutex<Muted>>,
}

impl AccountMutedData {
    pub fn new(ndb: &Ndb, pubkey: &[u8; 32]) -> Self {
        // Construct a filter for the user's NIP-51 muted list
        let filter = Filter::new()
            .authors([pubkey])
            .kinds([10000])
            .limit(1)
            .build();

        // Query the ndb immediately to see if the user's muted list is already there
        let txn = Transaction::new(ndb).expect("transaction");
        let lim = filter.limit().unwrap_or(crate::filter::default_limit()) as i32;
        let nks = ndb
            .query(&txn, &[filter.clone()], lim)
            .expect("query user muted results")
            .iter()
            .map(|qr| qr.note_key)
            .collect::<Vec<NoteKey>>();
        let muted = Self::harvest_nip51_muted(ndb, &txn, &nks);
        debug!("pubkey {}: initial muted {:?}", hex::encode(pubkey), muted);

        AccountMutedData {
            filter,
            sub_remote_id: None,
            muted: Arc::new(Mutex::new(muted)),
        }
    }

    // make this account the current selected account
    pub fn activate(&mut self, subman: &mut SubMan, default_relays: &[RelaySpec]) {
        assert!(self.sub_remote_id.is_none(), "subscription already exists");
        let ndb = subman.ndb();
        let subspec = SubSpecBuilder::new()
            .filters(vec![self.filter.clone()])
            .build();
        debug!(
            "activating account muted sub {}: {}",
            subspec.remote_id,
            self.filter.json().unwrap()
        );
        if let Ok(mut rcvr) = subman.subscribe(subspec, default_relays) {
            let idstr = rcvr.idstr();
            self.sub_remote_id = rcvr.remote_id();
            let mutedref = self.muted.clone();
            tokio::spawn(async move {
                loop {
                    match rcvr.next().await {
                        Err(SubError::StreamEnded) => {
                            debug!("account muted: sub {} complete", idstr);
                            break;
                        }
                        Err(err) => {
                            error!("account muted: sub {}: error: {:?}", idstr, err);
                            break;
                        }
                        Ok(nks) => {
                            debug!("account muted: sub {}: note keys: {:?}", idstr, nks);
                            let txn = Transaction::new(&ndb).expect("txn");
                            let muted = AccountMutedData::harvest_nip51_muted(&ndb, &txn, &nks);
                            debug!("updated muted {:?}", muted);
                            *mutedref.lock().unwrap() = muted;
                        }
                    }
                }
            });
        }
    }

    // this account is no longer the selected account
    pub fn deactivate(&mut self, subman: &mut SubMan) {
        assert!(self.sub_remote_id.is_some(), "subscription doesn't exist");
        let remote_id = self.sub_remote_id.as_ref().unwrap();
        debug!(
            "deactivating account muted sub {}: {}",
            remote_id,
            self.filter.json().unwrap()
        );

        subman.unsubscribe_remote_id(remote_id).ok();
        self.sub_remote_id = None;
    }

    fn harvest_nip51_muted(ndb: &Ndb, txn: &Transaction, nks: &[NoteKey]) -> Muted {
        let mut muted = Muted::default();
        for nk in nks.iter() {
            if let Ok(note) = ndb.get_note_by_key(txn, *nk) {
                for tag in note.tags() {
                    match tag.get(0).and_then(|t| t.variant().str()) {
                        Some("p") => {
                            if let Some(id) = tag.get(1).and_then(|f| f.variant().id()) {
                                muted.pubkeys.insert(*id);
                            }
                        }
                        Some("t") => {
                            if let Some(str) = tag.get(1).and_then(|f| f.variant().str()) {
                                muted.hashtags.insert(str.to_string());
                            }
                        }
                        Some("word") => {
                            if let Some(str) = tag.get(1).and_then(|f| f.variant().str()) {
                                muted.words.insert(str.to_string());
                            }
                        }
                        Some("e") => {
                            if let Some(id) = tag.get(1).and_then(|f| f.variant().id()) {
                                muted.threads.insert(*id);
                            }
                        }
                        Some("alt") => {
                            // maybe we can ignore these?
                        }
                        Some(x) => error!("query_nip51_muted: unexpected tag: {}", x),
                        None => error!(
                            "query_nip51_muted: bad tag value: {:?}",
                            tag.get_unchecked(0).variant()
                        ),
                    }
                }
            }
        }
        muted
    }
}

pub struct AccountData {
    relay: AccountRelayData,
    muted: AccountMutedData,
}

/// The interface for managing the user's accounts.
/// Represents all user-facing operations related to account management.
pub struct Accounts {
    currently_selected_account: Option<usize>,
    accounts: Vec<UserAccount>,
    key_store: KeyStorageType,
    account_data: BTreeMap<[u8; 32], AccountData>,
    _forced_relays: BTreeSet<RelaySpec>,
    bootstrap_relays: BTreeSet<RelaySpec>,
    needs_relay_config: bool,
}

impl Accounts {
    pub fn new(key_store: KeyStorageType, _forced_relays: Vec<String>) -> Self {
        let accounts = if let KeyStorageResponse::ReceivedResult(res) = key_store.get_keys() {
            res.unwrap_or_default()
        } else {
            Vec::new()
        };

        let currently_selected_account = get_selected_index(&accounts, &key_store);
        let account_data = BTreeMap::new();
        let _forced_relays: BTreeSet<RelaySpec> = _forced_relays
            .into_iter()
            .map(|u| RelaySpec::new(AccountRelayData::canonicalize_url(&u), false, false))
            .collect();
        let bootstrap_relays = [
            "wss://relay.damus.io",
            // "wss://pyramid.fiatjaf.com",  // Uncomment if needed
            "wss://nos.lol",
            "wss://nostr.wine",
            "wss://purplepag.es",
        ]
        .iter()
        .map(|&url| url.to_string())
        .map(|u| RelaySpec::new(AccountRelayData::canonicalize_url(&u), false, false))
        .collect();

        Accounts {
            currently_selected_account,
            accounts,
            key_store,
            account_data,
            _forced_relays,
            bootstrap_relays,
            needs_relay_config: true,
        }
    }

    pub fn get_accounts(&self) -> &Vec<UserAccount> {
        &self.accounts
    }

    pub fn get_account(&self, ind: usize) -> Option<&UserAccount> {
        self.accounts.get(ind)
    }

    pub fn find_account(&self, pk: &[u8; 32]) -> Option<&UserAccount> {
        self.accounts.iter().find(|acc| acc.pubkey.bytes() == pk)
    }

    pub fn remove_account(&mut self, index: usize) {
        if let Some(account) = self.accounts.get(index) {
            let _ = self.key_store.remove_key(account);
            self.accounts.remove(index);

            if let Some(selected_index) = self.currently_selected_account {
                match selected_index.cmp(&index) {
                    Ordering::Greater => {
                        self.select_account(selected_index - 1);
                    }
                    Ordering::Equal => {
                        if self.accounts.is_empty() {
                            // If no accounts remain, clear the selection
                            self.clear_selected_account();
                        } else if index >= self.accounts.len() {
                            // If the removed account was the last one, select the new last account
                            self.select_account(self.accounts.len() - 1);
                        } else {
                            // Otherwise, select the account at the same position
                            self.select_account(index);
                        }
                    }
                    Ordering::Less => {}
                }
            }
        }
    }

    pub fn needs_relay_config(&mut self) {
        self.needs_relay_config = true;
    }

    fn contains_account(&self, pubkey: &[u8; 32]) -> Option<ContainsAccount> {
        for (index, account) in self.accounts.iter().enumerate() {
            let has_pubkey = account.pubkey.bytes() == pubkey;
            let has_nsec = account.secret_key.is_some();
            if has_pubkey {
                return Some(ContainsAccount { has_nsec, index });
            }
        }

        None
    }

    pub fn contains_full_kp(&self, pubkey: &enostr::Pubkey) -> bool {
        if let Some(contains) = self.contains_account(pubkey.bytes()) {
            contains.has_nsec
        } else {
            false
        }
    }

    #[must_use = "UnknownIdAction's must be handled. Use .process_unknown_id_action()"]
    pub fn add_account(&mut self, account: Keypair) -> AddAccountAction {
        let pubkey = account.pubkey;
        let switch_to_index = if let Some(contains_acc) = self.contains_account(pubkey.bytes()) {
            if account.secret_key.is_some() && !contains_acc.has_nsec {
                info!(
                    "user provided nsec, but we already have npub {}. Upgrading to nsec",
                    pubkey
                );
                let _ = self.key_store.add_key(&account);

                self.accounts[contains_acc.index] = account;
            } else {
                info!("already have account, not adding {}", pubkey);
            }
            contains_acc.index
        } else {
            info!("adding new account {}", pubkey);
            let _ = self.key_store.add_key(&account);
            self.accounts.push(account);
            self.accounts.len() - 1
        };

        let source: Option<usize> = None;
        AddAccountAction {
            accounts_action: Some(AccountsAction::Switch(SwitchAccountAction::new(
                source,
                switch_to_index,
            ))),
            unk_id_action: SingleUnkIdAction::pubkey(pubkey),
        }
    }

    pub fn num_accounts(&self) -> usize {
        self.accounts.len()
    }

    pub fn get_selected_account_index(&self) -> Option<usize> {
        self.currently_selected_account
    }

    pub fn selected_or_first_nsec(&self) -> Option<FilledKeypair<'_>> {
        self.get_selected_account()
            .and_then(|kp| kp.to_full())
            .or_else(|| self.accounts.iter().find_map(|a| a.to_full()))
    }

    /// Get the selected account's pubkey as bytes. Common operation so
    /// we make it a helper here.
    pub fn selected_account_pubkey_bytes(&self) -> Option<&[u8; 32]> {
        self.get_selected_account().map(|kp| kp.pubkey.bytes())
    }

    pub fn get_selected_account(&self) -> Option<&UserAccount> {
        if let Some(account_index) = self.currently_selected_account {
            if let Some(account) = self.get_account(account_index) {
                Some(account)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn get_selected_account_data(&self) -> Option<&AccountData> {
        let account_pubkey = {
            let account = self.get_selected_account()?;
            *account.pubkey.bytes()
        };
        self.account_data.get(&account_pubkey)
    }

    pub fn get_selected_account_data_mut(&mut self) -> Option<&mut AccountData> {
        let account_pubkey = {
            let account = self.get_selected_account()?;
            *account.pubkey.bytes()
        };
        self.account_data.get_mut(&account_pubkey)
    }

    pub fn select_account(&mut self, index: usize) {
        if let Some(account) = self.accounts.get(index) {
            self.currently_selected_account = Some(index);
            self.key_store.select_key(Some(account.pubkey));
        }
    }

    pub fn clear_selected_account(&mut self) {
        self.currently_selected_account = None;
        self.key_store.select_key(None);
    }

    pub fn mutefun(&self) -> Box<MuteFun> {
        if let Some(index) = self.currently_selected_account {
            if let Some(account) = self.accounts.get(index) {
                let pubkey = account.pubkey.bytes();
                if let Some(account_data) = self.account_data.get(pubkey) {
                    let muted = account_data.muted.muted.lock().unwrap().clone();
                    return Box::new(move |note: &Note, thread: &[u8; 32]| {
                        muted.is_muted(note, thread)
                    });
                }
            }
        }
        Box::new(|_: &Note, _: &[u8; 32]| false)
    }

    // Return accounts which have no account_data yet (added) and accounts
    // which have still data but are no longer in our account list (removed).
    fn delta_accounts(&self) -> (Vec<[u8; 32]>, Vec<[u8; 32]>) {
        let mut added = Vec::new();
        for pubkey in self.accounts.iter().map(|a| a.pubkey.bytes()) {
            if !self.account_data.contains_key(pubkey) {
                added.push(*pubkey);
            }
        }
        let mut removed = Vec::new();
        for pubkey in self.account_data.keys() {
            if self.contains_account(pubkey).is_none() {
                removed.push(*pubkey);
            }
        }
        (added, removed)
    }

    fn handle_added_account(&mut self, ndb: &Ndb, pubkey: &[u8; 32]) {
        debug!("handle_added_account {}", hex::encode(pubkey));

        // Create the user account data
        let new_account_data = AccountData {
            relay: AccountRelayData::new(ndb, pubkey),
            muted: AccountMutedData::new(ndb, pubkey),
        };
        self.account_data.insert(*pubkey, new_account_data);
    }

    fn handle_removed_account(&mut self, pubkey: &[u8; 32]) {
        debug!("handle_removed_account {}", hex::encode(pubkey));
        // FIXME - we need to unsubscribe here
        self.account_data.remove(pubkey);
    }

    fn get_combined_relays(
        &self,
        data_option: Option<&AccountData>,
        filter: impl Fn(&RelaySpec) -> bool,
    ) -> Vec<RelaySpec> {
        let mut relays = if let Some(data) = data_option {
            data.relay
                .advertised
                .lock()
                .unwrap()
                .iter()
                .filter(|&x| filter(x))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        if relays.is_empty() {
            relays.extend(self.bootstrap_relays.iter().filter(|&x| filter(x)).cloned());
        }

        relays
    }

    pub fn get_selected_account_readable_relays(&self) -> Vec<RelaySpec> {
        self.get_combined_relays(self.get_selected_account_data(), |relay| {
            relay.is_readable()
        })
    }

    pub fn get_selected_account_writable_relays(&self) -> Vec<RelaySpec> {
        self.get_combined_relays(self.get_selected_account_data(), |relay| {
            relay.is_writable()
        })
    }

    pub fn get_all_selected_account_relays(&self) -> Vec<RelaySpec> {
        self.get_combined_relays(self.get_selected_account_data(), |_| true)
    }

    pub fn update(&mut self, subman: &mut SubMan, _ctx: &egui::Context) {
        // IMPORTANT - This function is called in the UI update loop,
        // make sure it is fast when idle

        // Do we need to deactivate any existing account subs?
        for (ndx, account) in self.accounts.iter().enumerate() {
            if Some(ndx) != self.currently_selected_account {
                // this account is not currently selected
                if let Some(data) = self.account_data.get_mut(account.pubkey.bytes()) {
                    if data.relay.sub_remote_id.is_some() {
                        // this account has relay subs, deactivate them
                        data.relay.deactivate(subman);
                    }
                    if data.muted.sub_remote_id.is_some() {
                        // this account has muted subs, deactivate them
                        data.muted.deactivate(subman);
                    }
                }
            }
        }

        let ndb = subman.ndb().clone();

        // Were any accounts added or removed?
        let (added, removed) = self.delta_accounts();
        for pk in added {
            self.handle_added_account(&ndb, &pk);
        }
        for pk in removed {
            self.handle_removed_account(&pk);
        }

        // Do we need to activate account subs?
        let default_relays = self.get_all_selected_account_relays();
        if let Some(data) = self.get_selected_account_data_mut() {
            if data.relay.sub_remote_id.is_none() {
                // the currently selected account doesn't have relay subs, activate them
                data.relay.activate(subman, &default_relays);
            }
            if data.muted.sub_remote_id.is_none() {
                // the currently selected account doesn't have muted subs, activate them
                data.muted.activate(subman, &default_relays);
            }
        }
    }

    pub fn get_full<'a>(&'a self, pubkey: &[u8; 32]) -> Option<FilledKeypair<'a>> {
        if let Some(contains) = self.contains_account(pubkey) {
            if contains.has_nsec {
                if let Some(kp) = self.get_account(contains.index) {
                    return kp.to_full();
                }
            }
        }

        None
    }

    fn modify_advertised_relays(
        &mut self,
        relay_url: &str,
        pool: &mut RelayPool,
        action: RelayAction,
    ) {
        let relay_url = AccountRelayData::canonicalize_url(relay_url);
        match action {
            RelayAction::Add => info!("add advertised relay \"{}\"", relay_url),
            RelayAction::Remove => info!("remove advertised relay \"{}\"", relay_url),
        }
        match self.currently_selected_account {
            None => error!("no account is currently selected."),
            Some(index) => match self.accounts.get(index) {
                None => error!("selected account index {} is out of range.", index),
                Some(keypair) => {
                    let key_bytes: [u8; 32] = *keypair.pubkey.bytes();
                    match self.account_data.get_mut(&key_bytes) {
                        None => error!("no account data found for the provided key."),
                        Some(account_data) => {
                            let advertised = &mut account_data.relay.advertised.lock().unwrap();
                            if advertised.is_empty() {
                                // If the selected account has no advertised relays,
                                // initialize with the bootstrapping set.
                                advertised.extend(self.bootstrap_relays.iter().cloned());
                            }
                            match action {
                                RelayAction::Add => {
                                    advertised.insert(RelaySpec::new(relay_url, false, false));
                                }
                                RelayAction::Remove => {
                                    advertised.remove(&RelaySpec::new(relay_url, false, false));
                                }
                            }
                            self.needs_relay_config = true;

                            // If we have the secret key publish the NIP-65 relay list
                            if let Some(secretkey) = &keypair.secret_key {
                                account_data
                                    .relay
                                    .publish_nip65_relays(&secretkey.to_secret_bytes(), pool);
                            }
                        }
                    }
                }
            },
        }
    }

    pub fn add_advertised_relay(&mut self, relay_to_add: &str, pool: &mut RelayPool) {
        self.modify_advertised_relays(relay_to_add, pool, RelayAction::Add);
    }

    pub fn remove_advertised_relay(&mut self, relay_to_remove: &str, pool: &mut RelayPool) {
        self.modify_advertised_relays(relay_to_remove, pool, RelayAction::Remove);
    }
}

enum RelayAction {
    Add,
    Remove,
}

fn get_selected_index(accounts: &[UserAccount], keystore: &KeyStorageType) -> Option<usize> {
    match keystore.get_selected_key() {
        KeyStorageResponse::ReceivedResult(Ok(Some(pubkey))) => {
            return accounts.iter().position(|account| account.pubkey == pubkey);
        }

        KeyStorageResponse::ReceivedResult(Err(e)) => error!("Error getting selected key: {}", e),
        KeyStorageResponse::Waiting | KeyStorageResponse::ReceivedResult(Ok(None)) => {}
    };

    None
}

impl AddAccountAction {
    // Simple wrapper around processing the unknown action to expose too
    // much internal logic. This allows us to have a must_use on our
    // LoginAction type, otherwise the SingleUnkIdAction's must_use will
    // be lost when returned in the login action
    pub fn process_action(&mut self, ids: &mut UnknownIds, ndb: &Ndb, txn: &Transaction) {
        self.unk_id_action.process_action(ids, ndb, txn);
    }
}
