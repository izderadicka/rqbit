use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use buffers::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use librqbit_core::{id20::Id20, lengths::ChunkInfo, peer_id::try_decode_peer_id};
use peer_binary_protocol::{
    extended::{handshake::ExtendedHandshake, ExtendedMessage},
    serialize_piece_preamble, Handshake, Message, MessageBorrowed, MessageDeserializeError,
    MessageOwned, PIECE_MESSAGE_DEFAULT_LEN,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::time::timeout;
use tracing::trace;

use crate::spawn_utils::BlockingSpawner;

pub trait PeerConnectionHandler {
    fn on_connected(&self, _connection_time: Duration) {}
    fn get_have_bytes(&self) -> u64;
    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> anyhow::Result<usize>;
    fn on_handshake<B>(&self, handshake: Handshake<B>) -> anyhow::Result<()>;
    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()>;
    fn on_received_message(&self, msg: Message<ByteBuf<'_>>) -> anyhow::Result<()>;
    fn on_uploaded_bytes(&self, bytes: u32);
    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()>;
}

#[derive(Debug)]
pub enum WriterRequest {
    Message(MessageOwned),
    ReadChunkRequest(ChunkInfo),
    Disconnect,
}

#[serde_as]
#[derive(Default, Debug, Copy, Clone, Serialize, Deserialize)]
pub struct PeerConnectionOptions {
    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub connect_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub read_write_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub keep_alive_interval: Option<Duration>,
}

pub(crate) struct PeerConnection<H> {
    handler: H,
    addr: SocketAddr,
    info_hash: Id20,
    peer_id: Id20,
    options: PeerConnectionOptions,
    spawner: BlockingSpawner,
}

pub(crate) async fn with_timeout<T, E>(
    timeout_value: Duration,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> anyhow::Result<T>
where
    E: Into<anyhow::Error>,
{
    match timeout(timeout_value, fut).await {
        Ok(v) => v.map_err(Into::into),
        Err(_) => anyhow::bail!("timeout at {timeout_value:?}"),
    }
}

macro_rules! read_one {
    ($conn:ident, $read_buf:ident, $read_so_far:ident, $rwtimeout:ident) => {{
        let (extended, size) = loop {
            match MessageBorrowed::deserialize(&$read_buf[..$read_so_far]) {
                Ok((msg, size)) => break (msg, size),
                Err(MessageDeserializeError::NotEnoughData(d, _)) => {
                    if $read_buf.len() < $read_so_far + d {
                        $read_buf.reserve(d);
                        $read_buf.resize($read_buf.capacity(), 0);
                    }

                    let size = with_timeout($rwtimeout, $conn.read(&mut $read_buf[$read_so_far..]))
                        .await
                        .context("error reading from peer")?;
                    if size == 0 {
                        anyhow::bail!("disconnected while reading, read so far: {}", $read_so_far)
                    }
                    $read_so_far += size;
                }
                Err(e) => return Err(e.into()),
            }
        };
        (extended, size)
    }};
}

impl<H: PeerConnectionHandler> PeerConnection<H> {
    pub fn new(
        addr: SocketAddr,
        info_hash: Id20,
        peer_id: Id20,
        handler: H,
        options: Option<PeerConnectionOptions>,
        spawner: BlockingSpawner,
    ) -> Self {
        PeerConnection {
            handler,
            addr,
            info_hash,
            peer_id,
            spawner,
            options: options.unwrap_or_default(),
        }
    }

    // By the time this is called:
    // read_buf should start with valuable data. The handshake should be removed from it.
    pub async fn manage_peer_incoming(
        &self,
        outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
        // How many bytes into read buffer have we read already.
        read_so_far: usize,
        read_buf: Vec<u8>,
        handshake: Handshake<ByteString>,
        mut conn: tokio::net::TcpStream,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        if handshake.info_hash != self.info_hash.0 {
            anyhow::bail!("wrong info hash");
        }

        if handshake.peer_id == self.peer_id.0 {
            bail!("looks like we are connecting to ourselves");
        }

        trace!(
            "incoming connection: id={:?}",
            try_decode_peer_id(Id20(handshake.peer_id))
        );

        let mut write_buf = Vec::<u8>::with_capacity(PIECE_MESSAGE_DEFAULT_LEN);
        let handshake = Handshake::new(self.info_hash, self.peer_id);
        handshake.serialize(&mut write_buf);
        with_timeout(rwtimeout, conn.write_all(&write_buf))
            .await
            .context("error writing handshake")?;
        write_buf.clear();

        let h_supports_extended = handshake.supports_extended();

        self.handler.on_handshake(handshake)?;

        self.manage_peer(
            h_supports_extended,
            read_so_far,
            read_buf,
            write_buf,
            conn,
            outgoing_chan,
        )
        .await
    }

    pub async fn manage_peer_outgoing(
        &self,
        outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let connect_timeout = self
            .options
            .connect_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let now = Instant::now();
        let mut conn = with_timeout(connect_timeout, tokio::net::TcpStream::connect(self.addr))
            .await
            .context("error connecting")?;
        self.handler.on_connected(now.elapsed());

        let mut write_buf = Vec::<u8>::with_capacity(PIECE_MESSAGE_DEFAULT_LEN);
        let handshake = Handshake::new(self.info_hash, self.peer_id);
        handshake.serialize(&mut write_buf);
        with_timeout(rwtimeout, conn.write_all(&write_buf))
            .await
            .context("error writing handshake")?;
        write_buf.clear();

        let mut read_buf = vec![0u8; PIECE_MESSAGE_DEFAULT_LEN * 2];
        let mut read_so_far = with_timeout(rwtimeout, conn.read(&mut read_buf))
            .await
            .context("error reading handshake")?;
        if read_so_far == 0 {
            anyhow::bail!("bad handshake");
        }
        let (h, size) = Handshake::deserialize(&read_buf[..read_so_far])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;

        let h_supports_extended = h.supports_extended();
        trace!("connected: id={:?}", try_decode_peer_id(Id20(h.peer_id)));
        if h.info_hash != self.info_hash.0 {
            anyhow::bail!("info hash does not match");
        }

        if h.peer_id == self.peer_id.0 {
            bail!("looks like we are connecting to ourselves");
        }

        self.handler.on_handshake(h)?;

        if read_so_far > size {
            read_buf.copy_within(size..read_so_far, 0);
        }
        read_so_far -= size;

        self.manage_peer(
            h_supports_extended,
            read_so_far,
            read_buf,
            write_buf,
            conn,
            outgoing_chan,
        )
        .await
    }

    async fn manage_peer(
        &self,
        handshake_supports_extended: bool,
        // How many bytes into read_buf is there of peer-sent-data.
        mut read_so_far: usize,
        mut read_buf: Vec<u8>,
        mut write_buf: Vec<u8>,
        mut conn: tokio::net::TcpStream,
        mut outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let mut extended_handshake: Option<ExtendedHandshake<ByteString>> = None;
        let supports_extended = handshake_supports_extended;

        if supports_extended {
            let my_extended =
                Message::Extended(ExtendedMessage::Handshake(ExtendedHandshake::new()));
            trace!("sending extended handshake: {:?}", &my_extended);
            my_extended.serialize(&mut write_buf, None).unwrap();
            with_timeout(rwtimeout, conn.write_all(&write_buf))
                .await
                .context("error writing extended handshake")?;
            write_buf.clear();

            let (extended, size) = read_one!(conn, read_buf, read_so_far, rwtimeout);
            match extended {
                Message::Extended(ExtendedMessage::Handshake(h)) => {
                    trace!("received: {:?}", &h);
                    self.handler.on_extended_handshake(&h)?;
                    extended_handshake = Some(h.clone_to_owned())
                }
                other => anyhow::bail!("expected extended handshake, but got {:?}", other),
            };

            if read_so_far > size {
                read_buf.copy_within(size..read_so_far, 0);
            }
            read_so_far -= size;
        }

        let (mut read_half, mut write_half) = tokio::io::split(conn);

        let writer = async move {
            let keep_alive_interval = self
                .options
                .keep_alive_interval
                .unwrap_or_else(|| Duration::from_secs(120));

            if self.handler.get_have_bytes() > 0 {
                let len = self
                    .handler
                    .serialize_bitfield_message_to_buf(&mut write_buf)?;
                with_timeout(rwtimeout, write_half.write_all(&write_buf[..len]))
                    .await
                    .context("error writing bitfield to peer")?;
                trace!("sent bitfield");
            }

            loop {
                let req = match timeout(keep_alive_interval, outgoing_chan.recv()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        anyhow::bail!("closing writer, channel closed")
                    }
                    Err(_) => WriterRequest::Message(MessageOwned::KeepAlive),
                };

                let mut uploaded_add = None;

                let len = match &req {
                    WriterRequest::Message(msg) => {
                        msg.serialize(&mut write_buf, extended_handshake.as_ref())?
                    }
                    WriterRequest::ReadChunkRequest(chunk) => {
                        // this whole section is an optimization
                        write_buf.resize(PIECE_MESSAGE_DEFAULT_LEN, 0);
                        let preamble_len = serialize_piece_preamble(chunk, &mut write_buf);
                        let full_len = preamble_len + chunk.size as usize;
                        write_buf.resize(full_len, 0);
                        self.spawner
                            .spawn_block_in_place(|| {
                                self.handler
                                    .read_chunk(chunk, &mut write_buf[preamble_len..])
                            })
                            .with_context(|| format!("error reading chunk {chunk:?}"))?;

                        uploaded_add = Some(chunk.size);
                        full_len
                    }
                    WriterRequest::Disconnect => {
                        return Ok(());
                    }
                };

                trace!("sending: {:?}, length={}", &req, len);

                with_timeout(rwtimeout, write_half.write_all(&write_buf[..len]))
                    .await
                    .context("error writing the message to peer")?;
                write_buf.clear();

                if let Some(uploaded_add) = uploaded_add {
                    self.handler.on_uploaded_bytes(uploaded_add)
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let reader = async move {
            loop {
                let (message, size) = read_one!(read_half, read_buf, read_so_far, rwtimeout);
                trace!("received: {:?}", &message);

                self.handler
                    .on_received_message(message)
                    .context("error in handler.on_received_message()")?;

                if read_so_far > size {
                    read_buf.copy_within(size..read_so_far, 0);
                }
                read_so_far -= size;
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let r = tokio::select! {
            r = reader => {r}
            r = writer => {r}
        };
        trace!("either reader or writer are done, exiting");
        r
    }
}
