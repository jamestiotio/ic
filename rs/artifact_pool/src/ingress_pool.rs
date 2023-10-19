/// Ingress Pool provides storage for all ingress messages in artifact_pool
/// Logically it can be viewed as part of the artifact pool
/// But we keep it separated for code readability
use crate::{
    metrics::{PoolMetrics, POOL_TYPE_UNVALIDATED, POOL_TYPE_VALIDATED},
    HasTimestamp,
};
use ic_config::artifact_pool::ArtifactPoolConfig;
use ic_constants::MAX_INGRESS_TTL;
use ic_interfaces::{
    artifact_pool::{
        ChangeResult, MutablePool, PriorityFnAndFilterProducer, UnvalidatedArtifact,
        ValidatedPoolReader,
    },
    ingress_pool::{
        ChangeAction, ChangeSet, IngressPool, IngressPoolObject, IngressPoolSelect,
        IngressPoolThrottler, PoolSection, SelectResult, UnvalidatedIngressArtifact,
        ValidatedIngressArtifact,
    },
    time_source::TimeSource,
};
use ic_logger::{debug, trace, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_types::{
    artifact::{Advert, IngressMessageId, Priority, PriorityFn},
    artifact_kind::IngressArtifact,
    messages::{MessageId, SignedIngress, EXPECTED_MESSAGE_ID_LENGTH},
    CountBytes, Height, NodeId, Time,
};
use prometheus::IntCounter;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
struct IngressPoolSection<T: AsRef<IngressPoolObject>> {
    /// Do not insert or remove elements in this map directly. Use this struct's
    /// associated functions [`insert`], [`remove`] and [`purge_below`].
    artifacts: BTreeMap<IngressMessageId, T>,
    metrics: PoolMetrics,
    /// Note: The byte size is updated incrementally as a side-effect of insert, remove
    /// and purge invocations. Never modify the artifacts map directly! Use the
    /// associated functions [`insert`], [`remove`] and [`purge_below`]
    byte_size: usize,
}

impl<T: AsRef<IngressPoolObject>> CountBytes for IngressPoolSection<T> {
    fn count_bytes(&self) -> usize {
        self.byte_size
    }
}
impl<T: AsRef<IngressPoolObject>> IngressPoolSection<T> {
    fn new(metrics: PoolMetrics) -> IngressPoolSection<T> {
        IngressPoolSection {
            artifacts: BTreeMap::new(),
            metrics,
            byte_size: 0,
        }
    }

    fn insert(&mut self, message_id: IngressMessageId, artifact: T) {
        let _timer = self
            .metrics
            .op_duration
            .with_label_values(&["insert"])
            .start_timer();
        let new_artifact_size = artifact.as_ref().count_bytes();
        self.metrics.observe_insert(new_artifact_size);
        if let Some(previous) = self.artifacts.insert(message_id, artifact) {
            let prev_size = previous.as_ref().count_bytes();
            self.byte_size -= prev_size;
            self.byte_size += new_artifact_size;
            self.metrics.observe_duplicate(prev_size);
        } else {
            self.byte_size += new_artifact_size;
        }
        // SAFETY: Checking byte size invariant
        section_ok(self);
    }

    fn remove(&mut self, message_id: &IngressMessageId) -> Option<T> {
        let _timer = self
            .metrics
            .op_duration
            .with_label_values(&["remove"])
            .start_timer();
        let removed = self.artifacts.remove(message_id);
        if let Some(artifact) = &removed {
            self.byte_size -= artifact.as_ref().count_bytes();
            self.metrics.observe_remove(artifact.as_ref().count_bytes());
        }
        // SAFETY: Checking byte size invariant
        section_ok(self);
        removed
    }

    fn exists(&self, message_id: &IngressMessageId) -> bool {
        let _timer = self
            .metrics
            .op_duration
            .with_label_values(&["exists"])
            .start_timer();
        self.artifacts.contains_key(message_id)
    }

    // Purge below an expiry prefix (non-inclusive), and return the purged artifacts
    // as an iterator.
    fn purge_below(&mut self, expiry: Time) -> Box<dyn Iterator<Item = T> + '_> {
        let _timer = self
            .metrics
            .op_duration
            .with_label_values(&["purge_below"])
            .start_timer();
        let zero_bytes = [0; EXPECTED_MESSAGE_ID_LENGTH];
        let key = IngressMessageId::new(expiry, MessageId::from(zero_bytes));
        let mut to_remove = self.artifacts.split_off(&key);
        std::mem::swap(&mut to_remove, &mut self.artifacts);
        for artifact in to_remove.values() {
            let artifact_size = artifact.as_ref().count_bytes();
            self.byte_size -= artifact_size;
            self.metrics.observe_remove(artifact_size);
        }
        // SAFETY: Checking byte size invariant
        section_ok(self);
        Box::new(to_remove.into_values())
    }
    /// Counts the exact bytes by iterating over the artifact btreemap, instead
    /// of returning the memoized byte_size.
    fn count_bytes_slow(&self) -> usize {
        self.artifacts
            .values()
            .map(|item| item.as_ref().count_bytes())
            .sum()
    }
}

/// Helper function to concisely validate that the real byte size of the pool section
/// (obtained by accumulating all btreemap values) is identical to count_bytes()
fn section_ok<T: AsRef<IngressPoolObject>>(section: &IngressPoolSection<T>) {
    debug_assert_eq!(
        section.count_bytes(),
        section.count_bytes_slow(),
        "invariant violated: byte_size == real size of btreemap"
    );
}

impl<T: AsRef<IngressPoolObject>> Default for IngressPoolSection<T> {
    fn default() -> Self {
        Self::new(PoolMetrics::new(
            MetricsRegistry::new(),
            POOL_INGRESS,
            "default",
        ))
    }
}

impl<T: AsRef<IngressPoolObject> + HasTimestamp> PoolSection<T> for IngressPoolSection<T> {
    fn get(&self, message_id: &IngressMessageId) -> Option<&T> {
        self.artifacts.get(message_id)
    }

    fn get_all_by_expiry_range<'a>(
        &self,
        range: std::ops::RangeInclusive<Time>,
    ) -> Box<dyn Iterator<Item = &T> + '_> {
        let (start, end) = range.into_inner();
        if end < start {
            return Box::new(std::iter::empty());
        }
        let min_bytes = [0; EXPECTED_MESSAGE_ID_LENGTH];
        let max_bytes = [0xff; EXPECTED_MESSAGE_ID_LENGTH];
        let range = std::ops::RangeInclusive::new(
            IngressMessageId::new(start, MessageId::from(min_bytes)),
            IngressMessageId::new(end, MessageId::from(max_bytes)),
        );
        let artifacts = &self.artifacts;
        Box::new(artifacts.range(range).map(|(_, v)| v))
    }

    fn get_timestamp(&self, message_id: &IngressMessageId) -> Option<Time> {
        self.get(message_id).map(|x| x.timestamp())
    }

    fn size(&self) -> usize {
        self.artifacts.len()
    }
}

#[derive(Clone)]
pub struct IngressPoolImpl {
    validated: IngressPoolSection<ValidatedIngressArtifact>,
    unvalidated: IngressPoolSection<UnvalidatedIngressArtifact>,
    // Track unvalidated pool quota usage only
    ingress_pool_max_count: usize,
    ingress_pool_max_bytes: usize,
    ingress_messages_throttled: IntCounter,
    node_id: NodeId,
    log: ReplicaLogger,
}

const POOL_INGRESS: &str = "ingress";

impl IngressPoolImpl {
    pub fn new(
        node_id: NodeId,
        config: ArtifactPoolConfig,
        metrics_registry: MetricsRegistry,
        log: ReplicaLogger,
    ) -> IngressPoolImpl {
        IngressPoolImpl {
            ingress_pool_max_count: config.ingress_pool_max_count,
            ingress_pool_max_bytes: config.ingress_pool_max_bytes,
            ingress_messages_throttled: metrics_registry.int_counter(
                "ingress_messages_throttled",
                "Number of throttled ingress messages",
            ),
            validated: IngressPoolSection::new(PoolMetrics::new(
                metrics_registry.clone(),
                POOL_INGRESS,
                POOL_TYPE_VALIDATED,
            )),
            unvalidated: IngressPoolSection::new(PoolMetrics::new(
                metrics_registry,
                POOL_INGRESS,
                POOL_TYPE_UNVALIDATED,
            )),
            node_id,
            log,
        }
    }

    /// Remove an artifact from unvalidated pool and remove it from peer_index
    /// Return the removed artifact and its size.
    fn remove_unvalidated(
        &mut self,
        message_id: &IngressMessageId,
    ) -> Option<(UnvalidatedIngressArtifact, usize)> {
        match self.unvalidated.remove(message_id) {
            Some(unvalidated_artifact) => {
                let size = unvalidated_artifact.message.signed_ingress.count_bytes();
                Some((unvalidated_artifact, size))
            }
            None => {
                trace!(self.log, "Did not find artifact in peer_index");
                None
            }
        }
    }
}

impl IngressPool for IngressPoolImpl {
    /// Validated Ingress Pool Section
    fn validated(&self) -> &dyn PoolSection<ValidatedIngressArtifact> {
        &self.validated
    }

    /// Unvalidated Ingress Pool
    fn unvalidated(&self) -> &dyn PoolSection<UnvalidatedIngressArtifact> {
        &self.unvalidated
    }
}

impl MutablePool<IngressArtifact> for IngressPoolImpl {
    type ChangeSet = ChangeSet;

    /// Insert a new ingress message in the Ingress Pool and update the
    /// peer_index
    fn insert(&mut self, artifact: UnvalidatedArtifact<SignedIngress>) {
        let ingress_pool_obj = IngressPoolObject::from(artifact.message);
        let peer_id = artifact.peer_id;
        let timestamp = artifact.timestamp;
        let size = ingress_pool_obj.count_bytes();

        debug!(
            self.log,
            "ingress_message_insert_unvalidated";
            ingress_message.message_id => format!("{}", ingress_pool_obj.message_id)
        );

        self.unvalidated.insert(
            IngressMessageId::from(&ingress_pool_obj),
            UnvalidatedIngressArtifact {
                message: ingress_pool_obj,
                peer_id,
                timestamp,
            },
        );
        debug!(
            self.log,
            "Ingress pool: insert {} bytes into unvalidated", size
        );
    }

    /// Removes an unvalidated ingress message from the unvalidated section.
    fn remove(&mut self, id: &IngressMessageId) {
        self.unvalidated.remove(id);
    }

    /// Apply changeset to the Ingress Pool
    fn apply_changes(
        &mut self,
        _time_source: &dyn TimeSource,
        change_set: ChangeSet,
    ) -> ChangeResult<IngressArtifact> {
        let changed = !change_set.is_empty();
        let mut adverts = Vec::new();
        let mut purged = Vec::new();
        for change_action in change_set {
            match change_action {
                ChangeAction::MoveToValidated((
                    message_id,
                    source_node_id,
                    size,
                    (),
                    integrity_hash,
                )) => {
                    if source_node_id == self.node_id {
                        adverts.push(Advert {
                            size,
                            id: message_id.clone(),
                            attribute: (),
                            integrity_hash: integrity_hash.clone(),
                        });
                    }
                    // remove it from unvalidated pool and remove it from peer_index, move it
                    // to the validated pool
                    match self.remove_unvalidated(&message_id) {
                        Some((unvalidated_artifact, size)) => {
                            self.validated.insert(
                                message_id,
                                ValidatedIngressArtifact {
                                    msg: unvalidated_artifact.message,
                                    timestamp: unvalidated_artifact.timestamp,
                                },
                            );
                            debug!(
                                self.log,
                                "Ingress pool: move {} bytes from unvalidated to validated", size
                            );
                        }
                        None => {
                            unreachable!(
                                "Unvalidated entry not found for MoveToValidated: {:?}",
                                message_id
                            );
                        }
                    }
                }
                ChangeAction::RemoveFromUnvalidated(message_id) => {
                    match self.remove_unvalidated(&message_id) {
                        Some((_, size)) => {
                            debug!(
                                self.log,
                                "Ingress pool: remove {} bytes from unvalidated", size
                            );
                        }
                        None => {
                            debug!(
                                self.log,
                                "Ingress pool: attempt to remove non-existent unvalidated ingress message {}",
                                message_id
                            );
                        }
                    }
                }
                ChangeAction::RemoveFromValidated(message_id) => {
                    match self.validated.remove(&message_id) {
                        Some(artifact) => {
                            purged.push(message_id);
                            let size = artifact.msg.signed_ingress.count_bytes();
                            debug!(
                                self.log,
                                "Ingress pool: remove {} bytes from validated", size
                            );
                        }
                        None => {
                            debug!(
                                self.log,
                                "Ingress pool: attempt to remove non-existent validated ingress message {}",
                                message_id
                            );
                        }
                    }
                }
                ChangeAction::PurgeBelowExpiry(expiry) => {
                    purged.extend(
                        self.validated
                            .purge_below(expiry)
                            .map(|i| (&i.msg.signed_ingress).into()),
                    );
                    let _unused = self.unvalidated.purge_below(expiry);
                }
            }
        }
        ChangeResult {
            purged,
            adverts,
            changed,
        }
    }
}

impl ValidatedPoolReader<IngressArtifact> for IngressPoolImpl {
    /// Check if an Ingress message exists by its hash
    fn contains(&self, id: &IngressMessageId) -> bool {
        self.unvalidated.exists(id) || self.validated.exists(id)
    }

    fn get_validated_by_identifier(&self, id: &IngressMessageId) -> Option<SignedIngress> {
        self.validated.get(id).map(|a| a.msg.signed_ingress.clone())
    }

    fn get_all_validated_by_filter<'a>(
        &'a self,
        _filter: &Height,
    ) -> Box<dyn Iterator<Item = SignedIngress> + 'a> {
        Box::new(vec![].into_iter())
    }
}

/// Implement the select interface required by IngressSelector (and consequently
/// by consensus). It allows the caller to select qualifying artifacts from the
/// validated pool without exposing extra functionalities.
impl IngressPoolSelect for IngressPoolImpl {
    fn select_validated<'a>(
        &self,
        range: std::ops::RangeInclusive<Time>,
        mut f: Box<dyn FnMut(&IngressPoolObject) -> SelectResult<SignedIngress> + 'a>,
    ) -> Vec<SignedIngress> {
        let mut collected = Vec::new();

        let mut artifacts = self
            .validated()
            .get_all_by_expiry_range(range)
            .collect::<Vec<_>>();

        // At this point [artifacts] are sorted by the expiry time. In order to prevent malicious
        // users from putting their messages ahead of others by carefully crafting the expiry
        // times, we sort the ingress messages by the time they were delivered to the pool.
        artifacts.sort_unstable_by_key(|artifact| artifact.timestamp);

        for artifact in artifacts {
            match f(&artifact.msg) {
                SelectResult::Selected(msg) => collected.push(msg),
                SelectResult::Skip => (),
                SelectResult::Abort => break,
            }
        }
        collected
    }
}

impl IngressPoolThrottler for IngressPoolImpl {
    fn exceeds_threshold(&self) -> bool {
        let ingress_count = self.validated.size() + self.unvalidated.size();
        let ingress_bytes = self.validated.count_bytes() + self.unvalidated.count_bytes();

        if ingress_count >= self.ingress_pool_max_count
            || ingress_bytes >= self.ingress_pool_max_bytes
        {
            self.ingress_messages_throttled.inc();
            return true;
        }
        false
    }
}

pub struct IngressPrioritizer {
    time_source: Arc<dyn TimeSource>,
}

impl IngressPrioritizer {
    pub fn new(time_source: Arc<dyn TimeSource>) -> Self {
        Self { time_source }
    }
}

impl PriorityFnAndFilterProducer<IngressArtifact, IngressPoolImpl> for IngressPrioritizer {
    fn get_priority_function(&self, pool: &IngressPoolImpl) -> PriorityFn<IngressMessageId, ()> {
        // EXPLANATION: Because ingress messages are included in blocks, consensus
        // does not rely on ingress gossip for correctness. Ingress gossip exists to
        // reduce latency in cases where replicas don't have enough ingress messages
        // to fill their block. Once a replica's pool is full, ingress gossip just
        // causes redundant traffic between replicas, and is thus not needed.
        // Please note that all P2P ingress messages will be dropped if 'exceeds_threshold'
        // returns true until the next invocation of 'get_priority_function'.
        if pool.exceeds_threshold() {
            return Box::new(move |_, _| Priority::Drop);
        }
        let time_source = self.time_source.clone();
        Box::new(move |ingress_id, _| {
            let start = time_source.get_relative_time();
            let range = start..=start + MAX_INGRESS_TTL;
            if range.contains(&ingress_id.expiry()) {
                Priority::Later
            } else {
                Priority::Drop
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_constants::MAX_INGRESS_TTL;
    use ic_interfaces::artifact_pool::MutablePool;
    use ic_interfaces::time_source::{SysTimeSource, TimeSource};
    use ic_test_utilities::{
        mock_time, types::ids::node_test_id, types::messages::SignedIngressBuilder,
        FastForwardTimeSource,
    };
    use ic_test_utilities_logger::with_test_replica_logger;
    use rand::Rng;
    use std::time::Duration;

    #[test]
    fn test_insert_in_ingress_pool() {
        with_test_replica_logger(|_log| {
            let mut ingress_pool = IngressPoolSection::default();
            let ingress_msg = SignedIngressBuilder::new().build();
            let message_id = IngressMessageId::from(&ingress_msg);

            ingress_pool.insert(
                message_id,
                UnvalidatedIngressArtifact {
                    message: IngressPoolObject::from(ingress_msg),
                    peer_id: node_test_id(0),
                    timestamp: mock_time(),
                },
            );
            assert_eq!(ingress_pool.size(), 1);
        });
    }

    #[test]
    fn test_exists() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                let ingress_msg = SignedIngressBuilder::new().nonce(1).build();
                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg,
                    peer_id: node_test_id(0),
                    timestamp: time_source.get_relative_time(),
                });
                let ingress_msg = SignedIngressBuilder::new().nonce(2).build();
                let message_id = IngressMessageId::from(&ingress_msg);

                ingress_pool.validated.insert(
                    message_id.clone(),
                    ValidatedIngressArtifact {
                        msg: IngressPoolObject::from(ingress_msg),
                        timestamp: mock_time(),
                    },
                );
                assert!(ingress_pool.contains(&message_id));
            })
        })
    }

    #[test]
    fn test_not_exists() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                let ingress_msg = SignedIngressBuilder::new().nonce(1).build();
                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg,
                    peer_id: node_test_id(0),
                    timestamp: time_source.get_relative_time(),
                });
                let ingress_msg = SignedIngressBuilder::new().nonce(2).build();
                let message_id = IngressMessageId::from(&ingress_msg);

                ingress_pool.validated.insert(
                    message_id,
                    ValidatedIngressArtifact {
                        msg: IngressPoolObject::from(ingress_msg),
                        timestamp: mock_time(),
                    },
                );

                // Ingress message not in the pool
                let ingress_msg = SignedIngressBuilder::new().nonce(3).build();
                assert!(!ingress_pool.contains(&IngressMessageId::from(&ingress_msg)));
            })
        })
    }

    #[test]
    fn test_insert_remove() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);

                let ingress_msg = SignedIngressBuilder::new().nonce(1).build();
                let message_id = IngressMessageId::from(&ingress_msg);

                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg,
                    peer_id: node_test_id(0),
                    timestamp: time_source.get_relative_time(),
                });
                assert!(ingress_pool.contains(&message_id));

                ingress_pool.remove(&message_id);
                assert!(!ingress_pool.contains(&message_id));
            })
        })
    }

    #[test]
    fn test_get_all_validated_by_filter() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let time = time_source.get_relative_time();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);

                let max_seconds = 10;
                let range_min = 3;
                let range_max = 7;
                let step = 66; // in seconds
                let range_min = time + Duration::from_secs(range_min * step);
                let range_max = time + Duration::from_secs(range_max * step);
                let range = range_min..=range_max;
                let mut msgs_in_range = Vec::new();
                for i in 0..max_seconds {
                    let expiry = time + Duration::from_secs(i * step);
                    let ingress_msg = SignedIngressBuilder::new().expiry_time(expiry).build();
                    if range.contains(&expiry) {
                        msgs_in_range.push(ingress_msg.clone())
                    }
                    let message_id = IngressMessageId::from(&ingress_msg);
                    ingress_pool.validated.insert(
                        message_id.clone(),
                        ValidatedIngressArtifact {
                            msg: IngressPoolObject::from(ingress_msg),
                            timestamp: mock_time(),
                        },
                    );
                }
                // empty
                let filtered_msgs = ingress_pool.get_all_validated_by_filter(&Height::default());
                assert!(filtered_msgs.count() == 0);
            })
        })
    }

    #[test]
    fn test_timestamp() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let time_0 = time_source.get_relative_time();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                let ingress_msg_0 = SignedIngressBuilder::new().nonce(1).build();
                let message_id0 = IngressMessageId::from(&ingress_msg_0);
                let msg_0_integrity_hash =
                    ic_types::crypto::crypto_hash(ingress_msg_0.binary()).get();

                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg_0,
                    peer_id: node_test_id(0),
                    timestamp: time_source.get_relative_time(),
                });

                let time_1 = time_0 + Duration::from_secs(42);
                time_source.set_time(time_1).unwrap();

                let ingress_msg_1 = SignedIngressBuilder::new().nonce(2).build();
                let message_id1 = IngressMessageId::from(&ingress_msg_1);
                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg_1,
                    peer_id: node_test_id(0),
                    timestamp: time_source.get_relative_time(),
                });

                // Check timestamp is the insertion time.
                assert_eq!(
                    ingress_pool.unvalidated.get_timestamp(&message_id0),
                    Some(time_0)
                );
                assert_eq!(
                    ingress_pool.unvalidated.get_timestamp(&message_id1),
                    Some(time_1)
                );

                let changeset = vec![
                    ChangeAction::MoveToValidated((
                        message_id0.clone(),
                        node_test_id(0),
                        0,
                        (),
                        msg_0_integrity_hash,
                    )),
                    ChangeAction::RemoveFromUnvalidated(message_id1.clone()),
                ];
                let result = ingress_pool.apply_changes(&SysTimeSource::new(), changeset);

                // Check moved message is returned as an advert
                assert!(result.purged.is_empty());
                assert_eq!(result.adverts.len(), 1);
                assert_eq!(result.adverts[0].id, message_id0);
                assert!(result.changed);
                // Check timestamp is carried over for msg_0.
                assert_eq!(ingress_pool.unvalidated.get_timestamp(&message_id0), None);
                assert_eq!(
                    ingress_pool.validated.get_timestamp(&message_id0),
                    Some(time_0)
                );
                // Check timestamp is removed for msg_1.
                assert_eq!(ingress_pool.unvalidated.get_timestamp(&message_id1), None);
                assert_eq!(ingress_pool.validated.get_timestamp(&message_id1), None);
            })
        })
    }

    #[test]
    fn test_purge_below() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                let nodes = 10;
                let mut changeset = ChangeSet::new();
                let ingress_size = 10;
                let mut rng = rand::thread_rng();
                let now = time_source.get_relative_time();
                let cutoff_time = now + MAX_INGRESS_TTL * 3 / 2;
                let initial_count = 1000;
                let mut non_expired_count = 0;
                for i in 0..initial_count {
                    let expiry = Duration::from_millis(
                        rng.gen::<u64>() % (3 * (MAX_INGRESS_TTL.as_millis() as u64)),
                    );
                    if now + expiry >= cutoff_time {
                        non_expired_count += 1;
                    }
                    let ingress = SignedIngressBuilder::new()
                        .method_payload(vec![0; ingress_size])
                        .nonce(i as u64)
                        .expiry_time(now + expiry)
                        .build();
                    let message_id = IngressMessageId::from(&ingress);
                    let integrity_hash = ic_types::crypto::crypto_hash(ingress.binary()).get();
                    let peer_id = (i % nodes) as u64;
                    ingress_pool.insert(UnvalidatedArtifact {
                        message: ingress,
                        peer_id: node_test_id(peer_id),
                        timestamp: time_source.get_relative_time(),
                    });
                    changeset.push(ChangeAction::MoveToValidated((
                        message_id,
                        node_test_id(peer_id),
                        0,
                        (),
                        integrity_hash,
                    )));
                }
                assert_eq!(ingress_pool.unvalidated().size(), initial_count);
                let result = ingress_pool.apply_changes(&SysTimeSource::new(), changeset);
                assert!(result.purged.is_empty());
                // adverts are only created for own node id
                assert_eq!(result.adverts.len(), initial_count / nodes);
                assert!(result.changed);
                assert_eq!(ingress_pool.unvalidated().size(), 0);
                assert_eq!(ingress_pool.validated().size(), initial_count);

                let changeset = vec![ChangeAction::PurgeBelowExpiry(cutoff_time)];
                let result = ingress_pool.apply_changes(&SysTimeSource::new(), changeset);
                assert!(result.adverts.is_empty());
                assert_eq!(result.purged.len(), initial_count - non_expired_count);
                assert!(result.changed);
                assert_eq!(ingress_pool.validated().size(), non_expired_count);
            })
        })
    }

    #[test]
    fn test_exceeds_threshold_msgcount() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|mut pool_config| {
                // 3 ingress messages, each with 153 bytes (subject to change)
                pool_config.ingress_pool_max_bytes = 153 * 5;
                pool_config.ingress_pool_max_count = 3;
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                assert!(!ingress_pool.exceeds_threshold());

                // MESSAGE #1
                insert_unvalidated_artifact(&mut ingress_pool, 2, time_source.get_relative_time());
                assert!(!ingress_pool.exceeds_threshold());
                // MESSAGE #2
                insert_validated_artifact(&mut ingress_pool, 3);
                assert!(!ingress_pool.exceeds_threshold());
                // MESSAGE #3
                insert_unvalidated_artifact(&mut ingress_pool, 4, time_source.get_relative_time());
                assert!(ingress_pool.exceeds_threshold());
                // MESSAGE #4
                insert_unvalidated_artifact(&mut ingress_pool, 5, time_source.get_relative_time());
                assert!(ingress_pool.exceeds_threshold());
            })
        })
    }

    #[test]
    fn test_exceeds_threshold_bytes() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|mut pool_config| {
                // 3 ingress messages, each with 153 bytes (subject to change)
                pool_config.ingress_pool_max_bytes = 153 * 3;
                pool_config.ingress_pool_max_count = 5;
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);
                assert!(!ingress_pool.exceeds_threshold());

                // MESSAGE #1
                insert_unvalidated_artifact(&mut ingress_pool, 2, time_source.get_relative_time());
                assert!(!ingress_pool.exceeds_threshold());
                // MESSAGE #2
                insert_validated_artifact(&mut ingress_pool, 3);
                assert!(!ingress_pool.exceeds_threshold());
                // MESSAGE #3
                insert_unvalidated_artifact(&mut ingress_pool, 4, time_source.get_relative_time());
                assert!(ingress_pool.exceeds_threshold());
                // MESSAGE #4
                insert_unvalidated_artifact(&mut ingress_pool, 5, time_source.get_relative_time());
                assert!(ingress_pool.exceeds_threshold());
            })
        })
    }

    #[test]
    fn test_throttling_disabled() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|mut pool_config| {
                pool_config.ingress_pool_max_count = usize::MAX;
                pool_config.ingress_pool_max_bytes = usize::MAX;
                let time_source = FastForwardTimeSource::new();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);

                assert!(!ingress_pool.exceeds_threshold());

                let ingress_msg = SignedIngressBuilder::new().nonce(2).build();
                ingress_pool.insert(UnvalidatedArtifact {
                    message: ingress_msg,
                    peer_id: node_test_id(100),
                    timestamp: time_source.get_relative_time(),
                });
                assert!(!ingress_pool.exceeds_threshold());
            })
        })
    }

    #[test]
    fn select_validated_sorts_messages() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time = |millis: u64| Time::from_millis_since_unix_epoch(millis).unwrap();
                let nonce = |nonce: u64| nonce.to_le_bytes().to_vec();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);

                insert_validated_artifact_with_timestamps(&mut ingress_pool, 0, time(1), time(30));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 1, time(3), time(40));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 2, time(2), time(50));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 3, time(5), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 4, time(4), time(20));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 5, time(6), time(60));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 6, time(6), time(0));

                let selected = ingress_pool.select_validated(
                    time(10)..=time(50),
                    Box::new(|ingress_obj| {
                        SelectResult::Selected(ingress_obj.signed_ingress.clone())
                    }),
                );
                assert_eq!(
                    selected
                        .iter()
                        .map(|message| message.nonce().unwrap())
                        .collect::<Vec<_>>(),
                    // Artifacts with nonce = 5, 6 are filtered out because of their expiry times
                    // being outside the range
                    &[nonce(0), nonce(2), nonce(1), nonce(4), nonce(3)]
                );
            });
        });
    }

    #[test]
    fn select_validated_applies_closure() {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
                let time = |millis: u64| Time::from_millis_since_unix_epoch(millis).unwrap();
                let nonce = |nonce: u64| nonce.to_le_bytes().to_vec();
                let metrics_registry = MetricsRegistry::new();
                let mut ingress_pool =
                    IngressPoolImpl::new(node_test_id(0), pool_config, metrics_registry, log);

                insert_validated_artifact_with_timestamps(&mut ingress_pool, 0, time(0), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 1, time(1), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 2, time(2), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 3, time(3), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 4, time(4), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 5, time(5), time(10));
                insert_validated_artifact_with_timestamps(&mut ingress_pool, 6, time(6), time(10));

                let mut counter = 0;

                let selected = ingress_pool.select_validated(
                    time(10)..=time(10),
                    Box::new(|ingress_obj| {
                        let result = match counter {
                            1 | 3 => SelectResult::Skip,
                            4 => SelectResult::Abort,
                            _ => SelectResult::Selected(ingress_obj.signed_ingress.clone()),
                        };
                        counter += 1;
                        result
                    }),
                );

                // nonce = 0 Selected
                // nonce = 1 Skipped
                // nonce = 2 Selected
                // nonce = 3 Skipped
                // nonce = 4 Aborted
                // nonce = 5 not selected because already aborted
                // nonce = 6 not selected because already aborted
                assert_eq!(
                    selected
                        .iter()
                        .map(|message| message.nonce().unwrap())
                        .collect::<Vec<_>>(),
                    &[nonce(0), nonce(2)]
                );
            });
        });
    }

    fn insert_validated_artifact(ingress_pool: &mut IngressPoolImpl, nonce: u64) {
        insert_validated_artifact_with_timestamps(
            ingress_pool,
            nonce,
            mock_time(),
            ic_types::time::expiry_time_from_now(),
        );
    }

    fn insert_validated_artifact_with_timestamps(
        ingress_pool: &mut IngressPoolImpl,
        nonce: u64,
        receive_time: Time,
        expiry_time: Time,
    ) {
        let ingress_msg = SignedIngressBuilder::new()
            .nonce(nonce)
            .expiry_time(expiry_time)
            .build();

        let message_id = IngressMessageId::from(&ingress_msg);
        ingress_pool.validated.insert(
            message_id,
            ValidatedIngressArtifact {
                msg: IngressPoolObject::from(ingress_msg),
                timestamp: receive_time,
            },
        );
    }

    fn insert_unvalidated_artifact(ingress_pool: &mut IngressPoolImpl, nonce: u64, time: Time) {
        let ingress_msg = SignedIngressBuilder::new().nonce(nonce).build();
        ingress_pool.insert(UnvalidatedArtifact {
            message: ingress_msg,
            peer_id: node_test_id(nonce * 100),
            timestamp: time,
        });
    }
}
