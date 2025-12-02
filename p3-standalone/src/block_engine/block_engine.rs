// https://github.com/jito-foundation/jito-relayer/blob/master/relayer/src/relayer.rs
use {
    crate::{
        convert::{packet_to_proto_packet, proto_packets_to_batch, proto_packets_to_bundle},
        rpc::load_balancer::LoadBalancer,
    },
    crossbeam_channel::{bounded, Receiver, RecvError, Sender},
    jito_protos::proto::{
        block_engine::{
            block_engine_validator_server::BlockEngineValidator, BlockBuilderFeeInfoRequest,
            BlockBuilderFeeInfoResponse, GetBlockEngineEndpointRequest,
            GetBlockEngineEndpointResponse, SubscribeBundlesRequest, SubscribeBundlesResponse,
            SubscribePacketsRequest, SubscribePacketsResponse,
        },
        shared::Header,
    },
    log::*,
    prost_types::Timestamp,
    solana_perf::packet::{PacketBatch, PacketRef},
    solana_sdk::{
        packet::{Packet, PACKET_DATA_SIZE},
        pubkey::Pubkey,
    },
    std::{
        collections::{hash_map::Entry, HashMap},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, JoinHandle},
        time::SystemTime,
    },
    thiserror::Error,
    tokio::sync::mpsc::{channel, error::TrySendError, Sender as TokioSender},
    tokio_stream::wrappers::ReceiverStream,
    tonic::{Request, Response, Status},
};

pub enum Subscription {
    BlockEnginePacketSubscription {
        pubkey: Pubkey,
        sender: TokioSender<Result<SubscribePacketsResponse, Status>>,
    },
    BlockEngineBundleSubscription {
        pubkey: Pubkey,
        sender: TokioSender<Result<SubscribeBundlesResponse, Status>>,
    },
}

#[derive(Error, Debug)]
pub enum BlockEngineError {
    #[error("shutdown")]
    Shutdown(#[from] RecvError),
}

pub type BlockEngineResult<T> = Result<T, BlockEngineError>;

type PacketSubscriptions =
    Arc<RwLock<HashMap<Pubkey, TokioSender<Result<SubscribePacketsResponse, Status>>>>>;
type BundleSubscriptions =
    Arc<RwLock<HashMap<Pubkey, TokioSender<Result<SubscribeBundlesResponse, Status>>>>>;

pub struct BlockEngineImpl {
    subscription_sender: Sender<Subscription>,
    identity_pubkey: Pubkey,
}

impl BlockEngineImpl {
    pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 50_000;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        packet_receiver: Receiver<PacketBatch>,
        mev_packet_receiver: Receiver<PacketBatch>,
        identity_pubkey: Pubkey,
        exit: Arc<AtomicBool>,
    ) -> (Self, JoinHandle<()>) {
        let (subscription_sender, subscription_receiver) =
            bounded(LoadBalancer::SLOT_QUEUE_CAPACITY);

        let packet_subscriptions = Arc::new(RwLock::new(HashMap::default()));
        let bundle_subscriptions = Arc::new(RwLock::new(HashMap::default()));

        let thread = {
            let packet_subscriptions = packet_subscriptions.clone();
            let bundle_subscriptions = bundle_subscriptions.clone();
            thread::Builder::new()
                .name("block_engine-event_loop_thread".to_string())
                .spawn(move || {
                    let res = Self::run_event_loop(
                        subscription_receiver,
                        packet_receiver,
                        mev_packet_receiver,
                        exit,
                        &packet_subscriptions,
                        &bundle_subscriptions,
                    );
                    warn!("BlockEngineImpl thread exited with result {res:?}")
                })
                .unwrap()
        };

        (
            Self {
                subscription_sender,
                identity_pubkey,
            },
            thread,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_event_loop(
        subscription_receiver: Receiver<Subscription>,
        packet_receiver: Receiver<PacketBatch>,
        mev_packet_receiver: Receiver<PacketBatch>,
        exit: Arc<AtomicBool>,
        packet_subscriptions: &PacketSubscriptions,
        bundle_subscriptions: &BundleSubscriptions,
    ) -> BlockEngineResult<()> {
        while !exit.load(Ordering::Relaxed) {
            crossbeam_channel::select! {
                recv(packet_receiver) -> maybe_packet_batches => {
                    let failed_forwards = Self::forward_regular_packets(maybe_packet_batches, packet_subscriptions)?;
                    Self::drop_connections(failed_forwards, packet_subscriptions);
                },
                recv(mev_packet_receiver) -> maybe_mev_packet_batches => {
                    let failed_forwards = Self::forward_mev_packets(maybe_mev_packet_batches, bundle_subscriptions)?;
                    Self::drop_connections(failed_forwards, bundle_subscriptions);
                }
                recv(subscription_receiver) -> maybe_subscription => {
                    Self::handle_subscription(maybe_subscription, packet_subscriptions, bundle_subscriptions)?;
                }
            }
        }
        Ok(())
    }

    fn drop_connections<T>(
        disconnected_pubkeys: Vec<Pubkey>,
        subscriptions: &Arc<RwLock<HashMap<Pubkey, TokioSender<Result<T, Status>>>>>,
    ) {
        let mut l_subscriptions = subscriptions.write().unwrap();
        for disconnected in disconnected_pubkeys {
            l_subscriptions.remove(&disconnected);
        }
    }

    fn forward_regular_packets(
        maybe_packet_batch: Result<PacketBatch, RecvError>,
        packet_subscriptions: &PacketSubscriptions,
    ) -> BlockEngineResult<Vec<Pubkey>> {
        let batch = maybe_packet_batch?;
        let proto_packets = batch.into_iter().fold(Vec::new(), |mut acc, pkt| {
            let pkt = match pkt {
                PacketRef::Packet(pkt) => pkt.clone(),
                PacketRef::Bytes(pkt) => {
                    let src = pkt.data(..).unwrap();
                    let mut dst = [0; PACKET_DATA_SIZE];
                    dst[..src.len()].copy_from_slice(src);
                    Packet::new(dst, pkt.meta().clone())
                }
            };
            acc.extend(packet_to_proto_packet(&pkt));
            acc
        });

        let now_ts = Timestamp::from(SystemTime::now());
        let packet_batch_proto = proto_packets_to_batch(proto_packets).unwrap_or_default();
        let packet_resp = {
            let batch_clone = packet_batch_proto.clone();
            let ts = now_ts.clone();
            move |_: &Pubkey| SubscribePacketsResponse {
                header: Some(Header {
                    ts: Some(ts.clone()),
                }),
                batch: Some(batch_clone.clone()),
            }
        };

        let mut failed = Vec::new();
        Self::broadcast_into(&mut failed, packet_subscriptions, packet_resp);
        Ok(failed)
    }

    fn forward_mev_packets(
        maybe_mev_batch: Result<PacketBatch, RecvError>,
        bundle_subscriptions: &BundleSubscriptions,
    ) -> BlockEngineResult<Vec<Pubkey>> {
        let batch = maybe_mev_batch?;
        // Convert all packets in the batch to proto packets.
        let mev_pkts = batch
            .into_iter()
            .map(|pkt| {
                let pkt = match pkt {
                    PacketRef::Packet(pkt) => pkt.clone(),
                    PacketRef::Bytes(pkt) => {
                        let src = pkt.data(..).unwrap();
                        let mut dst = [0; PACKET_DATA_SIZE];
                        dst[..src.len()].copy_from_slice(src);
                        Packet::new(dst, pkt.meta().clone())
                    }
                };
                packet_to_proto_packet(&pkt)
            })
            .flatten()
            .collect::<Vec<_>>();

        let now_ts = Timestamp::from(SystemTime::now());
        let bundle_proto =
            proto_packets_to_bundle(mev_pkts, now_ts, String::new()).unwrap_or_default();
        let bundle_resp = move |_: &Pubkey| SubscribeBundlesResponse {
            bundles: vec![bundle_proto.clone()],
        };
        let mut failed = Vec::new();
        Self::broadcast_into(&mut failed, bundle_subscriptions, bundle_resp);
        Ok(failed)
    }

    fn broadcast_into<T, F>(
        failed: &mut Vec<Pubkey>,
        subs: &Arc<RwLock<HashMap<Pubkey, TokioSender<Result<T, Status>>>>>,
        make: F,
    ) where
        F: Fn(&Pubkey) -> T,
    {
        let l = subs.read().unwrap();
        for (pk, tx) in l.iter() {
            match tx.try_send(Ok(make(pk))) {
                Ok(_) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => failed.push(*pk),
            }
        }
    }

    fn handle_subscription(
        maybe_subscription: Result<Subscription, RecvError>,
        packet_subscriptions: &PacketSubscriptions,
        bundle_subscriptions: &BundleSubscriptions,
    ) -> BlockEngineResult<()> {
        let subscription = maybe_subscription?;

        fn insert_subscription<T>(
            subscriptions: &Arc<RwLock<HashMap<Pubkey, TokioSender<Result<T, Status>>>>>,
            err_msg: &str,
            key: Pubkey,
            sender: TokioSender<Result<T, Status>>,
        ) {
            let mut map = subscriptions.write().unwrap();
            match map.entry(key) {
                Entry::Vacant(e) => {
                    e.insert(sender);
                }
                Entry::Occupied(mut e) => {
                    error!("{}", err_msg);
                    e.insert(sender);
                }
            }
        }

        match subscription {
            Subscription::BlockEnginePacketSubscription { pubkey, sender } => {
                let err = format!("already connected, dropping old connection: {:?}", pubkey);
                insert_subscription(packet_subscriptions, &err, pubkey, sender);
            }
            Subscription::BlockEngineBundleSubscription { pubkey, sender } => {
                let err = format!(
                    "already connected for bundles, dropping old connection: {:?}",
                    pubkey
                );
                insert_subscription(bundle_subscriptions, &err, pubkey, sender);
            }
        }

        Ok(())
    }
}

#[tonic::async_trait]
impl BlockEngineValidator for BlockEngineImpl {
    type SubscribePacketsStream = ReceiverStream<Result<SubscribePacketsResponse, Status>>;

    /// Validator calls this to subscribe to packets
    async fn subscribe_packets(
        &self,
        request: Request<SubscribePacketsRequest>,
    ) -> Result<Response<Self::SubscribePacketsStream>, Status> {
        let pubkey: &Pubkey = request
            .extensions()
            .get()
            .ok_or_else(|| Status::internal("internal error fetching public key"))?;

        let (sender, receiver) = channel(BlockEngineImpl::SUBSCRIBER_QUEUE_CAPACITY);
        self.subscription_sender
            .send(Subscription::BlockEnginePacketSubscription {
                pubkey: *pubkey,
                sender,
            })
            .map_err(|_| Status::internal("internal error adding subscription"))?;

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    type SubscribeBundlesStream = ReceiverStream<Result<SubscribeBundlesResponse, Status>>;

    async fn subscribe_bundles(
        &self,
        request: Request<SubscribeBundlesRequest>,
    ) -> Result<Response<Self::SubscribeBundlesStream>, Status> {
        let pubkey: &Pubkey = request
            .extensions()
            .get()
            .ok_or_else(|| Status::internal("internal error fetching public key"))?;

        let (sender, receiver) = channel(BlockEngineImpl::SUBSCRIBER_QUEUE_CAPACITY);
        self.subscription_sender
            .send(Subscription::BlockEngineBundleSubscription {
                pubkey: *pubkey,
                sender,
            })
            .map_err(|_| Status::internal("internal error adding bundle subscription"))?;

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn get_block_builder_fee_info(
        &self,
        _request: Request<BlockBuilderFeeInfoRequest>,
    ) -> Result<Response<BlockBuilderFeeInfoResponse>, Status> {
        // Return the identity pubkey as the fee publisher
        Ok(Response::new(BlockBuilderFeeInfoResponse {
            pubkey: self.identity_pubkey.to_string(),
            commission: 0,
        }))
    }

    async fn get_block_engine_endpoints(
        &self,
        _request: Request<GetBlockEngineEndpointRequest>,
    ) -> Result<Response<GetBlockEngineEndpointResponse>, Status> {
        // Return default response for endpoints (no endpoints configured)
        Ok(Response::new(GetBlockEngineEndpointResponse::default()))
    }
}
