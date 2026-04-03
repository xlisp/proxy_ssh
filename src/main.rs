#[allow(dead_code)]
mod protocol;

fn main() {
    println!("Use `proxy-server` or `proxy-client` binary.");
    println!();
    println!("Server (on public node 104.244.95.160):");
    println!("  proxy-server --control-port 7000 --proxy-port 7001 --secret YOUR_SECRET");
    println!();
    println!("Client (on home Linux machine):");
    println!("  proxy-client --server 104.244.95.160 --control-port 7000 --local-target 127.0.0.1:22 --secret YOUR_SECRET");
}
