use {
    crossbeam_channel::{RecvError, TrySendError},
    log::info,
    solana_keypair::{Keypair, Signer},
    solana_metrics::datapoint_info,
    solana_perf::packet::PacketBatch,
    solana_pubkey::Pubkey,
    solana_streamer::{
        nonblocking::swqos::SwQosConfig,
        quic::{EndpointKeyUpdater, QuicStreamerConfig, QuicVariant, SpawnServerResult},
        streamer::StakedNodes,
    },
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        num::Saturating,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
    tokio_util::sync::CancellationToken,
};

const MAX_STAKED_CONNECTIONS: usize = 256;

// const STAKED_NODES_UPDATE_INTERVAL: Duration = Duration::from_secs(900); // 15 minutes
// const POOL_KEY: Pubkey = pubkey!("EJi4Rj2u1VXiLpKtaqeQh3w4XxAGLFqnAG1jCorSvVmg");

const DEFAULT_STAKE: u64 = 100_000_000_000;

pub struct P3Quic {
    exit: Arc<AtomicBool>,
    quic_server_regular: std::thread::JoinHandle<()>,
    quic_server_mev: std::thread::JoinHandle<()>,

    reg_packet_rx: crossbeam_channel::Receiver<PacketBatch>,
    mev_packet_rx: crossbeam_channel::Receiver<PacketBatch>,
    normal_packet_tx: crossbeam_channel::Sender<PacketBatch>,
    mev_packet_tx: crossbeam_channel::Sender<PacketBatch>,

    metrics: P3Metrics,
    metrics_creation: Instant,
}

impl P3Quic {
    pub fn spawn(
        cancel: CancellationToken,
        exit: Arc<AtomicBool>,
        normal_p3_packet_tx: crossbeam_channel::Sender<PacketBatch>,
        mev_p3_packet_tx: crossbeam_channel::Sender<PacketBatch>,
        keypair: &Keypair,
        (p3_socket, p3_mev_socket): (SocketAddr, SocketAddr),
    ) -> (std::thread::JoinHandle<()>, [Arc<EndpointKeyUpdater>; 2]) {
        // Bind the P3 QUIC UDP socket.
        let socket_regular = UdpSocket::bind(p3_socket).unwrap();
        let socket_mev = UdpSocket::bind(p3_mev_socket).unwrap();

        // Setup initial staked nodes (empty).
        let stakes = Self::update_stakes(
            vec![(keypair.pubkey().to_string().as_str(), DEFAULT_STAKE)],
            true,
        );
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(stakes, HashMap::default())));

        // TODO: Would be ideal to reduce the number of threads spawned by tokio
        // in streamer (i.e. make it an argument).

        // Spawn the P3 QUIC server (regular).
        let (reg_packet_tx, reg_packet_rx) = crossbeam_channel::unbounded();
        let SpawnServerResult {
            endpoints: _,
            thread: quic_server_regular,
            key_updater: key_updater_regular,
        } = solana_streamer::quic::spawn_paladin_server_with_cancel(
            "p3Quic-streamer",
            "p3_quic",
            [socket_regular],
            keypair,
            // NB: Packets are verified using the usual TPU lane.
            reg_packet_tx,
            staked_nodes.clone(),
            QuicStreamerConfig {
                max_staked_connections: MAX_STAKED_CONNECTIONS,
                max_unstaked_connections: 0,
                // NB: This must be 1 second for the `P3_RATE_LIMIT` const to be valid.
                stream_throttling_interval_ms: 1000,
                ..Default::default()
            },
            SwQosConfig::default(),
            cancel.clone(),
            QuicVariant::P3,
        )
        .unwrap();

        // Spawn the P3 QUIC server (mev).
        let (mev_packet_tx, mev_packet_rx) = crossbeam_channel::unbounded();
        let SpawnServerResult {
            endpoints: _,
            thread: quic_server_mev,
            key_updater: key_updater_mev,
        } = solana_streamer::quic::spawn_paladin_server_with_cancel(
            "p3Quic-streamer",
            "p3_quic",
            [socket_mev],
            keypair,
            // NB: Packets are verified using the usual TPU lane.
            mev_packet_tx,
            staked_nodes.clone(),
            QuicStreamerConfig {
                max_staked_connections: MAX_STAKED_CONNECTIONS,
                max_unstaked_connections: 0,
                // NB: This must be 1 second for the `P3_RATE_LIMIT` const to be valid.
                stream_throttling_interval_ms: 1000,
                ..Default::default()
            },
            SwQosConfig::default(),
            cancel.clone(),
            QuicVariant::Mev,
        )
        .unwrap();

        // Spawn the P3 management thread.
        let p3 = Self {
            exit: exit.clone(),
            quic_server_regular,
            quic_server_mev,

            reg_packet_rx,
            mev_packet_rx,
            normal_packet_tx: normal_p3_packet_tx,
            mev_packet_tx: mev_p3_packet_tx,

            metrics: P3Metrics::default(),
            metrics_creation: Instant::now(),
        };

        (
            std::thread::Builder::new()
                .name("P3Quic".to_owned())
                .spawn(move || p3.run())
                .unwrap(),
            [key_updater_regular, key_updater_mev],
        )
    }

    fn update_stakes(
        custom_stakes: Vec<(&str, u64)>,
        with_defaults: bool,
    ) -> Arc<HashMap<Pubkey, u64>> {
        let mut stakes: HashMap<Pubkey, u64> = HashMap::new();

        // Add defaults stakes if wanted
        if with_defaults {
            stakes.insert(
                Pubkey::from_str_const("3wWrxQNpmGRzaVYVCCGEVLV6GMHG4Vvzza4iT79atw5A"),
                DEFAULT_STAKE,
            );
            stakes.insert(
                Pubkey::from_str_const("E9Gr9GmYTB9eEYr44VMhfZh9LRVYzppD94UrcgQubTrG"),
                DEFAULT_STAKE,
            );
        }

        for (pubkey, amount) in custom_stakes {
            stakes.insert(Pubkey::from_str_const(pubkey), amount);
        }

        Arc::new(stakes)
    }

    fn run(mut self) {
        info!("Spawned P3Quic");

        let start = Instant::now();
        self.metrics.staked_nodes_us += start.elapsed().as_micros() as u64;

        while !self.exit.load(Ordering::Relaxed) {
            // Try to receive mev packets.
            crossbeam_channel::select_biased! {
                recv(self.mev_packet_rx) -> res => match res {
                    Ok(packets) => self.on_mev_packets(packets),
                    Err(RecvError) => break,
                },
                recv(self.reg_packet_rx) -> res => match res {
                    Ok(packets) => self.on_regular_packets(packets),
                    Err(RecvError) => break,
                }
            }

            // Check if we need to report metrics for the last interval.
            let now = Instant::now();
            let age = now - self.metrics_creation;
            if age > Duration::from_secs(1) {
                self.metrics.report(age.as_millis() as u64);
                self.metrics = P3Metrics::default();
                self.metrics_creation = now;
            }
        }

        self.quic_server_regular.join().unwrap();
        self.quic_server_mev.join().unwrap();
    }

    fn on_regular_packets(&mut self, mut packets: PacketBatch) {
        let len = packets.len() as u64;
        self.metrics.p3_forwarded += len;

        for mut packet in packets.iter_mut() {
            // NB: Unset the staked node flag to prevent forwarding.
            packet.meta_mut().set_from_staked_node(false);
        }

        // Forward for verification & inclusion.
        if let Err(TrySendError::Full(_)) = self.normal_packet_tx.try_send(packets) {
            self.metrics.p3_dropped += len;
        }
    }

    fn on_mev_packets(&mut self, mut packets: PacketBatch) {
        let len = packets.len() as u64;
        self.metrics.mev_forwarded += len;

        for mut packet in packets.iter_mut() {
            // NB: Unset the staked node flag to prevent forwarding.
            packet.meta_mut().set_from_staked_node(false);
        }

        // Forward for verification & inclusion.
        if let Err(TrySendError::Full(_)) = self.mev_packet_tx.try_send(packets) {
            self.metrics.mev_dropped += len;
        }
    }
}

#[derive(Default, PartialEq, Eq)]
struct P3Metrics {
    /// Number of regular packets forwarded.
    p3_forwarded: Saturating<u64>,
    /// Number of regular packets dropped.
    p3_dropped: Saturating<u64>,
    /// Number of mev packets forwarded.
    mev_forwarded: Saturating<u64>,
    /// Number of mev packets dropped.
    mev_dropped: Saturating<u64>,
    /// Time taken to update staked nodes.
    staked_nodes_us: Saturating<u64>,
}

impl P3Metrics {
    fn report(&self, age_ms: u64) {
        // Suppress logs if there are no recorded metrics.
        if self == &P3Metrics::default() {
            return;
        }

        let &Self {
            p3_forwarded: Saturating(p3_forwarded),
            p3_dropped: Saturating(p3_dropped),
            mev_forwarded: Saturating(mev_forwarded),
            mev_dropped: Saturating(mev_dropped),
            staked_nodes_us: Saturating(staked_nodes_us),
        } = self;

        datapoint_info!(
            "p3_quic",
            ("age_ms", age_ms as i64, i64),
            ("p3_packets_forwarded", p3_forwarded as i64, i64),
            ("p3_packets_dropped", p3_dropped as i64, i64),
            ("mev_packets_forwarded", mev_forwarded as i64, i64),
            ("mev_packets_dropped", mev_dropped as i64, i64),
            ("staked_nodes_us", staked_nodes_us as i64, i64),
        );
    }
}
