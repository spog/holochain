use super::*;
use crate::event::*;
use kitsune_p2p_types::dht_arc::*;

pub(crate) async fn step_2_local_sync_inner(
    space: Arc<KitsuneSpace>,
    evt_sender: futures::channel::mpsc::Sender<event::KitsuneP2pEvent>,
    local_agents: HashSet<Arc<KitsuneAgent>>,
) -> KitsuneResult<(DataMap, KeySet, BloomFilter)> {
    let mut inner = Inner {
        space,
        evt_sender,
        local_agents,
        data_map: HashMap::new(),
        has_hash: HashMap::new(),
    };

    inner.collect_local_ops().await;
    inner.collect_local_agents().await;
    inner.local_sync().await?;
    Ok(inner.finish())
}

struct Inner {
    space: Arc<KitsuneSpace>,
    evt_sender: futures::channel::mpsc::Sender<event::KitsuneP2pEvent>,
    local_agents: HashSet<Arc<KitsuneAgent>>,
    data_map: DataMap,
    has_hash: HasMap,
}

impl Inner {
    pub async fn collect_local_ops(&mut self) {
        let Inner {
            space,
            evt_sender,
            local_agents,
            has_hash,
            ..
        } = self;

        // collect all local agents' ops
        for agent in local_agents.iter() {
            if let Ok(ops) = evt_sender
                .fetch_op_hashes_for_constraints(FetchOpHashesForConstraintsEvt {
                    space: space.clone(),
                    agent: agent.clone(),
                    dht_arc: DhtArc::new(0, u32::MAX),
                    since_utc_epoch_s: i64::MIN,
                    until_utc_epoch_s: i64::MAX,
                })
                .await
            {
                for op in ops {
                    let key = Arc::new(MetaOpKey::Op(op));
                    has_hash
                        .entry(agent.clone())
                        .or_insert_with(HashSet::new)
                        .insert(key);
                }
            }
        }
    }

    pub async fn collect_local_agents(&mut self) {
        let Inner {
            space,
            evt_sender,
            local_agents,
            data_map,
            has_hash,
            ..
        } = self;

        // agent store is shared between agents in one space
        // we only have to query it once for all local_agents
        if let Some(agent) = local_agents.iter().next() {
            if let Ok(agent_infos) = evt_sender
                .query_agent_info_signed(QueryAgentInfoSignedEvt {
                    space: space.clone(),
                    agent: agent.clone(),
                })
                .await
            {
                for agent_info in agent_infos {
                    let data = Arc::new(MetaOpData::Agent(agent_info));
                    let key = data.key();
                    data_map.insert(key.clone(), data);
                    for (_agent, has) in has_hash.iter_mut() {
                        has.insert(key.clone());
                    }
                }
            }
        }
    }

    pub async fn local_sync(&mut self) -> KitsuneResult<()> {
        let mut new_has_map = self.has_hash.clone();

        let Self {
            space,
            evt_sender,
            data_map,
            has_hash,
            ..
        } = self;

        let mut local_synced_ops = 0;
        for (old_agent, old_set) in has_hash.iter() {
            for (new_agent, new_set) in new_has_map.iter_mut() {
                if old_agent == new_agent {
                    continue;
                }
                for old_key in old_set.iter() {
                    if !new_set.contains(old_key) {
                        local_synced_ops += 1;
                        let op_data =
                            data_map_get(evt_sender, space, old_agent, data_map, &old_key).await?;

                        match &*op_data {
                            MetaOpData::Op(key, data) => {
                                evt_sender
                                    .gossip(
                                        space.clone(),
                                        new_agent.clone(),
                                        old_agent.clone(),
                                        key.clone(),
                                        data.clone(),
                                    )
                                    .await
                                    .map_err(KitsuneError::other)?;
                            }
                            // this should be impossible right now
                            // due to the shared agent store
                            MetaOpData::Agent(_) => unreachable!(),
                        }

                        new_set.insert(old_key.clone());
                    }
                }
            }
        }

        if local_synced_ops > 0 {
            tracing::debug!(
                %local_synced_ops,
                "local sync",
            );
        }

        *has_hash = new_has_map;

        Ok(())
    }

    pub fn finish(self) -> (DataMap, KeySet, BloomFilter) {
        let Self {
            data_map, has_hash, ..
        } = self;

        // 1 in 100 false positives...
        // we can get 1 in 1000 for ~2x the filter size, but may not be worth it
        // 1 in 100 pretty much guarantees full sync after two communications.
        const TGT_FP: f64 = 0.01;

        // at this point, all the local has_hash maps should be identical,
        // so we can just take the first one
        let (key_set, bloom) = if let Some((_, map)) = has_hash.into_iter().next() {
            let len = map.len();
            tracing::trace!(
                local_op_count=%len,
                "generating local bloom",
            );
            let mut bloom = bloomfilter::Bloom::new_for_fp_rate(len, TGT_FP);
            for h in map.iter() {
                bloom.set(h);
            }
            (map, bloom)
        } else {
            (HashSet::new(), bloomfilter::Bloom::new(1, 1))
        };

        (data_map, key_set, bloom)
    }
}

async fn data_map_get(
    evt_sender: &mut futures::channel::mpsc::Sender<event::KitsuneP2pEvent>,
    space: &Arc<KitsuneSpace>,
    agent: &Arc<KitsuneAgent>,
    map: &mut DataMap,
    key: &Arc<MetaOpKey>,
) -> KitsuneResult<Arc<MetaOpData>> {
    use crate::event::*;
    if let Some(data) = map.get(key) {
        return Ok(data.clone());
    }
    match &**key {
        MetaOpKey::Op(key) => {
            let mut op = evt_sender
                .fetch_op_hash_data(FetchOpHashDataEvt {
                    space: space.clone(),
                    agent: agent.clone(),
                    op_hashes: vec![key.clone()],
                })
                .await
                .map_err(KitsuneError::other)?;

            if op.len() != 1 {
                return Err(format!("Error fetching op {:?}", &key).into());
            }

            let (key, data) = op.remove(0);
            let data = Arc::new(MetaOpData::Op(key.clone(), data));
            let key = Arc::new(MetaOpKey::Op(key));

            map.insert(key, data.clone());
            Ok(data)
        }
        // the query agents api returns all the data,
        // so we should already be fully pre-populated.
        MetaOpKey::Agent(_, _) => unreachable!(),
    }
}
