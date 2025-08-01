#![allow(clippy::arithmetic_side_effects)]
use {
    bincode::serialized_size,
    itertools::Itertools,
    log::*,
    rayon::{prelude::*, ThreadPool, ThreadPoolBuilder},
    serial_test::serial,
    solana_gossip::{
        cluster_info_metrics::GossipStats,
        contact_info::ContactInfo,
        crds::GossipRoute,
        crds_data::CrdsData,
        crds_gossip::*,
        crds_gossip_error::CrdsGossipError,
        crds_gossip_pull::{
            CrdsTimeouts, ProcessPullStats, PullRequest, CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
        },
        crds_gossip_push::CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS,
        crds_value::{CrdsValue, CrdsValueLabel},
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_rayon_threadlimit::get_thread_count,
    solana_sha256_hasher::hash,
    solana_signer::Signer,
    solana_streamer::socket::SocketAddrSpace,
    solana_time_utils::timestamp,
    std::{
        collections::{HashMap, HashSet},
        net::{Ipv4Addr, SocketAddr},
        ops::Deref,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    },
};

type PingCache = solana_gossip::ping_pong::PingCache<32>;

#[derive(Clone)]
struct Node {
    keypair: Arc<Keypair>,
    contact_info: ContactInfo,
    gossip: Arc<CrdsGossip>,
    ping_cache: Arc<Mutex<PingCache>>,
    stake: u64,
}

impl Node {
    fn new(keypair: Arc<Keypair>, contact_info: ContactInfo, gossip: Arc<CrdsGossip>) -> Self {
        Self::staked(keypair, contact_info, gossip, 0)
    }

    fn staked(
        keypair: Arc<Keypair>,
        contact_info: ContactInfo,
        gossip: Arc<CrdsGossip>,
        stake: u64,
    ) -> Self {
        let ping_cache = Arc::new(new_ping_cache());
        Node {
            keypair,
            contact_info,
            gossip,
            ping_cache,
            stake,
        }
    }
}

struct Network {
    nodes: HashMap<Pubkey, Node>,
    stake_pruned: u64,
    connections_pruned: HashSet<(Pubkey, Pubkey)>,
}

impl Network {
    fn new(nodes: HashMap<Pubkey, Node>) -> Self {
        Network {
            nodes,
            connections_pruned: HashSet::new(),
            stake_pruned: 0,
        }
    }
}

impl Deref for Network {
    type Target = HashMap<Pubkey, Node>;

    fn deref(&self) -> &Self::Target {
        &self.nodes
    }
}

fn stakes(network: &Network) -> HashMap<Pubkey, u64> {
    let mut stakes = HashMap::new();
    for (key, Node { stake, .. }) in network.iter() {
        stakes.insert(*key, *stake);
    }
    stakes
}

fn star_network_create(num: usize) -> Network {
    let gossip_port_offset = 9000;
    let node_keypair = Arc::new(Keypair::new());
    let contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
    let entry = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
    let mut network: HashMap<_, _> = (1..num)
        .map(|k| {
            let node_keypair = Arc::new(Keypair::new());
            // Need unique gossip addresses, otherwise nodes will be deduped by
            // crds_gossip::dedup_gossip_addresses before peer sampling.
            let mut contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
            let gossip_port = gossip_port_offset + u16::try_from(k).unwrap();
            contact_info
                .set_gossip((Ipv4Addr::LOCALHOST, gossip_port))
                .unwrap();
            let new = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
            let node = CrdsGossip::default();
            {
                let mut node_crds = node.crds.write().unwrap();
                node_crds
                    .insert(new.clone(), timestamp(), GossipRoute::LocalMessage)
                    .unwrap();
                node_crds
                    .insert(entry.clone(), timestamp(), GossipRoute::LocalMessage)
                    .unwrap();
            }
            let node = Node::new(node_keypair, contact_info, Arc::new(node));
            (new.label().pubkey(), node)
        })
        .collect();
    let node = CrdsGossip::default();
    let id = entry.label().pubkey();
    node.crds
        .write()
        .unwrap()
        .insert(entry, timestamp(), GossipRoute::LocalMessage)
        .unwrap();
    let node = Node::new(node_keypair, contact_info, Arc::new(node));
    network.insert(id, node);
    Network::new(network)
}

fn rstar_network_create(num: usize) -> Network {
    let node_keypair = Arc::new(Keypair::new());
    let contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
    let entry = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
    let origin = CrdsGossip::default();
    let id = entry.label().pubkey();
    origin
        .crds
        .write()
        .unwrap()
        .insert(entry, timestamp(), GossipRoute::LocalMessage)
        .unwrap();
    let mut network: HashMap<_, _> = (1..num)
        .map(|_| {
            let node_keypair = Arc::new(Keypair::new());
            let contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
            let new = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
            let node = CrdsGossip::default();
            node.crds
                .write()
                .unwrap()
                .insert(new.clone(), timestamp(), GossipRoute::LocalMessage)
                .unwrap();
            origin
                .crds
                .write()
                .unwrap()
                .insert(new.clone(), timestamp(), GossipRoute::LocalMessage)
                .unwrap();
            let node = Node::new(node_keypair, contact_info, Arc::new(node));
            (new.label().pubkey(), node)
        })
        .collect();
    let node = Node::new(node_keypair, contact_info, Arc::new(origin));
    network.insert(id, node);
    Network::new(network)
}

fn ring_network_create(num: usize) -> Network {
    let mut network: HashMap<_, _> = (0..num)
        .map(|_| {
            let node_keypair = Arc::new(Keypair::new());
            let contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
            let new = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
            let node = CrdsGossip::default();
            node.crds
                .write()
                .unwrap()
                .insert(new.clone(), timestamp(), GossipRoute::LocalMessage)
                .unwrap();
            let node = Node::new(node_keypair, contact_info, Arc::new(node));
            (new.label().pubkey(), node)
        })
        .collect();
    let keys: Vec<Pubkey> = network.keys().cloned().collect();
    for k in 0..keys.len() {
        let start_info = {
            let start = &network[&keys[k]];
            let start_id = keys[k];
            let label = CrdsValueLabel::ContactInfo(start_id);
            let gossip_crds = start.gossip.crds.read().unwrap();
            gossip_crds.get::<&CrdsValue>(&label).unwrap().clone()
        };
        let end = network.get_mut(&keys[(k + 1) % keys.len()]).unwrap();
        let mut end_crds = end.gossip.crds.write().unwrap();
        end_crds
            .insert(start_info, timestamp(), GossipRoute::LocalMessage)
            .unwrap();
    }
    Network::new(network)
}

fn connected_staked_network_create(stakes: &[u64]) -> Network {
    let num = stakes.len();
    let mut network: HashMap<_, _> = (0..num)
        .map(|n| {
            let node_keypair = Arc::new(Keypair::new());
            let contact_info = ContactInfo::new_localhost(&node_keypair.pubkey(), 0);
            let new = CrdsValue::new(CrdsData::from(&contact_info), &node_keypair);
            let node = CrdsGossip::default();
            node.crds
                .write()
                .unwrap()
                .insert(new.clone(), timestamp(), GossipRoute::LocalMessage)
                .unwrap();
            let node = Node::staked(node_keypair, contact_info, Arc::new(node), stakes[n]);
            (new.label().pubkey(), node)
        })
        .collect();

    let keys: Vec<Pubkey> = network.keys().cloned().collect();
    let start_entries: Vec<_> = keys
        .iter()
        .map(|k| {
            let start = &network[k];
            let start_label = CrdsValueLabel::ContactInfo(*k);
            let gossip_crds = start.gossip.crds.read().unwrap();
            gossip_crds.get::<&CrdsValue>(&start_label).unwrap().clone()
        })
        .collect();
    for (end_pubkey, end) in network.iter_mut() {
        let mut end_crds = end.gossip.crds.write().unwrap();
        for k in 0..keys.len() {
            if keys[k] != *end_pubkey {
                let start_info = start_entries[k].clone();
                end_crds
                    .insert(start_info, timestamp(), GossipRoute::LocalMessage)
                    .unwrap();
            }
        }
    }
    Network::new(network)
}

fn network_simulator_pull_only(thread_pool: &ThreadPool, network: &Network) {
    let num = network.len();
    // In absence of gossip push messages, a pull only network with star
    // topology will not converge because it forms a DAG. We add additional
    // edges so that there is a directed path between every two pair of nodes.
    let (pubkeys, mut entries): (Vec<_>, Vec<_>) = network
        .nodes
        .iter()
        .map(|(&pubkey, node)| {
            let label = CrdsValueLabel::ContactInfo(pubkey);
            let crds = node.gossip.crds.read().unwrap();
            let entry = crds.get::<&CrdsValue>(&label).unwrap().clone();
            (pubkey, entry)
        })
        .unzip();
    entries.rotate_right(1);
    for (pubkey, entry) in pubkeys.into_iter().zip(entries) {
        let mut crds = network.nodes[&pubkey].gossip.crds.write().unwrap();
        let _ = crds.insert(entry, timestamp(), GossipRoute::LocalMessage);
    }
    let (converged, bytes_tx) = network_run_pull(thread_pool, network, 0, num * 2, 0.9);
    trace!("network_simulator_pull_{num}: converged: {converged} total_bytes: {bytes_tx}");
    assert!(converged >= 0.9);
}

fn network_simulator(thread_pool: &ThreadPool, network: &mut Network, max_convergance: f64) {
    let num = network.len();
    // run for a small amount of time
    let (converged, bytes_tx) = network_run_pull(thread_pool, network, 0, 10, 1.0);
    trace!("network_simulator_push_{num}: converged: {converged}");
    // make sure there is someone in the active set
    let network_values: Vec<Node> = network.values().cloned().collect();
    network_values.par_iter().for_each(|node| {
        node.gossip.refresh_push_active_set(
            &node.keypair,
            0,               // shred version
            &HashMap::new(), // stakes
            None,            // gossip validators
            &node.ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );
    });
    let mut total_bytes = bytes_tx;
    let mut ts = timestamp();
    for _ in 1..num {
        let start = ts.div_ceil(100) as usize;
        let end = start + 10;
        let now = (start * 100) as u64;
        ts += 1000;
        // push a message to the network
        network_values.par_iter().for_each(|node| {
            let node_pubkey = node.keypair.pubkey();
            let mut m = {
                let node_crds = node.gossip.crds.read().unwrap();
                node_crds.get::<&ContactInfo>(node_pubkey).cloned().unwrap()
            };
            m.set_wallclock(now);
            node.gossip.process_push_message(
                vec![(
                    Pubkey::default(),
                    vec![CrdsValue::new(CrdsData::ContactInfo(m), &Keypair::new())],
                )],
                now,
            );
        });
        // push for a bit
        let (queue_size, bytes_tx) = network_run_push(thread_pool, network, start, end);
        total_bytes += bytes_tx;
        trace!("network_simulator_push_{num}: queue_size: {queue_size} bytes: {bytes_tx}");
        // pull for a bit
        let (converged, bytes_tx) = network_run_pull(thread_pool, network, start, end, 1.0);
        total_bytes += bytes_tx;
        trace!(
            "network_simulator_push_{num}: converged: {converged} bytes: {bytes_tx} total_bytes: \
             {total_bytes}"
        );
        if converged > max_convergance {
            break;
        }
    }
}

fn network_run_push(
    thread_pool: &ThreadPool,
    network: &mut Network,
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut bytes: usize = 0;
    let mut num_msgs: usize = 0;
    let mut total: usize = 0;
    let num = network.len();
    let mut prunes: usize = 0;
    let mut delivered: usize = 0;
    let mut stake_pruned: u64 = 0;
    let network_values: Vec<Node> = network.values().cloned().collect();
    let stakes = stakes(network);
    for t in start..end {
        let now = t as u64 * 100;
        let requests: Vec<_> = network_values
            .par_iter()
            .map(|node| {
                let node_pubkey = node.keypair.pubkey();
                let timeouts = node.gossip.make_timeouts(
                    node_pubkey,
                    &stakes,
                    Duration::from_millis(node.gossip.pull.crds_timeout),
                );
                node.gossip.purge(&node_pubkey, thread_pool, now, &timeouts);
                let (entries, messages, _) = node.gossip.new_push_messages(
                    &node_pubkey,
                    now,
                    &stakes,
                    |_| true, // should_retain_crds_value
                );
                let messages = messages
                    .into_iter()
                    .map(|(pubkey, indices)| {
                        let values = indices.into_iter().map(|k| entries[k].clone()).collect();
                        (pubkey, values)
                    })
                    .collect::<Vec<(_, Vec<_>)>>();
                (node_pubkey, messages)
            })
            .collect();
        let transfered: Vec<_> = requests
            .into_par_iter()
            .map(|(from, push_messages)| {
                let mut bytes: usize = 0;
                let mut delivered: usize = 0;
                let mut num_msgs: usize = 0;
                let mut pruned: HashSet<(Pubkey, Pubkey)> = HashSet::new();
                for (to, msgs) in push_messages {
                    // 8 bytes for encoding the length of the vector.
                    bytes += 8 + msgs
                        .iter()
                        .map(CrdsValue::bincode_serialized_size)
                        .sum::<usize>();
                    num_msgs += 1;
                    let origins: HashSet<_> = network
                        .get(&to)
                        .unwrap()
                        .gossip
                        .process_push_message(vec![(from, msgs.clone())], now)
                        .into_iter()
                        .collect();
                    let prunes_map = network
                        .get(&to)
                        .map(|node| {
                            let node_pubkey = node.keypair.pubkey();
                            node.gossip
                                .prune_received_cache(&node_pubkey, origins, &stakes)
                        })
                        .unwrap();

                    for (from, prune_set) in prunes_map {
                        let prune_keys: Vec<_> = prune_set.into_iter().collect();
                        for prune_key in &prune_keys {
                            pruned.insert((from, *prune_key));
                        }

                        bytes += serialized_size(&prune_keys).unwrap() as usize;
                        delivered += 1;

                        network
                            .get(&from)
                            .map(|node| {
                                let node_pubkey = node.keypair.pubkey();
                                let destination = node_pubkey;
                                let now = timestamp();
                                node.gossip
                                    .process_prune_msg(
                                        &node_pubkey,
                                        &to,
                                        &destination,
                                        &prune_keys,
                                        now,
                                        now,
                                        &stakes,
                                    )
                                    .unwrap()
                            })
                            .unwrap();
                    }
                }
                (bytes, delivered, num_msgs, pruned)
            })
            .collect();

        for (b, d, m, p) in transfered {
            bytes += b;
            delivered += d;
            num_msgs += m;

            for (from, to) in p {
                let from_stake = stakes.get(&from).unwrap();
                if network.connections_pruned.insert((from, to)) {
                    prunes += 1;
                    stake_pruned += *from_stake;
                }
            }
        }
        if now % CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS == 0 && now > 0 {
            network_values.par_iter().for_each(|node| {
                node.gossip.refresh_push_active_set(
                    &node.keypair,
                    0,               // shred version
                    &HashMap::new(), // stakes
                    None,            // gossip validators
                    &node.ping_cache,
                    &mut Vec::new(), // pings
                    &SocketAddrSpace::Unspecified,
                );
            });
        }
        total = network_values
            .par_iter()
            .map(|node| node.gossip.push.num_pending(&node.gossip.crds))
            .sum();
        trace!(
            "network_run_push_{num}: now: {now} queue: {total} bytes: {bytes} num_msgs: \
             {num_msgs} prunes: {prunes} stake_pruned: {stake_pruned} delivered: {delivered}"
        );
    }

    network.stake_pruned += stake_pruned;
    (total, bytes)
}

fn network_run_pull(
    thread_pool: &ThreadPool,
    network: &Network,
    start: usize,
    end: usize,
    max_convergance: f64,
) -> (f64, usize) {
    let mut bytes: usize = 0;
    let mut msgs: usize = 0;
    let mut overhead: usize = 0;
    let mut convergance = 0f64;
    let num = network.len();
    let network_values: Vec<Node> = network.values().cloned().collect();
    let stakes = stakes(network);
    for node in &network_values {
        let mut ping_cache = node.ping_cache.lock().unwrap();
        for other in &network_values {
            if node.keypair.pubkey() != other.keypair.pubkey() {
                ping_cache.mock_pong(
                    other.keypair.pubkey(),
                    other.contact_info.gossip().unwrap(),
                    Instant::now(),
                );
            }
        }
    }
    let nodes: HashMap<SocketAddr, ContactInfo> = network
        .nodes
        .values()
        .map(|node| {
            let node = &node.contact_info;
            (node.gossip().unwrap(), node.clone())
        })
        .collect();
    for t in start..end {
        let now = t as u64 * 100;
        let requests: Vec<_> = {
            network_values
                .par_iter()
                .flat_map_iter(|from| {
                    let mut pings = Vec::new();
                    let requests = from
                        .gossip
                        .new_pull_request(
                            thread_pool,
                            from.keypair.deref(),
                            0, // shred version.
                            now,
                            None,
                            &HashMap::new(),
                            992, // max_bloom_filter_bytes
                            from.ping_cache.deref(),
                            &mut pings,
                            &SocketAddrSpace::Unspecified,
                        )
                        .map(|requests| {
                            requests
                                .into_group_map()
                                .into_iter()
                                .map(|(addr, filters)| {
                                    (nodes.get(&addr).cloned().unwrap(), filters)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let from_pubkey = from.keypair.pubkey();
                    let label = CrdsValueLabel::ContactInfo(from_pubkey);
                    let gossip_crds = from.gossip.crds.read().unwrap();
                    let self_info = gossip_crds.get::<&CrdsValue>(&label).unwrap().clone();
                    requests
                        .into_iter()
                        .map(move |(peer, filters)| (*peer.pubkey(), filters, self_info.clone()))
                })
                .collect()
        };
        let transfered: Vec<_> = requests
            .into_iter()
            .map(|(to, filters, caller_info)| {
                let mut bytes: usize = 0;
                let mut msgs: usize = 0;
                let mut overhead: usize = 0;
                let from = caller_info.label().pubkey();
                bytes += filters.iter().map(|f| f.filter.keys.len()).sum::<usize>();
                bytes += filters
                    .iter()
                    .map(|f| f.filter.bits.len() as usize / 8)
                    .sum::<usize>();
                bytes += caller_info.bincode_serialized_size();
                let requests: Vec<_> = filters
                    .into_iter()
                    .map(|filter| PullRequest {
                        pubkey: from,
                        addr: SocketAddr::from(([0; 4], 0)),
                        wallclock: now,
                        filter,
                    })
                    .collect();
                let rsp: Vec<_> = network
                    .get(&to)
                    .map(|node| {
                        node.gossip
                            .generate_pull_responses(
                                thread_pool,
                                &requests,
                                usize::MAX, // output_size_limit
                                now,
                                |_| true, // should_retain_crds_value
                                0,        // network shred version
                                &GossipStats::default(),
                            )
                            .into_iter()
                            .flatten()
                            .collect()
                    })
                    .unwrap();
                // 8 bytes for encoding the length of the vector.
                bytes += 8 + rsp
                    .iter()
                    .map(CrdsValue::bincode_serialized_size)
                    .sum::<usize>();
                msgs += rsp.len();
                if let Some(node) = network.get(&from) {
                    let mut stats = ProcessPullStats::default();
                    let timeouts = CrdsTimeouts::new(
                        node.keypair.pubkey(),
                        CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS, // default_timeout
                        Duration::from_secs(48 * 3600),   // epoch_duration
                        &stakes,
                    );
                    let (vers, vers_expired_timeout, failed_inserts) = node
                        .gossip
                        .filter_pull_responses(&timeouts, rsp, now, &mut stats);
                    node.gossip.process_pull_responses(
                        vers,
                        vers_expired_timeout,
                        failed_inserts,
                        now,
                        &mut stats,
                    );
                    overhead += stats.failed_insert;
                    overhead += stats.failed_timeout;
                }
                (bytes, msgs, overhead)
            })
            .collect();
        for (b, m, o) in transfered {
            bytes += b;
            msgs += m;
            overhead += o;
        }
        let total: usize = network_values
            .par_iter()
            .map(|v| v.gossip.crds.read().unwrap().len())
            .sum();
        convergance = total as f64 / ((num * num) as f64);
        if convergance > max_convergance {
            break;
        }
        trace!(
            "network_run_pull_{num}: now: {now} connections: {total} convergance: {convergance} \
             bytes: {bytes} msgs: {msgs} overhead: {overhead}"
        );
    }
    (convergance, bytes)
}

fn build_gossip_thread_pool() -> ThreadPool {
    ThreadPoolBuilder::new()
        .num_threads(get_thread_count().min(2))
        .thread_name(|i| format!("gossipTest{i:02}"))
        .build()
        .unwrap()
}

fn new_ping_cache() -> Mutex<PingCache> {
    let ping_cache = PingCache::new(
        &mut rand::thread_rng(),
        Instant::now(),
        Duration::from_secs(20 * 60),      // ttl
        Duration::from_secs(20 * 60) / 64, // rate_limit_delay
        2048,                              // capacity
    );
    Mutex::new(ping_cache)
}

#[test]
#[serial]
fn test_star_network_pull_50() {
    let network = star_network_create(50);
    let thread_pool = build_gossip_thread_pool();
    network_simulator_pull_only(&thread_pool, &network);
}
#[test]
#[serial]
fn test_star_network_pull_100() {
    let network = star_network_create(100);
    let thread_pool = build_gossip_thread_pool();
    network_simulator_pull_only(&thread_pool, &network);
}
#[test]
#[serial]
fn test_star_network_push_star_200() {
    let mut network = star_network_create(200);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}
#[ignore]
#[test]
fn test_star_network_push_rstar_200() {
    let mut network = rstar_network_create(200);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}
#[test]
#[serial]
fn test_star_network_push_ring_200() {
    let mut network = ring_network_create(200);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}

// With the new pruning logic, this test is no longer valid and can be deleted.
// Ignoring it for now until the pruning code is stable.
#[test]
#[ignore]
#[serial]
fn test_connected_staked_network() {
    solana_logger::setup();
    let thread_pool = build_gossip_thread_pool();
    let stakes = [
        [1000; 2].to_vec(),
        [100; 3].to_vec(),
        [10; 5].to_vec(),
        [1; 15].to_vec(),
    ]
    .concat();
    let mut network = connected_staked_network_create(&stakes);
    network_simulator(&thread_pool, &mut network, 1.0);

    let stake_sum: u64 = stakes.iter().sum();
    let avg_stake: u64 = stake_sum / stakes.len() as u64;
    let avg_stake_pruned = network.stake_pruned / network.connections_pruned.len() as u64;
    trace!(
        "connected staked networks, connections_pruned: {}, avg_stake: {}, avg_stake_pruned: {}",
        network.connections_pruned.len(),
        avg_stake,
        avg_stake_pruned
    );
    assert!(
        avg_stake_pruned < avg_stake,
        "network should prune lower stakes more often"
    )
}
#[test]
#[ignore]
fn test_star_network_large_pull() {
    solana_logger::setup();
    let network = star_network_create(2000);
    let thread_pool = build_gossip_thread_pool();
    network_simulator_pull_only(&thread_pool, &network);
}
#[test]
#[ignore]
fn test_rstar_network_large_push() {
    solana_logger::setup();
    let mut network = rstar_network_create(4000);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}
#[test]
#[ignore]
fn test_ring_network_large_push() {
    solana_logger::setup();
    let mut network = ring_network_create(4001);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}
#[test]
#[ignore]
fn test_star_network_large_push() {
    solana_logger::setup();
    let mut network = star_network_create(4002);
    let thread_pool = build_gossip_thread_pool();
    network_simulator(&thread_pool, &mut network, 0.9);
}
#[test]
fn test_prune_errors() {
    let crds_gossip = CrdsGossip::default();
    let keypair = Keypair::new();
    let id = keypair.pubkey();
    let ci = ContactInfo::new_localhost(&Pubkey::from([1; 32]), 0);
    let prune_pubkey = Pubkey::from([2; 32]);
    crds_gossip
        .crds
        .write()
        .unwrap()
        .insert(
            CrdsValue::new(CrdsData::from(&ci), &Keypair::new()),
            0,
            GossipRoute::LocalMessage,
        )
        .unwrap();
    let ping_cache = new_ping_cache();
    crds_gossip.refresh_push_active_set(
        &keypair,
        0,               // shred version
        &HashMap::new(), // stakes
        None,            // gossip validators
        &ping_cache,
        &mut Vec::new(), // pings
        &SocketAddrSpace::Unspecified,
    );
    let now = timestamp();
    let stakes = HashMap::<Pubkey, u64>::default();
    //incorrect dest
    let mut res = crds_gossip.process_prune_msg(
        &id,                                      // self_pubkey
        ci.pubkey(),                              // peer
        &Pubkey::from(hash(&[1; 32]).to_bytes()), // destination
        &[prune_pubkey],                          // origins
        now,
        now,
        &stakes,
    );
    assert_eq!(res.err(), Some(CrdsGossipError::BadPruneDestination));
    //correct dest
    res = crds_gossip.process_prune_msg(
        &id,             // self_pubkey
        ci.pubkey(),     // peer
        &id,             // destination
        &[prune_pubkey], // origins
        now,
        now,
        &stakes,
    );
    res.unwrap();
    //test timeout
    let timeout = now + crds_gossip.push.prune_timeout * 2;
    res = crds_gossip.process_prune_msg(
        &id,             // self_pubkey
        ci.pubkey(),     // peer
        &id,             // destination
        &[prune_pubkey], // origins
        now,
        timeout,
        &stakes,
    );
    assert_eq!(res.err(), Some(CrdsGossipError::PruneMessageTimeout));
}
