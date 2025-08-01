use {
    crate::{
        nonblocking::quic::{ALPN_TPU_PROTOCOL_ID, DEFAULT_WAIT_FOR_CHUNK_TIMEOUT},
        streamer::StakedNodes,
    },
    crossbeam_channel::Sender,
    pem::Pem,
    quinn::{
        crypto::rustls::{NoInitialCipherSuite, QuicServerConfig},
        Endpoint, IdleTimeout, ServerConfig,
    },
    rustls::KeyLogFile,
    solana_keypair::Keypair,
    solana_packet::PACKET_DATA_SIZE,
    solana_perf::packet::PacketBatch,
    solana_quic_definitions::{
        NotifyKeyUpdate, QUIC_MAX_TIMEOUT, QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS,
    },
    solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder},
    std::{
        net::UdpSocket,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex, RwLock,
        },
        thread,
        time::Duration,
    },
    tokio::runtime::Runtime,
};

// allow multiple connections for NAT and any open/close overlap
pub const DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER: usize = 8;

pub const DEFAULT_MAX_STAKED_CONNECTIONS: usize = 2000;

pub const DEFAULT_MAX_UNSTAKED_CONNECTIONS: usize = 500;

/// Limit to 500K PPS
pub const DEFAULT_MAX_STREAMS_PER_MS: u64 = 500;

/// The new connections per minute from a particular IP address.
/// Heuristically set to the default maximum concurrent connections
/// per IP address. Might be adjusted later.
pub const DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE: u64 = 8;

// This will be adjusted and parameterized in follow-on PRs.
pub const DEFAULT_QUIC_ENDPOINTS: usize = 1;

pub const DEFAULT_TPU_COALESCE: Duration = Duration::from_millis(5);

pub fn default_num_tpu_transaction_forward_receive_threads() -> usize {
    num_cpus::get().min(16)
}

pub fn default_num_tpu_transaction_receive_threads() -> usize {
    num_cpus::get().min(8)
}

pub fn default_num_tpu_vote_transaction_receive_threads() -> usize {
    num_cpus::get().min(8)
}

pub struct SpawnServerResult {
    pub endpoints: Vec<Endpoint>,
    pub thread: thread::JoinHandle<()>,
    pub key_updater: Arc<EndpointKeyUpdater>,
}

/// Controls the the channel size for the PacketBatch coalesce
pub(crate) const DEFAULT_MAX_COALESCE_CHANNEL_SIZE: usize = 250_000;

/// Returns default server configuration along with its PEM certificate chain.
#[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
pub(crate) fn configure_server(
    identity_keypair: &Keypair,
) -> Result<(ServerConfig, String), QuicServerError> {
    let (cert, priv_key) = new_dummy_x509_certificate(identity_keypair);
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.as_ref().to_vec(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config =
        tls_server_config_builder().with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    server_tls_config.key_log = Arc::new(KeyLogFile::new());
    let quic_server_config = QuicServerConfig::try_from(server_tls_config)?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));
    let config = Arc::get_mut(&mut server_config.transport).unwrap();

    // QUIC_MAX_CONCURRENT_STREAMS doubled, which was found to improve reliability
    const MAX_CONCURRENT_UNI_STREAMS: u32 =
        (QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS.saturating_mul(2)) as u32;
    config.max_concurrent_uni_streams(MAX_CONCURRENT_UNI_STREAMS.into());
    config.stream_receive_window((PACKET_DATA_SIZE as u32).into());
    config.receive_window((PACKET_DATA_SIZE as u32).into());
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    const MAX_CONCURRENT_BIDI_STREAMS: u32 = 0;
    config.max_concurrent_bidi_streams(MAX_CONCURRENT_BIDI_STREAMS.into());
    config.datagram_receive_buffer_size(None);

    // Disable GSO. The server only accepts inbound unidirectional streams initiated by clients,
    // which means that reply data never exceeds one MTU. By disabling GSO, we make
    // quinn_proto::Connection::poll_transmit allocate only 1 MTU vs 10 * MTU for _each_ transmit.
    // See https://github.com/anza-xyz/agave/pull/1647.
    config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

pub fn rt(name: String, num_threads: NonZeroUsize) -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name(name)
        .worker_threads(num_threads.get())
        .enable_all()
        .build()
        .unwrap()
}

#[derive(thiserror::Error, Debug)]
pub enum QuicServerError {
    #[error("Endpoint creation failed: {0}")]
    EndpointFailed(std::io::Error),
    #[error("TLS error: {0}")]
    TlsError(#[from] rustls::Error),
    #[error("No initial cipher suite")]
    NoInitialCipherSuite(#[from] NoInitialCipherSuite),
}

pub struct EndpointKeyUpdater {
    endpoints: Vec<Endpoint>,
}

impl NotifyKeyUpdate for EndpointKeyUpdater {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let (config, _) = configure_server(key)?;
        for endpoint in &self.endpoints {
            endpoint.set_server_config(Some(config.clone()));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct StreamerStats {
    pub(crate) total_connections: AtomicUsize,
    pub(crate) total_new_connections: AtomicUsize,
    pub(crate) total_streams: AtomicUsize,
    pub(crate) total_new_streams: AtomicUsize,
    pub(crate) invalid_stream_size: AtomicUsize,
    pub(crate) total_packets_allocated: AtomicUsize,
    pub(crate) total_packet_batches_allocated: AtomicUsize,
    pub(crate) total_chunks_received: AtomicUsize,
    pub(crate) total_staked_chunks_received: AtomicUsize,
    pub(crate) total_unstaked_chunks_received: AtomicUsize,
    pub(crate) total_packet_batch_send_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_batcher_send_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_batcher_send_full_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_batcher_send_disconnected_err: AtomicUsize,
    pub(crate) total_packet_batches_sent: AtomicUsize,
    pub(crate) total_packet_batches_none: AtomicUsize,
    pub(crate) total_packets_sent_for_batching: AtomicUsize,
    pub(crate) total_bytes_sent_for_batching: AtomicUsize,
    pub(crate) total_chunks_sent_for_batching: AtomicUsize,
    pub(crate) total_packets_sent_to_consumer: AtomicUsize,
    pub(crate) total_bytes_sent_to_consumer: AtomicUsize,
    pub(crate) total_chunks_processed_by_batcher: AtomicUsize,
    pub(crate) total_stream_read_errors: AtomicUsize,
    pub(crate) total_stream_read_timeouts: AtomicUsize,
    pub(crate) num_evictions: AtomicUsize,
    pub(crate) connection_added_from_staked_peer: AtomicUsize,
    pub(crate) connection_added_from_unstaked_peer: AtomicUsize,
    pub(crate) connection_add_failed: AtomicUsize,
    pub(crate) connection_add_failed_invalid_stream_count: AtomicUsize,
    pub(crate) connection_add_failed_staked_node: AtomicUsize,
    pub(crate) connection_add_failed_unstaked_node: AtomicUsize,
    pub(crate) connection_add_failed_on_pruning: AtomicUsize,
    pub(crate) connection_setup_timeout: AtomicUsize,
    pub(crate) connection_setup_error: AtomicUsize,
    pub(crate) connection_setup_error_closed: AtomicUsize,
    pub(crate) connection_setup_error_timed_out: AtomicUsize,
    pub(crate) connection_setup_error_transport: AtomicUsize,
    pub(crate) connection_setup_error_app_closed: AtomicUsize,
    pub(crate) connection_setup_error_reset: AtomicUsize,
    pub(crate) connection_setup_error_locally_closed: AtomicUsize,
    pub(crate) connection_removed: AtomicUsize,
    pub(crate) connection_remove_failed: AtomicUsize,
    // Number of connections to the endpoint exceeding the allowed limit
    // regardless of the source IP address.
    pub(crate) connection_rate_limited_across_all: AtomicUsize,
    // Per IP rate-limiting is triggered each time when there are too many connections
    // opened from a particular IP address.
    pub(crate) connection_rate_limited_per_ipaddr: AtomicUsize,
    pub(crate) throttled_streams: AtomicUsize,
    pub(crate) stream_load_ema: AtomicUsize,
    pub(crate) stream_load_ema_overflow: AtomicUsize,
    pub(crate) stream_load_capacity_overflow: AtomicUsize,
    pub(crate) process_sampled_packets_us_hist: Mutex<histogram::Histogram>,
    pub(crate) perf_track_overhead_us: AtomicU64,
    pub(crate) total_staked_packets_sent_for_batching: AtomicUsize,
    pub(crate) total_unstaked_packets_sent_for_batching: AtomicUsize,
    pub(crate) throttled_staked_streams: AtomicUsize,
    pub(crate) throttled_unstaked_streams: AtomicUsize,
    pub(crate) connection_rate_limiter_length: AtomicUsize,
    // All connections in various states such as Incoming, Connecting, Connection
    pub(crate) open_connections: AtomicUsize,
    pub(crate) refused_connections_too_many_open_connections: AtomicUsize,
    pub(crate) outstanding_incoming_connection_attempts: AtomicUsize,
    pub(crate) total_incoming_connection_attempts: AtomicUsize,
    pub(crate) quic_endpoints_count: AtomicUsize,
}

impl StreamerStats {
    pub fn report(&self, name: &'static str) {
        let process_sampled_packets_us_hist = {
            let mut metrics = self.process_sampled_packets_us_hist.lock().unwrap();
            let process_sampled_packets_us_hist = metrics.clone();
            metrics.clear();
            process_sampled_packets_us_hist
        };

        datapoint_info!(
            name,
            (
                "active_connections",
                self.total_connections.load(Ordering::Relaxed),
                i64
            ),
            (
                "active_streams",
                self.total_streams.load(Ordering::Relaxed),
                i64
            ),
            (
                "new_connections",
                self.total_new_connections.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "new_streams",
                self.total_new_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "evictions",
                self.num_evictions.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_added_from_staked_peer",
                self.connection_added_from_staked_peer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_added_from_unstaked_peer",
                self.connection_added_from_unstaked_peer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed",
                self.connection_add_failed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_invalid_stream_count",
                self.connection_add_failed_invalid_stream_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_staked_node",
                self.connection_add_failed_staked_node
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_unstaked_node",
                self.connection_add_failed_unstaked_node
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_on_pruning",
                self.connection_add_failed_on_pruning
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_removed",
                self.connection_removed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_remove_failed",
                self.connection_remove_failed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_timeout",
                self.connection_setup_timeout.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error",
                self.connection_setup_error.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_timed_out",
                self.connection_setup_error_timed_out
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_closed",
                self.connection_setup_error_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_transport",
                self.connection_setup_error_transport
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_app_closed",
                self.connection_setup_error_app_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_reset",
                self.connection_setup_error_reset.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_locally_closed",
                self.connection_setup_error_locally_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_rate_limited_across_all",
                self.connection_rate_limited_across_all
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_rate_limited_per_ipaddr",
                self.connection_rate_limited_per_ipaddr
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_stream_size",
                self.invalid_stream_size.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_allocated",
                self.total_packets_allocated.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batches_allocated",
                self.total_packet_batches_allocated
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_sent_for_batching",
                self.total_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "staked_packets_sent_for_batching",
                self.total_staked_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "unstaked_packets_sent_for_batching",
                self.total_unstaked_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_sent_for_batching",
                self.total_bytes_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "chunks_sent_for_batching",
                self.total_chunks_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_sent_to_consumer",
                self.total_packets_sent_to_consumer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_sent_to_consumer",
                self.total_bytes_sent_to_consumer.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "chunks_processed_by_batcher",
                self.total_chunks_processed_by_batcher
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "chunks_received",
                self.total_chunks_received.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "staked_chunks_received",
                self.total_staked_chunks_received.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "unstaked_chunks_received",
                self.total_unstaked_chunks_received
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batch_send_error",
                self.total_packet_batch_send_err.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "handle_chunk_to_packet_batcher_send_error",
                self.total_handle_chunk_to_packet_batcher_send_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "handle_chunk_to_packet_batcher_send_full_err",
                self.total_handle_chunk_to_packet_batcher_send_full_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "handle_chunk_to_packet_batcher_send_disconnected_err",
                self.total_handle_chunk_to_packet_batcher_send_disconnected_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batches_sent",
                self.total_packet_batches_sent.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batch_empty",
                self.total_packet_batches_none.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_read_errors",
                self.total_stream_read_errors.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_read_timeouts",
                self.total_stream_read_timeouts.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "throttled_streams",
                self.throttled_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_ema",
                self.stream_load_ema.load(Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_ema_overflow",
                self.stream_load_ema_overflow.load(Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_capacity_overflow",
                self.stream_load_capacity_overflow.load(Ordering::Relaxed),
                i64
            ),
            (
                "throttled_unstaked_streams",
                self.throttled_unstaked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "throttled_staked_streams",
                self.throttled_staked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "process_sampled_packets_us_90pct",
                process_sampled_packets_us_hist
                    .percentile(90.0)
                    .unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_min",
                process_sampled_packets_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_max",
                process_sampled_packets_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_mean",
                process_sampled_packets_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_count",
                process_sampled_packets_us_hist.entries(),
                i64
            ),
            (
                "perf_track_overhead_us",
                self.perf_track_overhead_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_rate_limiter_length",
                self.connection_rate_limiter_length.load(Ordering::Relaxed),
                i64
            ),
            (
                "outstanding_incoming_connection_attempts",
                self.outstanding_incoming_connection_attempts
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "total_incoming_connection_attempts",
                self.total_incoming_connection_attempts
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "quic_endpoints_count",
                self.quic_endpoints_count.load(Ordering::Relaxed),
                i64
            ),
            (
                "open_connections",
                self.open_connections.load(Ordering::Relaxed),
                i64
            ),
            (
                "refused_connections_too_many_open_connections",
                self.refused_connections_too_many_open_connections
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}

pub fn spawn_server(
    thread_name: &'static str,
    metrics_name: &'static str,
    socket: UdpSocket,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    quic_server_params: QuicServerParams,
) -> Result<SpawnServerResult, QuicServerError> {
    spawn_server_multi(
        thread_name,
        metrics_name,
        vec![socket],
        keypair,
        packet_sender,
        exit,
        staked_nodes,
        quic_server_params,
    )
}

#[derive(Clone)]
pub struct QuicServerParams {
    pub max_connections_per_peer: usize,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_streams_per_ms: u64,
    pub max_connections_per_ipaddr_per_min: u64,
    pub wait_for_chunk_timeout: Duration,
    pub coalesce: Duration,
    pub coalesce_channel_size: usize,
    pub num_threads: NonZeroUsize,
}

impl Default for QuicServerParams {
    fn default() -> Self {
        QuicServerParams {
            max_connections_per_peer: 1,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS,
            max_connections_per_ipaddr_per_min: DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE,
            wait_for_chunk_timeout: DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            coalesce: DEFAULT_TPU_COALESCE,
            coalesce_channel_size: DEFAULT_MAX_COALESCE_CHANNEL_SIZE,
            num_threads: NonZeroUsize::new(num_cpus::get().min(1)).expect("1 is non-zero"),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl QuicServerParams {
    pub const DEFAULT_NUM_SERVER_THREADS_FOR_TEST: NonZeroUsize = NonZeroUsize::new(8).unwrap();

    pub fn default_for_tests() -> Self {
        // Shrink the channel size to avoid a massive allocation for tests
        Self {
            coalesce_channel_size: 100_000,
            num_threads: Self::DEFAULT_NUM_SERVER_THREADS_FOR_TEST,
            ..Self::default()
        }
    }
}

pub fn spawn_server_multi(
    thread_name: &'static str,
    metrics_name: &'static str,
    sockets: Vec<UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    quic_server_params: QuicServerParams,
) -> Result<SpawnServerResult, QuicServerError> {
    let runtime = rt(format!("{thread_name}Rt"), quic_server_params.num_threads);
    let result = {
        let _guard = runtime.enter();
        crate::nonblocking::quic::spawn_server_multi(
            metrics_name,
            sockets,
            keypair,
            packet_sender,
            exit,
            staked_nodes,
            quic_server_params,
        )
    }?;
    let handle = thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = runtime.block_on(result.thread) {
                warn!("error from runtime.block_on: {e:?}");
            }
        })
        .unwrap();
    let updater = EndpointKeyUpdater {
        endpoints: result.endpoints.clone(),
    };
    Ok(SpawnServerResult {
        endpoints: result.endpoints,
        thread: handle,
        key_updater: Arc::new(updater),
    })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::nonblocking::{quic::test::*, testing_utilities::check_multiple_streams},
        crossbeam_channel::unbounded,
        solana_net_utils::sockets::bind_to_localhost_unique,
        std::net::SocketAddr,
    };

    fn rt_for_test() -> Runtime {
        rt(
            "solQuicTestRt".to_string(),
            QuicServerParams::DEFAULT_NUM_SERVER_THREADS_FOR_TEST,
        )
    }

    fn setup_quic_server() -> (
        std::thread::JoinHandle<()>,
        Arc<AtomicBool>,
        crossbeam_channel::Receiver<PacketBatch>,
        SocketAddr,
    ) {
        let s = bind_to_localhost_unique().expect("should bind");
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_server(
            "solQuicTest",
            "quic_streamer_test",
            s,
            &keypair,
            sender,
            exit.clone(),
            staked_nodes,
            QuicServerParams::default_for_tests(),
        )
        .unwrap();
        (t, exit, receiver, server_address)
    }

    #[test]
    fn test_quic_server_exit() {
        let (t, exit, _receiver, _server_address) = setup_quic_server();
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_timeout() {
        solana_logger::setup();
        let (t, exit, receiver, server_address) = setup_quic_server();
        let runtime = rt_for_test();
        runtime.block_on(check_timeout(receiver, server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_block_multiple_connections() {
        solana_logger::setup();
        let (t, exit, _receiver, server_address) = setup_quic_server();

        let runtime = rt_for_test();
        runtime.block_on(check_block_multiple_connections(server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_streams() {
        solana_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_server(
            "solQuicTest",
            "quic_streamer_test",
            s,
            &keypair,
            sender,
            exit.clone(),
            staked_nodes,
            QuicServerParams {
                max_connections_per_peer: 2,
                ..QuicServerParams::default_for_tests()
            },
        )
        .unwrap();

        let runtime = rt_for_test();
        runtime.block_on(check_multiple_streams(receiver, server_address, None));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_writes() {
        solana_logger::setup();
        let (t, exit, receiver, server_address) = setup_quic_server();

        let runtime = rt_for_test();
        runtime.block_on(check_multiple_writes(receiver, server_address, None));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_unstaked_node_connect_failure() {
        solana_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, _) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_server(
            "solQuicTest",
            "quic_streamer_test",
            s,
            &keypair,
            sender,
            exit.clone(),
            staked_nodes,
            QuicServerParams {
                max_unstaked_connections: 0,
                ..QuicServerParams::default_for_tests()
            },
        )
        .unwrap();

        let runtime = rt_for_test();
        runtime.block_on(check_unstaked_node_connect_failure(server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }
}
