//! Listener setup shared by the controller and the runner's worker proxy.
use axum::serve::ListenerExt;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

/// Accepts connections with Nagle's algorithm off. A response leaves in
/// several small writes (headers, body chunks, the closing chunk), and with
/// Nagle on each waits for the client's delayed ACK of the one before it, which
/// can add tens of milliseconds to the end of every response.
pub(crate) fn nodelay(
    listener: TcpListener,
) -> impl axum::serve::Listener<Io = TcpStream, Addr = SocketAddr> {
    listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "could not set TCP_NODELAY on an accepted connection");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::serve::Listener;

    async fn accepted_nodelay(wrap: bool) -> bool {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
        let accepted = if wrap {
            let (stream, _) = nodelay(listener).accept().await;
            stream
        } else {
            listener.accept().await.unwrap().0
        };
        let _client = client.await.unwrap();
        accepted.nodelay().unwrap()
    }

    #[tokio::test]
    async fn accepted_connections_have_nagle_off() {
        assert!(accepted_nodelay(true).await);
        // What the helper changes: a bare listener hands out Nagle-on sockets.
        assert!(!accepted_nodelay(false).await);
    }
}
