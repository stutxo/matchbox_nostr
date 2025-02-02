pub(crate) mod error;
mod matchbox_protocol;
mod messages;
mod signal_peer;
mod socket;

use self::error::{MessagingError, SignalingError};
use crate::{webrtc_socket::signal_peer::SignalPeer, Error};
use async_trait::async_trait;
use cfg_if::cfg_if;
use futures::{future::Either, stream::FuturesUnordered, Future, FutureExt, StreamExt};
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures_timer::Delay;
use futures_util::select;
use log::{debug, info, warn};
pub use matchbox_protocol::PeerId;

use nostr::{secp256k1::Message, Keys};

use messages::*;

pub(crate) use socket::MessageLoopChannels;
pub use socket::{
    BuildablePlurality, ChannelConfig, ChannelPlurality, MultipleChannels, NoChannels, PeerState,
    RtcIceServerConfig, SingleChannel, WebRtcChannel, WebRtcSocket, WebRtcSocketBuilder,
};
use std::{collections::HashMap, pin::Pin, time::Duration};

cfg_if! {
    if #[cfg(target_arch = "wasm32")] {
        use nostr::prelude::*;
        mod wasm;
        type UseMessenger = wasm::WasmMessenger;
        type UseSignaller = wasm::WasmSignaller;
        /// A future which runs the message loop for the socket and completes
        /// when the socket closes or disconnects

        pub type MessageLoopFuture = Pin<Box<dyn Future<Output = Result<(), Error>>>>;
    } else {
        mod native;
        type UseMessenger = native::NativeMessenger;
        type UseSignaller = native::NativeSignaller;
        /// A future which runs the message loop for the socket and completes
        /// when the socket closes or disconnects
        pub type MessageLoopFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
trait Signaller: Sized {
    async fn new(mut attempts: Option<u16>, room_url: &str) -> Result<Self, SignalingError>;

    async fn send(&mut self, request: String) -> Result<(), SignalingError>;

    async fn next_message(&mut self) -> Result<String, SignalingError>;
}

async fn signaling_loop<S: Signaller>(
    attempts: Option<u16>,
    room_url: String,
    mut requests_receiver: futures_channel::mpsc::UnboundedReceiver<PeerRequest>,
    events_sender: futures_channel::mpsc::UnboundedSender<PeerEvent>,
    nostr_keys: Keys,
) -> Result<(), SignalingError> {
    use nostr::prelude::*;

    let mut signaller = S::new(attempts, &room_url).await?;
    debug!("room {:?}", room_url);

    let pub_key = PeerId(nostr_keys.public_key());
    let tag = "matchbox-nostr-1";

    let id = uuid::Uuid::new_v4();
    let subscribe = ClientMessage::new_req(
        SubscriptionId::new(id.to_string()),
        vec![Filter::new()
            .kind(Kind::EncryptedDirectMessage)
            .since(Timestamp::now())],
    );

    signaller
        .send(subscribe.as_json())
        .await
        .map_err(SignalingError::from)?;
    debug!("subscribing to {:?}", subscribe);

    //add id and send peer message
    let assign_id = PeerEvent::IdAssigned(pub_key);
    warn!("{:?}", assign_id);
    events_sender
        .unbounded_send(assign_id)
        .map_err(SignalingError::from)?;

    loop {
        select! {
            request = requests_receiver.next().fuse() => {

            if let Some(matchbox_protocol::PeerRequest::Signal { receiver, data: _ }) = &request {

                let request = serde_json::to_string(&request).expect("serializing request");

                let created_at = Timestamp::now();
                let kind = Kind::EncryptedDirectMessage;

                let tags = vec![Tag::PubKey(receiver.0, None ), Tag::Hashtag(tag.to_string())];

                let content =
                encrypt(&nostr_keys.secret_key().unwrap(), &receiver.0, request).unwrap();

                let id = EventId::new(&nostr_keys.public_key(), created_at, &kind, &tags, &content);

                let id_bytes = id.as_bytes();
                let sig = Message::from_slice(id_bytes).unwrap();

                let event = Event {
                    id,
                    kind,
                    content,
                    pubkey: nostr_keys.public_key(),
                    created_at,
                    tags,
                    sig:  nostr_keys.sign_schnorr(&sig).unwrap(),
                };

                // Create a new ClientMessage with the encrypted message
                let msg = ClientMessage::new_event(event);

                // Log the message being sent
                warn!("SENDING...{msg:?}");

                // Send the message and handle possible errors
                signaller.send(msg.as_json()).await.map_err(SignalingError::from)?;
            }
        }

             message = signaller.next_message().fuse() => {

                match message {

                    Ok(message) => {
                        if let Ok(message) = RelayMessage::from_json(&message) {
                            match message {
                                RelayMessage::Event {
                                    event,
                                    subscription_id: _,
                                } => {
                                    if event.pubkey == nostr_keys.public_key() {
                                    } else if event.kind == Kind::EncryptedDirectMessage {
                                        warn!("RECEIVED..{event:?}");
                                        if let Ok(msg) = decrypt(
                                            &nostr_keys.secret_key().unwrap(),
                                            &event.pubkey,
                                            event.content,
                                        ) {
                                        let peer_key = event.pubkey;
                                        if let Ok(event) = serde_json::from_str::<PeerRequest>(&msg) {
                                            match event {
                                                PeerRequest::Signal{receiver: _, data } => {
                                                    let event = PeerEvent::Signal {
                                                        sender: PeerId(peer_key),
                                                        data,
                                                        };
                                                    events_sender.unbounded_send(event).map_err(SignalingError::from)?;
                                                }
                                                PeerRequest::KeepAlive => {}
                                             }
                                        } else if let Ok(new_peer) = serde_json::from_str::<PeerEvent>(&msg) {

                                            events_sender.unbounded_send(new_peer).map_err(SignalingError::from)?;
                                        }
                                        }
                                    }
                               }

                                RelayMessage::Notice { message: _ } => {
                                    warn!("{message:?}");
                                    // Handle the Notice case here
                                }
                                RelayMessage::EndOfStoredEvents(_subscription_id ) => {
                                    // Handle the EndOfStoredEvents case here
                                }
                                RelayMessage::Ok {
                                    event_id,
                                    status,
                                    message,
                                } => {
                                    warn!("{event_id:?} {status:?} {message:?}");

                                }
                                RelayMessage::Auth { challenge: _ } => {

                                }
                                RelayMessage::Count {
                                    subscription_id: _,
                                    count: _,
                                } => {
                                    // Handle the Count case here
                                }
                                RelayMessage::Empty => {
                                    // Handle the Empty case here
                                }
                            }
                        } else {

                            // Handle parsing errors if any
                        }
                    }
                    Err(SignalingError::UnknownFormat) => {
                        warn!("ignoring unexpected non-text message from signaling server")
                    }
                    Err(err) => {
                        break Err(err)
                    }
                }
            }
            complete => {
                break Ok(())
            }
        }
    }
}

/// The raw format of data being sent and received.
pub type Packet = Box<[u8]>;

trait PeerDataSender {
    fn send(&mut self, packet: Packet) -> Result<(), MessagingError>;
}

struct HandshakeResult<D: PeerDataSender, M> {
    peer_id: PeerId,
    data_channels: Vec<D>,
    metadata: M,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
trait Messenger {
    type DataChannel: PeerDataSender;
    type HandshakeMeta: Send;

    async fn offer_handshake(
        signal_peer: SignalPeer,
        mut peer_signal_rx: UnboundedReceiver<PeerSignal>,
        messages_from_peers_tx: Vec<UnboundedSender<(PeerId, Packet)>>,
        ice_server_config: &RtcIceServerConfig,
        channel_configs: &[ChannelConfig],
    ) -> HandshakeResult<Self::DataChannel, Self::HandshakeMeta>;

    async fn accept_handshake(
        signal_peer: SignalPeer,
        peer_signal_rx: UnboundedReceiver<PeerSignal>,
        messages_from_peers_tx: Vec<UnboundedSender<(PeerId, Packet)>>,
        ice_server_config: &RtcIceServerConfig,
        channel_configs: &[ChannelConfig],
    ) -> HandshakeResult<Self::DataChannel, Self::HandshakeMeta>;

    async fn peer_loop(peer_uuid: PeerId, handshake_meta: Self::HandshakeMeta) -> PeerId;
}

async fn message_loop<M: Messenger>(
    id_tx: crossbeam_channel::Sender<PeerId>,
    ice_server_config: &RtcIceServerConfig,
    channel_configs: &[ChannelConfig],
    channels: MessageLoopChannels,
    keep_alive_interval: Option<Duration>,
) {
    let MessageLoopChannels {
        requests_sender,
        mut events_receiver,
        mut peer_messages_out_rx,
        messages_from_peers_tx,
        peer_state_tx,
    } = channels;

    let mut handshakes = FuturesUnordered::new();
    let mut peer_loops = FuturesUnordered::new();
    let mut handshake_signals = HashMap::new();
    let mut data_channels = HashMap::new();

    let mut timeout = if let Some(interval) = keep_alive_interval {
        Either::Left(Delay::new(interval))
    } else {
        Either::Right(std::future::pending())
    }
    .fuse();

    loop {
        let mut next_peer_messages_out = peer_messages_out_rx
            .iter_mut()
            .enumerate()
            .map(|(channel, rx)| async move { (channel, rx.next().await) })
            .collect::<FuturesUnordered<_>>();

        let mut next_peer_message_out = next_peer_messages_out.next().fuse();

        select! {
            _  = &mut timeout => {
                requests_sender.unbounded_send(PeerRequest::KeepAlive).expect("send failed");
                // UNWRAP: we will only ever get here if there already was a timeout
                let interval = keep_alive_interval.unwrap();
                timeout = Either::Left(Delay::new(interval)).fuse();
            }

            message = events_receiver.next().fuse() => {
                if let Some(event) = message {
                    debug!("{:?}", event);
                    match event {
                        PeerEvent::IdAssigned(peer_uuid) => {
                            id_tx.try_send(peer_uuid.to_owned()).unwrap();
                        },
                        PeerEvent::NewPeer(peer_uuid) => {

                            let (signal_tx, signal_rx) = futures_channel::mpsc::unbounded();
                            handshake_signals.insert(peer_uuid, signal_tx);
                            let signal_peer = SignalPeer::new(peer_uuid, requests_sender.clone());
                            handshakes.push(M::offer_handshake(signal_peer, signal_rx, messages_from_peers_tx.clone(), ice_server_config, channel_configs))
                        },
                        PeerEvent::PeerLeft(peer_uuid) => {peer_state_tx.unbounded_send((peer_uuid, PeerState::Disconnected)).expect("fail to report peer as disconnected");},
                        PeerEvent::Signal { sender, data } => {
                            let signal_tx = handshake_signals.entry(sender).or_insert_with(|| {
                                let (from_peer_tx, peer_signal_rx) = futures_channel::mpsc::unbounded();
                                let signal_peer = SignalPeer::new(sender, requests_sender.clone());
                                handshakes.push(M::accept_handshake(signal_peer, peer_signal_rx, messages_from_peers_tx.clone(), ice_server_config, channel_configs));
                                from_peer_tx
                            });

                            if signal_tx.unbounded_send(data).is_err() {
                                warn!("ignoring signal from peer {sender:?} because the handshake has already finished");
                            }
                        },
                    }
                }
            }



            handshake_result = handshakes.select_next_some() => {
                data_channels.insert(handshake_result.peer_id, handshake_result.data_channels);
                peer_state_tx.unbounded_send((handshake_result.peer_id, PeerState::Connected)).expect("failed to report peer as connected");
                peer_loops.push(M::peer_loop(handshake_result.peer_id, handshake_result.metadata));
            }

            peer_uuid = peer_loops.select_next_some() => {
                debug!("peer {peer_uuid:?} finished");
                peer_state_tx.unbounded_send((peer_uuid, PeerState::Disconnected)).expect("failed to report peer as disconnected");
            }

            message = next_peer_message_out => {
                match message {
                    Some((channel_index, Some((peer, packet)))) => {
                        let data_channel = data_channels
                            .get_mut(&peer)
                            .expect("couldn't find data channel for peer")
                            .get_mut(channel_index).unwrap_or_else(|| panic!("couldn't find data channel with index {channel_index}"));
                        data_channel.send(packet).unwrap();

                    }
                    Some((_, None)) | None => {
                        // Receiver end of outgoing message channel closed,
                        // which most likely means the socket was dropped.
                        // There could probably be cleaner ways to handle this,
                        // but for now, just exit cleanly.
                        debug!("Outgoing message queue closed");
                        break;
                    }
                }
            }

            complete => break
        }
    }
}
