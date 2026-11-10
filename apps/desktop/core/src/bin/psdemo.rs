//! Phase 1 manual test tool.
//!   psdemo <data_dir> <name> host                 show a pairing code, then print received text
//!   psdemo <data_dir> <name> join <ip:port> <code>  pair with a host
//!   psdemo <data_dir> <name> send <device_id> <text>  send text to a paired device found via mDNS
//!   psdemo <data_dir> <name> listen               print received text
use std::time::Duration;

use pinchshare_core::{Incoming, Node, NodeConfig};

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: psdemo <data_dir> <name> host|join <ip:port> <code>|send <id> <text>|listen|peers");
        std::process::exit(2);
    }
    let (node, mut rx) = Node::start(NodeConfig {
        data_dir: a[1].clone().into(), device_name: a[2].clone(), bind: "0.0.0.0:0".parse().unwrap(), mdns: true,
    }).await.expect("start");
    println!("device {} ({}) listening on {}", node.identity.name, node.identity.device_id, node.addr);
    match a[3].as_str() {
        "host" => {
            println!("pairing code: {}  (valid 120s)", node.begin_pairing(Duration::from_secs(120)));
            while let Some(Incoming::Text { from_name, text, .. }) = rx.recv().await { println!("[{from_name}] {text}"); }
        }
        "join" => match node.pair_with(a[4].parse().expect("ip:port"), &a[5]).await {
            Ok(d) => println!("paired with {} ({})", d.name, d.id),
            Err(e) => { eprintln!("pairing failed: {e}"); std::process::exit(1) }
        },
        "send" => {
            tokio::time::sleep(Duration::from_secs(3)).await; // let mDNS resolve
            match node.send_text(&a[4], &a[5]).await { Ok(()) => println!("sent"), Err(e) => { eprintln!("{e}"); std::process::exit(1) } }
        }
        "peers" => { tokio::time::sleep(Duration::from_secs(3)).await; println!("{:#?}", node.peers()); }
        _ => while let Some(Incoming::Text { from_name, text, .. }) = rx.recv().await { println!("[{from_name}] {text}"); },
    }
    node.shutdown();
}
