//! UDP audio multicast + TCP control server implementation.
use std::{net::{TcpListener, TcpStream, UdpSocket, SocketAddr, Shutdown, Ipv4Addr}, thread, time::{Duration, Instant}, sync::{Arc, atomic::{AtomicBool, AtomicU8, Ordering, AtomicU64}}};
use std::io::Write;
use anyhow::{Result, Context};
use dashmap::DashMap;
use rand::{Rng, distributions::Alphanumeric};
use sha2::{Sha256, Digest};
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, XChaCha20Poly1305};
use crossbeam_channel::{Receiver};
use parking_lot::Mutex;

use crate::{audio::{AudioParams}, buffers::AudioBufferPool, types};
use crossbeam_channel::Sender as CbSender;

#[derive(Clone, Debug)]
/// Lightweight client entry (updated by control loop and used by multicast loop).
pub struct ClientInfo { pub addr: SocketAddr, pub key: String, pub last_seen: Instant, pub udp_port: Option<u16> }

// Minimal atomic f64 wrapper (reuse pattern from client)
#[derive(Debug)]
pub struct AtomicF64(pub AtomicU64);
impl AtomicF64 { pub fn new(v:f64)->Self { Self(AtomicU64::new(v.to_bits())) } pub fn load(&self)->f64 { f64::from_bits(self.0.load(Ordering::Relaxed)) } pub fn store(&self,v:f64){ self.0.store(v.to_bits(), Ordering::Relaxed); } }
impl Clone for AtomicF64 { fn clone(&self) -> Self { Self(AtomicU64::new(self.load().to_bits())) } }

/// Shared server mutable state (Arc-based cheap cloning for threads).
pub struct ServerState {
    pub running: Arc<AtomicBool>,
    pub clients: Arc<DashMap<SocketAddr, ClientInfo>>,
    pub audio_params: Arc<Mutex<Option<AudioParams>>>,
    pub stage: Arc<AtomicU8>, // 0=stopped,1=listening,2=audio_ready
    pub input_running: Arc<AtomicBool>, // controls input capture thread/stream
    pub input_stop_tx: Arc<Mutex<Option<CbSender<()>>>>, // signal precise stop
    pub current_rms: Arc<AtomicF64>, // latest audio RMS
    pub peak_rms: Arc<AtomicF64>,    // decaying peak RMS
    pub multicast_addr: Ipv4Addr,     // multicast address
    pub multicast_port: u16,          // multicast port (can be same or separate from control port)
    pub psk: Option<String>,          // optional pre-shared key (enables encryption)
    pub salt: [u8;8],                 // session salt (key derivation + nonce prefix)
    pub key_bytes: Option<[u8;32]>,   // derived symmetric key (XChaCha20-Poly1305)
}

impl ServerState { pub fn new() -> Self {
    // Multicast address: choose inside 239.0.0.0/8 (administratively scoped)
    let maddr = Ipv4Addr::new(239,rand::thread_rng().gen(),rand::thread_rng().gen(), rand::thread_rng().gen());
    let mut salt=[0u8;8]; rand::thread_rng().fill(&mut salt);
    Self { running: Arc::new(AtomicBool::new(false)), clients: Arc::new(DashMap::new()), audio_params: Arc::new(Mutex::new(None)), stage: Arc::new(AtomicU8::new(0)), input_running: Arc::new(AtomicBool::new(false)), input_stop_tx: Arc::new(Mutex::new(None)), current_rms: Arc::new(AtomicF64::new(0.0)), peak_rms: Arc::new(AtomicF64::new(0.0)), multicast_addr: maddr, multicast_port: 0, psk: None, salt, key_bytes: None }
} 
    /// Enable PSK encryption (call before start_server)
    pub fn enable_psk(&mut self, psk: String) {
        self.psk = Some(psk.clone());
    // Derive key = SHA256(psk || salt)
    let mut hasher: Sha256 = Default::default();
        hasher.update(psk.as_bytes());
        hasher.update(&self.salt);
        let digest = hasher.finalize();
        let mut key = [0u8;32]; key.copy_from_slice(&digest[..32]);
        self.key_bytes = Some(key);
    }
}
impl Clone for ServerState { fn clone(&self)->Self { Self { running: self.running.clone(), clients: self.clients.clone(), audio_params: self.audio_params.clone(), stage: self.stage.clone(), input_running: self.input_running.clone(), input_stop_tx: self.input_stop_tx.clone(), current_rms: self.current_rms.clone(), peak_rms: self.peak_rms.clone(), multicast_addr: self.multicast_addr, multicast_port: self.multicast_port, psk: self.psk.clone(), salt: self.salt, key_bytes: self.key_bytes } } }

/// Launch server threads (control + audio multicast). Non-blocking.
pub fn start_server(mut state: ServerState, bind_ip: String, port: u16, pool: Arc<AudioBufferPool>, filled_rx: Receiver<usize>) -> Result<()> {
    state.running.store(true, Ordering::SeqCst);
    state.stage.store(0, Ordering::SeqCst);
    let tcp_listener = TcpListener::bind((bind_ip.as_str(), port)).with_context(|| "bind tcp")?;
    tcp_listener.set_nonblocking(true).ok();
    // Multicast: bind ephemeral local port for sending
    let udp = UdpSocket::bind((bind_ip.as_str(), 0)).with_context(|| "bind udp multicast send socket")?;
    udp.set_nonblocking(true).ok();
    state.multicast_port = port; // use provided port for multicast receive side
    println!("[SERVER] multicast group selected: {}:{} (enc={})", state.multicast_addr, state.multicast_port, if state.key_bytes.is_some() {"on"} else {"off"});
    state.stage.store(1, Ordering::SeqCst); // listening
    let s_clone = state.clone();
    // Control thread
    thread::spawn(move || { control_loop(tcp_listener, s_clone); });
    let s_clone2 = state.clone();
    thread::spawn(move || { audio_multicast_loop(s_clone2, udp, pool, filled_rx); });
    Ok(())
}

fn random_key() -> String { rand::thread_rng().sample_iter(&Alphanumeric).take(16).map(char::from).collect() }

/// Accept & service control TCP connections (handshake + heartbeats + UDP port announce).
fn control_loop(listener: TcpListener, state: ServerState) {
    let _buf = [0u8; 1024];
    loop {
        if !state.running.load(Ordering::Relaxed) { break; }
        match listener.accept() {
            Ok((mut stream, addr)) => {
                // Make per-client stream non-blocking so we can poll running flag
                let _ = stream.set_nonblocking(true);
                let key = random_key();
                let params = state.audio_params.lock().clone();
                let header = if let Some(p)=params { 
                    let fmt_code = crate::types::sample_format_code(p.sample_format);
                    let mut base = format!("OK {} {} {} {} {} {}", key, p.sample_rate, p.channels, fmt_code, state.multicast_addr, state.multicast_port);
                    if let Some(_kb) = state.key_bytes { 
                        // Append ENC + salt hex
                        let salt_hex: String = state.salt.iter().map(|b| format!("{:02x}", b)).collect();
                        base.push_str(&format!(" ENC {}", salt_hex));
                    } else {
                        base.push_str(" NOENC");
                    }
                    base.push('\n');
                    base
                } else { format!("NO_PARAMS {key}\n") };
                let _ = stream.write_all(header.as_bytes());
                let ci = ClientInfo { addr, key: key.clone(), last_seen: Instant::now(), udp_port: None };
                state.clients.insert(addr, ci);
                let st_clone = state.clone();
                thread::spawn(move || { per_client_control(stream, addr, st_clone); });
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(50)); },
            Err(e) => { eprintln!("accept err: {e}"); thread::sleep(Duration::from_millis(200)); }
        }
        // Heartbeat cleanup
        let now = Instant::now();
        let mut to_remove = vec![];
        for r in state.clients.iter() { if now.duration_since(r.last_seen) > Duration::from_secs(5) { to_remove.push(*r.key()); } }
        for k in to_remove { state.clients.remove(&k); }
    }
}

/// Handle a single client's control connection until disconnect.
fn per_client_control(mut stream: TcpStream, addr: SocketAddr, state: ServerState) {
    use std::io::Read; use std::io::Write;
    let mut buf = [0u8; 256];
    loop {
        if !state.running.load(Ordering::Relaxed) {
            let _ = stream.write_all(b"SERVER_STOP\n");
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                for line in raw.lines() {
                    let line = line.trim(); if line.is_empty() { continue; }
                    if line.starts_with("HEART ") {
                        let parts: Vec<_> = line.split_whitespace().collect();
                        if parts.len()==2 { if let Some(mut ci) = state.clients.get_mut(&addr) { if ci.key == parts[1] { ci.last_seen = std::time::Instant::now(); let _ = stream.write_all(b"OK\n"); } } }
                    } else if line == "DISCONNECT" { state.clients.remove(&addr); let _ = stream.write_all(b"BYE\n"); return; }
                }
            },
            Err(e) if e.kind()==std::io::ErrorKind::WouldBlock => { std::thread::sleep(std::time::Duration::from_millis(50)); },
            Err(_) => { break; },
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Pop captured buffers, build framed packets with timestamp, and send to all clients.
fn audio_multicast_loop(state: ServerState, udp: UdpSocket, pool: Arc<AudioBufferPool>, filled_rx: Receiver<usize>) {
    let mut seq: u32 = 0;
    let mut rms_counter: u32 = 0;
        // Base monotonic time reference for timestamps (nanoseconds since first frame loop start)
        let start_instant = Instant::now();
    while state.running.load(Ordering::Relaxed) {
        if let Ok(idx) = filled_rx.recv_timeout(Duration::from_millis(200)) {
            let data_guard = pool.data[idx].lock();
            let raw: &[u8] = &data_guard;
            if raw.len() < 4 { pool.push(idx); continue; }
            let payload_len = u32::from_le_bytes([raw[0],raw[1],raw[2],raw[3]]) as usize;
            if payload_len == 0 || payload_len+4 > raw.len() { pool.push(idx); continue; }
            let data = &raw[4..4+payload_len];
            // Compute simple RMS (assume f32 frames if divisible by 4) for debug
            let rms = if data.len() % 4 == 0 { let mut acc=0f64; let mut cnt=0usize; for chunk in data.chunks_exact(4) { let mut a=[0u8;4]; a.copy_from_slice(chunk); let v=f32::from_ne_bytes(a) as f64; acc+=v*v; cnt+=1; } if cnt>0 { (acc/(cnt as f64)).sqrt() } else { 0.0 } } else { 0.0 };
            rms_counter += 1; if rms_counter % 50 == 0 { println!("[SERVER] RMS ~ {:.5}", rms); }
            // Update shared RMS & peak (decay ~1% per frame batch ~depends on capture rate) ; GUI decays similarly
            state.current_rms.store(rms as f64);
            let prev_peak = state.peak_rms.load();
            let new_peak = if rms > prev_peak { rms } else { prev_peak * 0.99 }; // simple exponential decay
            if (new_peak - prev_peak).abs() > 1e-12 { state.peak_rms.store(new_peak); }
            // println!("[SERVER] multicast buffer {} ({} bytes payload) to {} clients", idx, data.len(), state.clients.len());
            let to_remove = vec![]; // currently unused removal list placeholder
            let params_opt = state.audio_params.lock().clone();
            let (sr, ch, fmt_code) = if let Some(p)=params_opt { (p.sample_rate, p.channels, types::sample_format_code(p.sample_format)) } else { (48000u32, 2u16, types::FMT_F32) };
            // Header: magic(2) + seq(u32) + fmt(u8) + ch(u8) + rate(u32) + payload_len(u16) = 2+4+1+1+4+2 =14 bytes
            // New header with timestamp (nanoseconds since start):
            // magic(2) | seq(u32) | fmt(u8) | ch(u8) | rate(u32) | payload_len(u16) | ts_us(u64)
            // = 2+4+1+1+4+2+8 = 22 bytes header
            let payload_len = data.len().min(u16::MAX as usize) as u16;
            let ts_ns: u64 = start_instant.elapsed().as_nanos() as u64;
            let mut frame = Vec::with_capacity(22 + payload_len as usize);
            frame.extend_from_slice(&types::FRAME_MAGIC);          // 0..2
            frame.extend_from_slice(&seq.to_be_bytes());            // 2..6
            frame.push(fmt_code);                                   // 6
            frame.push(ch as u8);                                   // 7
            frame.extend_from_slice(&sr.to_be_bytes());             // 8..12
            frame.extend_from_slice(&payload_len.to_be_bytes());    // 12..14
            frame.extend_from_slice(&ts_ns.to_be_bytes());          // 14..22
            frame.extend_from_slice(&data[..payload_len as usize]); // 22..
            seq = seq.wrapping_add(1);
            // Optional encryption (payload only, header as AAD)
            let mcast_sock = SocketAddr::new(std::net::IpAddr::V4(state.multicast_addr), state.multicast_port);
            if let Some(key_bytes) = state.key_bytes {
                // Rebuild header so payload_len reflects ciphertext length; use final header as AAD
                if frame.len() >= 22 {
                    let plaintext_payload_len = frame.len() - 22; // existing payload length (u16 already capped)
                    let ciphertext_len = plaintext_payload_len + 16; // AEAD tag 16 bytes
                    if ciphertext_len <= u16::MAX as usize {
                        // Extract fields
                        let seq_header = seq.wrapping_sub(1); // seq value in header
                        let fmt_code = frame[6];
                        let ch_byte = frame[7];
                        let sr_bytes = &frame[8..12];
                        let ts_bytes = &frame[14..22];
                        let payload_plain = &frame[22..];
                        let mut nonce = [0u8;24];
                        nonce[..8].copy_from_slice(&state.salt);
                        nonce[8..12].copy_from_slice(&seq_header.to_be_bytes());
                        nonce[12..20].copy_from_slice(&u64::from_be_bytes(ts_bytes.try_into().unwrap()).to_be_bytes());
                        let cipher = XChaCha20Poly1305::new(&key_bytes.into());
                        // Build final header (AAD)
                        let mut final_header = [0u8;22];
                        final_header[0..2].copy_from_slice(&types::FRAME_MAGIC);
                        final_header[2..6].copy_from_slice(&seq_header.to_be_bytes());
                        final_header[6] = fmt_code;
                        final_header[7] = ch_byte;
                        final_header[8..12].copy_from_slice(sr_bytes);
                        final_header[12..14].copy_from_slice(&(ciphertext_len as u16).to_be_bytes());
                        final_header[14..22].copy_from_slice(ts_bytes);
                        match cipher.encrypt(&nonce.into(), Payload { msg: payload_plain, aad: &final_header }) {
                            Ok(ct) => {
                                let mut out = Vec::with_capacity(22 + ct.len());
                                out.extend_from_slice(&final_header);
                                out.extend_from_slice(&ct);
                                let _ = udp.send_to(&out, mcast_sock);
                            }
                            Err(e) => {
                                eprintln!("[SERVER][ENC] encrypt fail seq={seq_header}: {e} -> send plaintext");
                                let _ = udp.send_to(&frame, mcast_sock);
                            }
                        }
                    } else {
                        // Fallback: plaintext (too large)
                        let _ = udp.send_to(&frame, mcast_sock);
                    }
                } else {
                    let _ = udp.send_to(&frame, mcast_sock);
                }
            } else { let _ = udp.send_to(&frame, mcast_sock); }
            for r in to_remove { state.clients.remove(&r); }
            pool.push(idx);
        }
    }
}

/// Signal server shutdown (threads exit naturally when flags flip).
pub fn stop_server(state: &ServerState) {
    state.running.store(false, Ordering::SeqCst);
    state.input_running.store(false, Ordering::SeqCst);
    if let Some(tx) = state.input_stop_tx.lock().take() { let _ = tx.send(()); }
    state.stage.store(0, Ordering::SeqCst);
    // Clients will naturally time out / be removed; optionally we could clear now.
}
