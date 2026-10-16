use core::fmt;
use std::time::Duration;

use crypto_box::aead::{Aead, AeadCore, AeadMutInPlace, OsRng};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf},
    sync::Mutex,
};
use tokio_util::codec::{FramedRead, FramedWrite};
use ts_http_util::Client as _;
use ts_keys::{NodeKeyPair, NodePublicKey};
use ts_packet::PacketMut;
use ts_transport::{BatchRecvIter, BatchSendIter, DynEndpoint, UnderlayTransport};
use url::Url;

use crate::{
    Error, ServerConnInfo, frame,
    frame::{ClientInfo, FrameType, PeerGone, Ping, RawFrame, ServerInfo, ServerKey},
};

type DefaultIo = ts_http_util::Upgraded;

/// Type alias for the default derp client over upgraded HTTP on a tokio executor.
pub type DefaultClient = Client<DefaultIo>;

// ThinkTerm (derp reads): added.
/// The longest a connection may go without a frame from the server before it counts as gone.
/// The server sends a keep-alive whenever it has had nothing else to send for a minute, so a
/// connection quiet for longer died without a word: the machine slept, the network changed.
/// Go's client allows as long.
const READ_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(120)
};

/// Single-region DERP client.
pub struct Client<Io> {
    read_conn: Mutex<FramedRead<ReadHalf<Io>, frame::Codec>>,
    write_conn: Mutex<FramedWrite<WriteHalf<Io>, frame::Codec>>,
}

/// Establish and upgrade a http connection to the derp region.
#[tracing::instrument(skip_all, err)]
pub async fn connect<'c>(
    region: impl IntoIterator<Item = &'c ServerConnInfo>,
) -> Result<Option<DefaultIo>, Error> {
    // ThinkTerm (derp dial): a failed TLS handshake is an error for the caller; this unwrapped it.
    let Some((conn, _, addr)) = crate::dial::dial_region_tls(region)
        .await
        .map_err(std::io::Error::other)?
    else {
        return Ok(None);
    };

    let url = Url::parse(&format!("https://{addr}/derp"))?;

    let client = ts_http_util::http1::connect(conn).await?;

    let resp = client
        .send(ts_http_util::make_upgrade_req(&url, "DERP", None)?)
        .await?;

    let upgraded = ts_http_util::do_upgrade(resp)
        .await
        .map_err(tokio::io::Error::other)
        .map_err(Error::from)?;

    Ok(Some(upgraded))
}

impl<Io> Client<Io>
where
    Io: AsyncRead + AsyncWrite,
{
    /// Perform a derp handshake over the given transport and return a [`Client`].
    #[tracing::instrument(skip_all)]
    pub async fn handshake(conn: Io, node_keypair: &NodeKeyPair) -> Result<Self, Error> {
        let (read_conn, write_conn) = tokio::io::split(conn);

        let mut fw = FramedWrite::new(write_conn, frame::Codec);
        let mut fr = FramedRead::new(read_conn, frame::Codec);

        let frame = fr.next().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream ended before server key",
            )
        })??;
        let (sk, _rest) = frame
            .get()
            .as_type::<ServerKey>()
            .ok_or_else(|| std::io::Error::other("initial message was not serverkey"))?;

        sk.validate()?;

        tracing::trace!(
            server_public_key = %sk.key,
            "derp server public key"
        );

        let (client_info, encrypted) = make_clientinfo(node_keypair, &sk.key)?;
        tracing::trace!(?client_info);

        fw.send((
            RawFrame::from_body(&client_info, encrypted.len())?,
            encrypted.as_ref(),
        ))
        .await?;

        tracing::trace!("sent client info");

        let frame = fr.next().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream ended before server info",
            )
        })??;
        let (si, payload) = frame
            .get()
            .as_type::<ServerInfo>()
            .ok_or_else(|| std::io::Error::other("frame was not serverinfo"))?;

        tracing::trace!(server_info = ?si, "got server info");

        let info = decrypt_server_info(node_keypair, sk, si, payload)?;
        tracing::trace!(server_info = ?info);

        Ok(Self {
            read_conn: Mutex::new(fr),
            write_conn: Mutex::new(fw),
        })
    }

    /// Send a message to a nodekey on the derp server.
    pub async fn send_one(&self, node_key: NodePublicKey, msg: &[u8]) -> Result<(), Error> {
        self.send_frame_with_extra(&frame::SendPacket { dest: node_key }, msg)
            .await
    }

    /// Send a frame to the derp server.
    pub async fn send_frame(
        &self,
        frame: &(impl frame::Body + zerocopy::IntoBytes + zerocopy::Immutable + Send),
    ) -> Result<(), Error> {
        self.send_frame_with_extra(frame, &[]).await
    }

    /// Send a frame to the derp server with the specified additional payload.
    pub async fn send_frame_with_extra(
        &self,
        frame: &(impl frame::Body + zerocopy::IntoBytes + zerocopy::Immutable + Send),
        additional_payload: &[u8],
    ) -> Result<(), Error> {
        let raw = RawFrame::from_body(frame, additional_payload.len())?;

        {
            let mut wr = self.write_conn.lock().await;
            wr.send((raw, additional_payload)).await?;
        }

        Ok(())
    }

    /// Waits for a single data packet from a peer to arrive via this DERP server and returns it.
    /// DERP control messages (KeepAlive, Ping, etc) are handled inline and are not returned.
    pub async fn recv_one(&self) -> Result<(NodePublicKey, PacketMut), Error> {
        // DERP exchanges control messages (KeepAlives, Pings, etc) in-band with data messages
        // (SendPacket, RecvPacket, etc). The caller only cares about the payloads of data
        // messages, so we recv_one_raw() in a loop to handle any control messages while waiting
        // for data messages.

        loop {
            let frame = {
                let mut r = self.read_conn.lock().await;
                // ThinkTerm (derp reads): each frame, keep-alives included, has READ_TIMEOUT
                // to come. Waiting without a limit, a connection that died while nothing was
                // sent on it, as the home region's often does, held its runner for good.
                tokio::time::timeout(READ_TIMEOUT, r.next())
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "nothing from the derp server in time",
                        )
                    })?
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "derp stream ended")
                    })??
            };
            let frame = frame.get();

            match frame.header.typ {
                // TODO (dylan): handle other control message types
                // TODO (dylan): handle other data message types (ForwardPacket, etc)
                #[allow(deprecated)]
                FrameType::KeepAlive => {
                    // TODO (dylan): do we need to do anything on KeepAlive other than reset a timer?
                    // TODO (dylan): handle KeepAlive timer
                    tracing::trace!("received KeepAlive frame");
                }
                FrameType::Ping => {
                    let Some((&ping, _)) = frame.as_type::<Ping>() else {
                        tracing::warn!("ping frame was not ping");
                        continue;
                    };

                    tracing::trace!(payload = ?ping.payload, "ping");

                    let pong: frame::Pong = ping.into();
                    self.send_frame(&pong).await?;

                    tracing::trace!(payload = ?pong.payload, "pong");
                }
                FrameType::PeerGone => {
                    // ThinkTerm (derp frames): a frame too short for its reason byte, as servers
                    // from before the reason send, is passed over; this unwrapped it, panicking.
                    let Some((gone, _rest)) = frame.as_type::<PeerGone>() else {
                        tracing::debug!("peer gone frame without its reason");
                        continue;
                    };

                    tracing::debug!(
                        peer = %gone.key,
                        // ThinkTerm (derp frames): a reason this does not know is no fault of
                        // the connection; `?` ended it over one.
                        reason = ?gone.reason(),
                        "peer gone from derp server"
                    );
                }
                FrameType::RecvPacket => {
                    // ThinkTerm (derp frames): a packet too short for its source key is dropped;
                    // this unwrapped it, panicking.
                    let Some((recv, payload)) = frame.as_type::<frame::RecvPacket>() else {
                        tracing::debug!("packet frame too short for its source key");
                        continue;
                    };

                    return Ok((recv.src, payload.into()));
                }
                t => {
                    return Err(Error::UnexpectedRecvFrameType(t));
                }
            }
        }
    }
}

impl Client<DefaultIo> {
    /// Connect to and handshake with the derp server with the given URL over HTTP.
    pub async fn connect<'c>(
        region: impl IntoIterator<Item = &'c ServerConnInfo>,
        node_keypair: &NodeKeyPair,
    ) -> Result<Self, Error> {
        // ThinkTerm (derp dial): no server of the region answering is an error for the caller;
        // this unwrapped it, and the panic took the region's client down.
        let conn = connect(region).await?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::HostUnreachable,
                "no server of the DERP region could be reached",
            )
        })?;

        Client::handshake(conn, node_keypair).await
    }
}

impl<Io> fmt::Debug for Client<Io> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<Io> fmt::Display for Client<Io> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Client").finish()
    }
}

fn make_clientinfo(
    node_keypair: &NodeKeyPair,
    server_key: &ts_keys::DerpServerPublicKey,
) -> Result<(ClientInfo, Vec<u8>), Error> {
    let cbox = crypto_box::SalsaBox::new(
        &server_key.to_crypto_box(),
        &node_keypair.private.to_crypto_box(),
    );
    let nonce = crypto_box::SalsaBox::generate_nonce(&mut OsRng);

    let json = serde_json::to_vec(&frame::ClientInfoPayload {
        can_ack_pings: false,
        is_prober: false,
        mesh_key: "none".to_string(),
        version: 2,
    })?;
    let encrypted = cbox
        .encrypt(&nonce, &json[..])
        .map_err(|_| frame::Error::EncryptionFailed)?;

    Ok((
        ClientInfo {
            key: node_keypair.public,
            nonce: nonce.into(),
        },
        encrypted,
    ))
}

fn decrypt_server_info(
    node_keypair: &NodeKeyPair,
    sk: &ServerKey,
    server_info: &ServerInfo,
    payload: &[u8],
) -> Result<frame::ServerInfoPayload, Error> {
    let mut payload = PacketMut::from(payload);

    let mut cbox = crypto_box::SalsaBox::new(
        &sk.key.to_crypto_box(),
        &node_keypair.private.to_crypto_box(),
    );
    cbox.decrypt_in_place(&server_info.nonce.into(), &[], &mut payload)
        .map_err(|e| frame::Error::DecryptionFailed(format!("err: {e}")))?;

    let sip = serde_json::from_slice::<frame::ServerInfoPayload>(payload.as_ref())?;
    if sip.version() != frame::PROTOCOL_VERSION {
        return Err(Error::UnsupportedProtocolVersion(
            sip.version(),
            frame::PROTOCOL_VERSION,
        ));
    }

    Ok(sip)
}

impl<Io> UnderlayTransport for Client<Io>
where
    Io: AsyncRead + AsyncWrite + Send,
{
    type Error = Error;

    async fn send(&self, packet_batch: impl BatchSendIter) -> Result<(), Self::Error> {
        for (ep, pkt) in packet_batch.batch_iter() {
            let Some(key) = ep.as_derp() else {
                tracing::warn!(?ep, "derp transport got wrong endpoint info type");
                continue;
            };

            for pkt in pkt {
                self.send_one(key, pkt.as_ref()).await?;
            }
        }

        Ok(())
    }

    async fn recv(&self) -> impl BatchRecvIter<Error = Self::Error> {
        [self
            .recv_one()
            .await
            .map(|(k, pkt)| (DynEndpoint::derp(k), [pkt]))]
    }
}

// ThinkTerm (derp reads): the tests are ours.
#[cfg(test)]
mod tests {
    use tokio::io::DuplexStream;

    use super::*;

    /// A client on one end of a connection, and the server's end to send frames on.
    fn connection() -> (
        Client<DuplexStream>,
        FramedWrite<DuplexStream, frame::Codec>,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(client);
        let client = Client {
            read_conn: Mutex::new(FramedRead::new(read, frame::Codec)),
            write_conn: Mutex::new(FramedWrite::new(write, frame::Codec)),
        };
        (client, FramedWrite::new(server, frame::Codec))
    }

    #[tokio::test]
    async fn a_connection_gone_quiet_is_an_error() {
        let (client, _server) = connection();
        // 0.6.1 waited for good.
        let received = tokio::time::timeout(Duration::from_secs(3), client.recv_one()).await;
        assert!(
            matches!(&received, Ok(Err(Error::IoFailure(e))) if e.kind() == std::io::ErrorKind::TimedOut),
            "{received:?}"
        );
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn keep_alives_keep_a_connection_with_nothing_to_carry() {
        let (client, mut server) = connection();
        let sender = NodeKeyPair::random().public;
        let serving = tokio::spawn(async move {
            // Well past READ_TIMEOUT in all.
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let keep_alive = RawFrame::from_body(&frame::KeepAlive, 0).unwrap();
                server.send((keep_alive, &[][..])).await.unwrap();
            }
            let body = frame::RecvPacket { src: sender };
            let packet = RawFrame::from_body(&body, 3).unwrap();
            server.send((packet, &b"abc"[..])).await.unwrap();
            server
        });

        let received = tokio::time::timeout(Duration::from_secs(3), client.recv_one()).await;
        let Ok(Ok((src, packet))) = received else {
            panic!("{received:?}");
        };
        assert_eq!(src, sender);
        assert_eq!(packet.as_ref(), b"abc");
        drop(serving.await);
    }
}
