pub use discv5::{
    enr, enr::CombinedKey, service::Service, Config as Discv5Config,
    ConfigBuilder as Discv5ConfigBuilder, Discv5, Enr, Event,
};

use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use parking_lot::{Mutex, MutexGuard};
use secp256k1::SecretKey;
use std::{
    default::Default,
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream};

// Wrapper struct for Discv5
pub struct Discv5Handle {
    inner: Arc<Mutex<Discv5>>,
}

impl Discv5Handle {
    // Constructor to create a new Discv5Handle
    pub fn new(discv5: Discv5) -> Self {
        Discv5Handle { inner: Arc::new(Mutex::new(discv5)) }
    }

    pub fn from_secret_key(
        secret_key: SecretKey,
        discv5_config: Discv5Config,
    ) -> Result<Self, Discv5Error> {
        let secret_key_bytes = secret_key.as_ref();
        let signing_key = SigningKey::from_slice(secret_key_bytes)
            .map_err(|_e| Discv5Error::SecretKeyDecode.into())?;
        let enr_key = CombinedKey::Secp256k1(signing_key);
        let enr = enr::EnrBuilder::new("v4")
            .build(&enr_key)
            .map_err(|_e| Discv5Error::EnrBuilderConstruct.into())?;
        Ok(Discv5Handle::new(
            Discv5::new(enr, enr_key, discv5_config)
                .map_err(|_e| Discv5Error::Discv5Construct.into())?,
        ))
    }

    pub fn lock(&self) -> MutexGuard<'_, Discv5> {
        self.inner.lock()
    }

    pub fn convert_to_discv5(&self) -> Arc<Mutex<Discv5>> {
        self.inner.clone()
    }

    // Start the Discv5 service
    pub async fn start_service(&self) -> Result<(), Discv5Error> {
        let mut discv5 = self.inner.lock();
        discv5.start().await.map_err(|e| Discv5Error::Discv5Construct.into())
    }

    // Create the event stream
    pub async fn create_event_stream(
        &self,
    ) -> Result<tokio::sync::mpsc::Receiver<Event>, Discv5Error> {
        let discv5 = self.inner.lock();
        discv5.event_stream().await.map_err(|e| Discv5Error::Discv5EventStreamStart.into())
    }

    // Method to get a mutable reference to Discv5 from Discv5Handle
    pub async fn get_mut(&self) -> MutexGuard<'_, Discv5> {
        self.inner.lock()
    }
}

impl fmt::Debug for Discv5Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Discv5Handle(<Discv5>)")
    }
}

/// The default table filter that results in all nodes being accepted into the local routing table.
const fn allow_all_enrs(_enr: &Enr) -> bool {
    true
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Discv5Error {
    /// Failed to decode a key from a table.
    #[error("failed to parse secret key to Signing Key")]
    SecretKeyDecode,
    /// Failed to construct a new EnrBuilder
    #[error("failed to constuct a new EnrBuilder")]
    EnrBuilderConstruct,
    /// Failed to construct Discv5 instance
    #[error("failed to construct a new Discv5 instance")]
    Discv5Construct,
    /// Failed to create a event stream
    #[error("failed to create event stream")]
    Discv5EventStream,
    /// Failed to start Discv5 event stream
    #[error("failed to start event stream")]
    Discv5EventStreamStart,
}

pub fn default_discv5_config() -> Discv5Config {
    Discv5Config {
        enable_packet_filter: Default::default(),
        request_timeout: Default::default(),
        vote_duration: Default::default(),
        query_peer_timeout: Default::default(),
        query_timeout: Default::default(),
        request_retries: Default::default(),
        session_timeout: Default::default(),
        session_cache_capacity: Default::default(),
        enr_update: Default::default(),
        max_nodes_response: Default::default(),
        enr_peer_update_min: Default::default(),
        query_parallelism: Default::default(),
        ip_limit: Default::default(),
        incoming_bucket_limit: Default::default(),
        table_filter: allow_all_enrs,
        ping_interval: Default::default(),
        report_discovered_peers: Default::default(),
        filter_rate_limiter: Default::default(),
        filter_max_nodes_per_ip: Default::default(),
        filter_max_bans_per_ip: Default::default(),
        permit_ban_list: Default::default(),
        ban_duration: Default::default(),
        executor: Default::default(),
        listen_config: Default::default(),
    }
}
pub struct Discv5Service {
    inner: ReceiverStream<Event>,
}

impl Discv5Service {
    // A constructor to create a new Discv5Service
    pub fn new(event_receiver: mpsc::Receiver<Event>) -> Self {
        Discv5Service { inner: ReceiverStream::new(event_receiver) }
    }
}

impl Stream for Discv5Service {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let receiver = self.get_mut().inner.poll_next_unpin(cx);
        receiver
    }
}
