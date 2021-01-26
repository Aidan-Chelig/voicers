use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, mpsc, Mutex};
use tokio::prelude::{*};
use std::thread;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use anyhow::Result;
use futures::prelude::*;
use futures::never::Never;

use crate::voice;

type Tx = mpsc::UnboundedSender<u8>;
type Rx = mpsc::UnboundedReceiver<u8>;

type Watcher = tokio::sync::watch::Receiver<u8>;

struct Shared {
    peers: HashMap<SocketAddr, Tx>,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
        }
    }

    /// Send a `LineCodec` encoded message to every peer, except
    /// for the sender.
    async fn broadcast(&mut self, sender: SocketAddr, message: u8) {
        for peer in self.peers.iter_mut() {
            if *peer.0 != sender {
                let _ = peer.1.send(message);
            }
        }
    }
}

struct Peer {
    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    data: TcpStream,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: Rx,
}

impl Peer {
    /// Create a new instance of `Peer`.
    async fn new(
        state: Arc<Mutex<Shared>>,
        data: TcpStream,
    ) -> io::Result<Peer> {
        // Get the client socket address
        let addr = data.peer_addr()?;

        // Create a channel for this peer
        let (tx, rx) = mpsc::unbounded_channel();

        // Add an entry for this `Peer` in the shared state map.
        state.lock().await.peers.insert(addr, tx);

        Ok(Peer { data, rx })
    }
}


pub async fn start(rx: Watcher) -> Result<(), anyhow::Error> {
    let addr = "127.0.0.1:3000".to_string();
    let state = Arc::new(Mutex::new(Shared::new()));


    let addr = "127.0.0.1:3000";
    let mut listener = TcpListener::bind(addr).await?;
    

    loop {
        let (stream, addr) = listener.accept().await?;
        println!("newconnection");
        
        let state = Arc::clone(&state);
        let send = rx.clone();
        tokio::spawn(async move {
            //process all peers in one loopfutures join
            if let Err(e) = process(send, state, addr, stream).await {
                  println!("an error occurred; error = {:#}", e);
                  println!("{:?}", addr);

            }
        });
    }



    Ok(())

}

async fn process(mut rx: Watcher, state: Arc<Mutex<Shared>>, addr: SocketAddr, mut stream: TcpStream) -> Result<(), anyhow::Error> {

    while let Some(res) = rx.next().await {
        stream.write_all(&[res]).await?;
    }
    //important bit. there should be plenty of bytes to pull from rx
    /*
    loop {
    while let Some(value) = rx.recv().await {
        println!("{:?}", value);
    }*/

    /*loop {
        let t = match rx.recv().await {
            Some(n) => stream.write_all(&n.to_be_bytes()).await?,
            None => (),
        };
    }*/

    return Ok(());
}