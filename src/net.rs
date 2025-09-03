use std::net::TcpListener;
use anyhow::Result;

/// Pick a random free TCP port by binding to port 0 and returning the assigned port.
pub fn pick_free_port() -> Result<u16> {
    let sock = TcpListener::bind(("0.0.0.0", 0))?;
    let port = sock.local_addr()?.port();
    drop(sock);
    Ok(port)
}
