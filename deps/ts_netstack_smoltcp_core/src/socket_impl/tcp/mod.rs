use alloc::vec;

use smoltcp::socket::tcp;

use crate::Netstack;

mod listener;
mod stream;

pub use listener::{ListenerHandle, TcpListenerState};

impl Netstack {
    fn tcp_buffer(&self) -> tcp::SocketBuffer<'static> {
        tcp::SocketBuffer::new(vec![0; self.config.tcp_buffer_size])
    }
}

/// ThinkTerm: how long a connection may go without a packet from its peer -- an answer to what
/// it sent, or to a keep-alive -- before it is aborted, and how long it may sit idle before a
/// keep-alive asks. Without them a peer that goes away without a word, a laptop put to sleep,
/// leaves the connection retrying forever, and a connect nobody waits for any more lingers too.
const PEER_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(600);
const KEEP_ALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(120);

/// ThinkTerm: has `socket` notice a peer that went away, as the system's own TCP would.
fn notice_lost_peers(socket: &mut tcp::Socket) {
    socket.set_timeout(Some(PEER_TIMEOUT));
    socket.set_keep_alive(Some(KEEP_ALIVE));
}
