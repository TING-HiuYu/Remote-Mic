//! Client side: TCP control + UDP receive + jitter buffer + playback.
use std::{net::{TcpStream, SocketAddr, UdpSocket, Ipv4Addr}, thread, time::Duration, sync::{Arc, atomic::{AtomicBool, Ordering}, Mutex}}; use std::io::Write;
use sha2::{Sha256, Digest};
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, XChaCha20Poly1305};
use crate::audio; // bring module into scope
use anyhow::Result;
use crossbeam_channel::{unbounded, Sender, Receiver};
use crate::audio::AudioParams;
use crate::types;
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::Sender as CbSender;
use tokio::sync::mpsc::UnboundedSender as EventSender;

/// Aggregated client runtime state shared across helper threads.
pub struct ClientState {
    pub connected: Arc<AtomicBool>,
    pub params: Option<AudioParams>,
    pub key: Option<String>,
    pub server: Option<SocketAddr>,
    pub udp_local: Option<SocketAddr>,
    pub multicast_addr: Option<(Ipv4Addr, u16)>,
    pub audio_tx: Option<Sender<Vec<f32>>>,
    pub output_running: Arc<AtomicBool>,
    pub udp_thread_alive: Arc<AtomicBool>,
    pub ctrl: Option<Arc<std::sync::Mutex<TcpStream>>>,
    pub output_stop_tx: Arc<Mutex<Option<CbSender<()>>>>, 
    pub disconnection_reason: Arc<Mutex<Option<String>>>,
    pub event_sender: Option<EventSender<String>>,
    // metrics shared with GUI
    pub avg_latency_ms: Arc<AtomicF64>,
    pub jitter_ms: Arc<AtomicF64>,
    pub packet_loss: Arc<AtomicF64>, // ratio 0..1
    pub late_drop: Arc<AtomicF64>,   // count (as f64)
    pub current_rms: Arc<AtomicF64>,
    pub peak_rms: Arc<AtomicF64>, // 带衰减的峰值 (RMS)
    // encryption
    pub enc_enabled: bool,
    pub enc_salt: Option<[u8;8]>,
    pub enc_key: Option<[u8;32]>,
    pub decrypt_fail: Arc<std::sync::atomic::AtomicU64>, // decrypt failures counter
    pub enc_status: Arc<std::sync::atomic::AtomicI32>,   // encryption status: 0=plain 1=ok -1=key error
}

// Minimal f64 atomic wrapper (stable AtomicF64 not yet available everywhere)
#[derive(Default)]
pub struct AtomicF64(std::sync::atomic::AtomicU64);
impl AtomicF64 { pub fn new(v:f64)->Self { Self(std::sync::atomic::AtomicU64::new(v.to_bits())) } pub fn load(&self)->f64 { f64::from_bits(self.0.load(Ordering::Relaxed)) } pub fn store(&self,v:f64){ self.0.store(v.to_bits(), Ordering::Relaxed); } }

impl ClientState { pub fn new() -> Self { Self { connected: Arc::new(AtomicBool::new(false)), params: None, key: None, server: None, udp_local: None, multicast_addr: None, audio_tx: None, output_running: Arc::new(AtomicBool::new(false)), udp_thread_alive: Arc::new(AtomicBool::new(false)), ctrl: None, output_stop_tx: Arc::new(Mutex::new(None)), disconnection_reason: Arc::new(Mutex::new(None)), event_sender: None, avg_latency_ms: Arc::new(AtomicF64::new(0.0)), jitter_ms: Arc::new(AtomicF64::new(0.0)), packet_loss: Arc::new(AtomicF64::new(0.0)), late_drop: Arc::new(AtomicF64::new(0.0)), current_rms: Arc::new(AtomicF64::new(0.0)), peak_rms: Arc::new(AtomicF64::new(0.0)), enc_enabled: false, enc_salt: None, enc_key: None, decrypt_fail: Arc::new(std::sync::atomic::AtomicU64::new(0)), enc_status: Arc::new(std::sync::atomic::AtomicI32::new(0)) } } 
    pub fn update_enc_status(&self, new: i32) { if self.enc_status.load(Ordering::Relaxed) != new { self.enc_status.store(new, Ordering::Relaxed); } }
}

fn hex_to_array8(s: &str) -> Result<[u8;8], ()> {
    if s.len()!=16 { return Err(()); }
    let mut out=[0u8;8];
    for i in 0..8 { let byte = u8::from_str_radix(&s[i*2..i*2+2], 16).map_err(|_| ())?; out[i]=byte; }
    Ok(out)
}

/// Connect to server (TCP handshake + start heartbeat). No audio output.
pub fn connect(server_ip: String, port: u16, psk: Option<String>, event_sender: Option<EventSender<String>>) -> Result<ClientState> {
    use std::io::{Read, ErrorKind};
    let mut stream = TcpStream::connect((server_ip.as_str(), port))?; // 初始连接
    // Make stream non-blocking and poll handshake bytes
    stream.set_nonblocking(true)?;
    let start = std::time::Instant::now();
    let deadline = start + Duration::from_secs(3);
    let mut header_bytes: Vec<u8> = Vec::with_capacity(256);
    loop {
        let mut tmp = [0u8; 128];
        match stream.read(&mut tmp) {
            Ok(0) => { // 远端关闭
                break;
            }
            Ok(n) => {
                header_bytes.extend_from_slice(&tmp[..n]);
                if header_bytes.contains(&b'\n') { break; }
                if header_bytes.len() > 240 { break; }
            }
            Err(ref e) if e.kind()==ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    return Err(anyhow::anyhow!("handshake timeout (waited >3s)"));
                }
                std::thread::sleep(Duration::from_millis(15));
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let header = String::from_utf8_lossy(&header_bytes).to_string();
    println!("[CLIENT] handshake raw: {:?}", header_bytes);
    println!("[CLIENT] handshake header: {}", header.trim());
    let mut state = ClientState::new(); state.event_sender = event_sender;
    let parts: Vec<_> = header.split_whitespace().collect();
    if parts.len()>=2 && parts[0]=="OK" {
        let key = parts[1].to_string();
        state.key = Some(key.clone());
        if parts.len()>=5 { if let (Ok(sr), Ok(ch), Ok(fmt_code)) = (parts[2].parse::<u32>(), parts[3].parse::<u16>(), parts[4].parse::<u8>()) { let sf = types::code_to_sample_format(fmt_code); state.params = Some(AudioParams { sample_rate: sr, channels: ch, sample_format: sf }); } }
        if parts.len()>=7 { if let (Ok(ipv4), Ok(mport)) = (parts[5].parse::<Ipv4Addr>(), parts[6].parse::<u16>()) { state.multicast_addr = Some((ipv4, mport)); } }
    // Encryption tokens: either ENC <salthex> or NOENC
        if let Some(idx_enc) = parts.iter().position(|p| *p=="ENC" || p.starts_with("ENC")) {
            // Accept: ENC <salthex> or ENC<salthex>
            let salt_hex = if parts[idx_enc]=="ENC" { parts.get(idx_enc+1).map(|s| *s).unwrap_or("") } else { &parts[idx_enc][3..] };
            if salt_hex.len()==16 { // 8 bytes hex
                if let Ok(salt_bytes) = hex_to_array8(salt_hex) {
                    state.enc_enabled = true; state.enc_salt = Some(salt_bytes);
                    if let (Some(psk_str), Some(_)) = (psk.as_ref(), state.enc_salt) {
                        let mut hasher: Sha256 = Default::default();
                        hasher.update(psk_str.as_bytes());
                        hasher.update(&salt_bytes);
                        let digest = hasher.finalize();
                        let mut key=[0u8;32]; key.copy_from_slice(&digest[..32]);
                        state.enc_key = Some(key);
                        println!("[CLIENT] encryption enabled (salt={}, key_derived)", salt_hex);
                        state.update_enc_status(1);
                    } else { println!("[CLIENT][WARN] server encryption enabled but no PSK provided"); }
                } else { println!("[CLIENT][WARN] invalid salt hex len"); }
            } else { println!("[CLIENT][WARN] ENC token but salt malformed"); }
        } else {
            // Plain (no encryption) path
            state.update_enc_status(0);
        }
        state.server = Some(SocketAddr::new(stream.peer_addr()?.ip(), port));
        state.connected.store(true, Ordering::SeqCst);
    let ctrl_arc = Arc::new(std::sync::Mutex::new(stream));
    let hb_connected = state.connected.clone();
    let hb_output_running = state.output_running.clone();
    let hb_udp_alive = state.udp_thread_alive.clone();
    let hb_stop_tx_arc = state.output_stop_tx.clone();
    let key_copy = state.key.clone(); let reason_clone = state.disconnection_reason.clone();
    state.ctrl = Some(ctrl_arc.clone());
    let ev_clone = state.event_sender.clone();
    thread::spawn(move || heartbeat_loop(
        ctrl_arc.clone(),
        key_copy.unwrap(),
        hb_connected,
        hb_output_running,
        hb_udp_alive,
        hb_stop_tx_arc,
        reason_clone,
        ev_clone,
    ));
        // UDP thread TODO: handshake actual port; for now reuse same port local ephemeral.
    }
    Ok(state)
}

/// Connect plus configure UDP + output playback thread.
pub fn connect_with_output(server_ip: String, port: u16, output_index: usize, psk: Option<String>, event_sender: Option<EventSender<String>>) -> Result<ClientState> {
    let mut state = connect(server_ip.clone(), port, psk, event_sender)?;
    if !state.connected.load(Ordering::Relaxed) { return Ok(state); }
    // Setup UDP multicast receiving socket
    let (m_ip, m_port) = if let Some(t) = state.multicast_addr { t } else { (Ipv4Addr::new(239,255,0,222), port) }; // fallback default
    let bind_addr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), m_port);
    let udp = UdpSocket::bind(bind_addr)?; 
    let _ = udp.set_nonblocking(true); // reuse_address not exposed in stable std; OS default usually fine
    if let Err(e) = udp.join_multicast_v4(&m_ip, &Ipv4Addr::UNSPECIFIED) { eprintln!("[CLIENT][MCAST] join group {m_ip}:{m_port} failed: {e}"); }
    let local_addr = udp.local_addr().ok(); state.udp_local = local_addr.clone();
    println!("[CLIENT] Joined multicast {m_ip}:{m_port} local={:?}", local_addr);
    if let Some(params) = &state.params {
        let outputs = audio::list_devices().map(|(_i,o)| o).unwrap_or(vec![]);
        let out_dev = outputs.get(output_index).or_else(|| outputs.get(0));
        if let Some(dev) = out_dev { println!("[CLIENT] Selected output device: {}", audio::device_name(dev));
            let (tx, rx) = unbounded::<Vec<f32>>();
        state.audio_tx = Some(tx.clone());
            state.output_running.store(true, Ordering::SeqCst);
            if let Some(dev_clone) = out_dev.cloned() { let stop_tx = spawn_output_thread(dev_clone, rx, state.output_running.clone(), params.clone()); if let Ok(mut guard)=state.output_stop_tx.lock() { *guard = Some(stop_tx); } }
            // UDP receive -> channel
            let udp_clone = udp.try_clone()?;
        let alive = state.udp_thread_alive.clone(); alive.store(true, Ordering::SeqCst);
            // Capture metrics handles
            let metrics_latency = state.avg_latency_ms.clone();
            let metrics_jitter = state.jitter_ms.clone();
            let metrics_loss = state.packet_loss.clone();
            let metrics_late = state.late_drop.clone();
            let metrics_rms = state.current_rms.clone();
            let metrics_peak = state.peak_rms.clone();
            // Clone encryption fields & decrypt fail counter for UDP thread so we don't move full state
            let enc_enabled = state.enc_enabled;
            let enc_salt = state.enc_salt;
            let enc_key = state.enc_key;
            let decrypt_fail = state.decrypt_fail.clone();
            let enc_status = state.enc_status.clone();
            thread::spawn(move || {
                use std::cmp::Reverse; use std::collections::BinaryHeap;
                let mut buf = vec![0u8; 65536];
                let mut last_stats_report = std::time::Instant::now();
                let mut latency_acc: f64 = 0.0; let mut latency_samples: u64 = 0;
                // Clock alignment & jitter state
                let mut base_server_ts: Option<u64> = None;            // first server ts_ns
                let mut base_client_instant: Option<std::time::Instant> = None; // first local arrival Instant
                let mut offset_ns: i128 = 0; // arrival_rel - server_rel
                let mut jitter_ewma_ns: f64 = 0.0;  // RFC3550 style EWMA of transit deltas (ns)
                let mut prev_transit: Option<i128> = None; // previous transit for jitter
                // Adaptive buffering
                let mut target_buffer_ns: u64 = 20_000_000; // start 20ms
                let mut max_buffer_ns: u64 = 80_000_000;    // start 80ms (will adjust)
                let _init_read = (target_buffer_ns, max_buffer_ns);
                let mut newest_ts: u64 = 0;
                // heap-based reorder buffer (min-heap via Reverse)
                #[derive(Debug)] struct BufFrame { ts_ns: u64, dur_ns: u64, data: Vec<f32> }
                impl PartialEq for BufFrame { fn eq(&self, other: &Self) -> bool { self.ts_ns == other.ts_ns } }
                impl Eq for BufFrame {}
                impl Ord for BufFrame { fn cmp(&self, other:&Self)->std::cmp::Ordering { self.ts_ns.cmp(&other.ts_ns) } }
                impl PartialOrd for BufFrame { fn partial_cmp(&self, other:&Self)->Option<std::cmp::Ordering>{ Some(self.cmp(other)) } }
                let mut heap: BinaryHeap<Reverse<BufFrame>> = BinaryHeap::new();
                let mut buffered_total_ns: u64 = 0;
                // Frame buffer reuse pool
                const POOL_CAPACITY: usize = 64;
                let mut frame_pool: Vec<Vec<f32>> = (0..POOL_CAPACITY).map(|_| Vec::with_capacity(2048)).collect();
                let _pool_recycled: u64 = 0; // 保留占位用于后续调试统计
                let mut late_drop_count: u64 = 0;
                let mut recv_seq: u64 = 0; let mut expected_seq: u64 = 0; let mut loss_acc: f64 = 0.0;
                let mut last_metrics_push = std::time::Instant::now();
                // Compute dynamic reorder delay (5ms base up to 40ms)
                fn compute_reorder_delay(jitter_ns: f64) -> u64 { let base=5_000_000f64; let scaled = (jitter_ns*2.5).max(base); scaled.min(40_000_000f64) as u64 }
                // Compute adaptive targets based on jitter
                fn adjust_targets(jitter_ns: f64) -> (u64,u64) {
                    // Map jitter to extra buffer: jitter 0..8ms -> add 0..20ms (cap 40ms total target)
                    let jitter_ms = jitter_ns/1_000_000.0;
                    let base_ms = 15.0; // slightly lower base to allow growth
                    let extra = (jitter_ms*2.5).clamp(0.0, 25.0); // up to +25ms
                    let target = (base_ms + extra).clamp(10.0, 40.0); // 10..40ms
                    let max = (target*2.0).clamp(30.0, 100.0); // max 100ms
                    ((target*1_000_000.0) as u64, (max*1_000_000.0) as u64)
                }
                while alive.load(Ordering::Relaxed) {
                    match udp_clone.recv_from(&mut buf) {
                        Ok((n,_src)) => {
                            if n < 22 { continue; }
                            if &buf[0..2] != &types::FRAME_MAGIC { continue; }
                            let seq = u32::from_be_bytes([buf[2],buf[3],buf[4],buf[5]]) as u64;
                            let fmt = buf[6]; let ch = buf[7] as u16; let sr = u32::from_be_bytes([buf[8],buf[9],buf[10],buf[11]]);
                            let payload_len = u16::from_be_bytes([buf[12],buf[13]]) as usize; // ciphertext length if encrypted
                            let ts_ns = u64::from_be_bytes([buf[14],buf[15],buf[16],buf[17],buf[18],buf[19],buf[20],buf[21]]);
                            if 22+payload_len > n { continue; }
                            let mut _payload_plain_owned: Option<Vec<u8>> = None; // decrypted buffer holder
                            let payload: &[u8] = if enc_enabled {
                                let ct = &buf[22..22+payload_len];
                                if let (Some(salt), Some(key)) = (enc_salt, enc_key) {
                                    let cipher = XChaCha20Poly1305::new(&key.into());
                                    let mut nonce = [0u8;24];
                                    nonce[..8].copy_from_slice(&salt);
                                    nonce[8..12].copy_from_slice(&(seq as u32).to_be_bytes());
                                    nonce[12..20].copy_from_slice(&ts_ns.to_be_bytes());
                    // AAD = first 22 bytes header (payload_len already ciphertext length on sender)
                    let aad = &buf[0..22];
                                    match cipher.decrypt(&nonce.into(), Payload { msg: ct, aad }) {
                                        Ok(pt) => { // 确认已加密状态 (仅一次)
                                            if enc_status.load(Ordering::Relaxed) != 1 { enc_status.store(1, Ordering::Relaxed); }
                                            _payload_plain_owned = Some(pt); _payload_plain_owned.as_ref().unwrap() }
                                        Err(e) => { decrypt_fail.fetch_add(1, Ordering::Relaxed); if enc_status.load(Ordering::Relaxed) != -1 { enc_status.store(-1, Ordering::Relaxed); eprintln!("[CLIENT][DEC] decrypt fail seq={seq}: {e}"); } continue; }
                                    }
                                } else { // No key yet derived
                                    if enc_status.load(Ordering::Relaxed) != 0 { enc_status.store(0, Ordering::Relaxed); }
                                    continue;
                                }
                            } else { &buf[22..22+payload_len] };
                            let now_inst = std::time::Instant::now();
                            // --- Clock alignment & latency ---
                            if base_server_ts.is_none() { base_server_ts = Some(ts_ns); base_client_instant = Some(now_inst); offset_ns = 0; }
                            let arrival_rel_ns: u64 = if let Some(base) = base_client_instant { now_inst.duration_since(base).as_nanos() as u64 } else { 0 };
                            let server_rel_ns: u64 = if let Some(base_s) = base_server_ts { ts_ns - base_s } else { 0 };
                            // (Optional) adjust offset on first few frames if negative delay appears
                            // expected arrival = server_rel + offset
                            let expected_arrival_rel = (server_rel_ns as i128) + offset_ns;
                            let mut delay_ns_i = (arrival_rel_ns as i128) - expected_arrival_rel;
                            if delay_ns_i < 0 { // clamp & gently pull offset toward eliminating negatives
                                offset_ns += delay_ns_i / 8; // small correction to reduce future negatives
                                delay_ns_i = 0;
                            }
                            let delay_ms = (delay_ns_i as f64)/1_000_000.0;
                            latency_acc += delay_ms; latency_samples += 1;
                            // transit for jitter (same basis: arrival_rel - server_rel - offset)
                            let transit = (arrival_rel_ns as i128) - (server_rel_ns as i128) - offset_ns;
                            if let Some(prev_t) = prev_transit { let mut d = transit - prev_t; if d < 0 { d = -d; } // |d|
                                if jitter_ewma_ns == 0.0 { jitter_ewma_ns = d as f64; } else { jitter_ewma_ns += (d as f64 - jitter_ewma_ns)/16.0; }
                            }
                            prev_transit = Some(transit);
                            // seq / loss update
                            if expected_seq==0 { expected_seq=seq; }
                            if seq>=expected_seq { let gap = seq - expected_seq; if gap>0 { // lost frames
                                    loss_acc += gap as f64;
                                }
                                expected_seq = seq + 1;
                            } else {
                                // out-of-order older frame (already handled by reorder), ignore for loss calc
                            }
                            recv_seq += 1;
                            // adaptive target buffer & caps
                            let (tgt, max_cap) = adjust_targets(jitter_ewma_ns);
                            target_buffer_ns = tgt; max_buffer_ns = max_cap;
                            // dynamic reorder delay
                            let reorder_delay = compute_reorder_delay(jitter_ewma_ns);
                            // late frame drop policy (severely late > 2*reorder_delay behind newest)
                            if newest_ts!=0 && ts_ns + 2*reorder_delay < newest_ts { late_drop_count += 1; continue; }
                            if ts_ns > newest_ts { newest_ts = ts_ns; }
                            // 解码到统一 f32
                            let mut frames: Vec<f32> = if let Some(mut reused)=frame_pool.pop(){ reused.clear(); reused } else { Vec::with_capacity(2048) };
                            match fmt {
                                types::FMT_F32 => { let cnt=payload_len/4; frames.reserve(cnt); for chunk in payload.chunks_exact(4).take(cnt){ let mut a=[0u8;4]; a.copy_from_slice(chunk); frames.push(f32::from_ne_bytes(a)); } },
                                types::FMT_I16 => { let cnt=payload_len/2; frames.reserve(cnt); for chunk in payload.chunks_exact(2).take(cnt){ let v=i16::from_le_bytes([chunk[0],chunk[1]]); frames.push(v as f32/32768.0); } },
                                types::FMT_U16 => { let cnt=payload_len/2; frames.reserve(cnt); for chunk in payload.chunks_exact(2).take(cnt){ let v=u16::from_le_bytes([chunk[0],chunk[1]]); frames.push((v as f32 - 32768.0)/32768.0); } },
                                _ => { if frame_pool.len()<POOL_CAPACITY { frame_pool.push(frames); } continue }
                            }
                            // Down-mix to mono if multi-channel
                            let effective = if ch>1 { let mut mono = if let Some(mut reused)=frame_pool.pop(){ reused.clear(); reused } else { Vec::with_capacity(frames.len()/ch as usize) }; for chunk in frames.chunks_exact(ch as usize){ let s: f32 = chunk.iter().copied().sum(); mono.push(s / ch as f32); } if frame_pool.len()<POOL_CAPACITY { frame_pool.push(frames); } mono } else { frames };
                            // RMS & peak (with decay)
                            if !effective.is_empty() { let mut acc=0f64; for &smp in &effective { acc += (smp as f64)*(smp as f64); } let rms=(acc/(effective.len() as f64)).sqrt(); metrics_rms.store(rms); // peak update
                                let prev_peak = metrics_peak.load();
                                let new_peak = if rms > prev_peak { rms } else { // 100ms metrics push cadence -> approximate 1% decay per 100ms
                                    prev_peak * 0.99
                                }; if (new_peak - prev_peak).abs() > 1e-12 { metrics_peak.store(new_peak); } }
                            let dur_ns = if sr>0 { ((effective.len() as u128)*1_000_000_000u128 / sr as u128) as u64 } else {0};
                            buffered_total_ns = buffered_total_ns.saturating_add(dur_ns);
                            heap.push(Reverse(BufFrame { ts_ns, dur_ns, data: effective }));
                            // Release frames while latency condition or overflow
                            let mut released = 0usize;
                            while let Some(Reverse(ref peek)) = heap.peek() {
                                let can_release = (peek.ts_ns + reorder_delay <= newest_ts && buffered_total_ns >= target_buffer_ns && heap.len()>2) || buffered_total_ns > max_buffer_ns;
                                if can_release {
                                    if let Some(Reverse(f)) = heap.pop() {
                                        buffered_total_ns = buffered_total_ns.saturating_sub(f.dur_ns);
                                        let mut out_vec = if let Some(mut reused)=frame_pool.pop(){ reused.clear(); reused } else { Vec::with_capacity(f.data.len()) };
                                        out_vec.extend_from_slice(&f.data);
                                        if tx.send(out_vec).is_err() { break; }
                                        if frame_pool.len()<POOL_CAPACITY { frame_pool.push(f.data); }
                                        released +=1;
                                    } else { break; }
                                } else { break; }
                            }
                            // Periodic stats (5s)
                            if last_stats_report.elapsed().as_secs() >= 5 { let avg_lat = if latency_samples>0 { latency_acc/(latency_samples as f64) } else {0.0}; println!("[CLIENT] stats: avg_lat={:.2}ms jitter={:.2}ms tgt={:.1}ms buf={:.1}ms max={:.1}ms heap={} rel={} late_drop={} rdelay={:.1}ms", avg_lat, jitter_ewma_ns/1_000_000.0, target_buffer_ns as f64/1_000_000.0, buffered_total_ns as f64/1_000_000.0, max_buffer_ns as f64/1_000_000.0, heap.len(), released, late_drop_count, reorder_delay as f64/1_000_000.0); latency_acc=0.0; latency_samples=0; last_stats_report=std::time::Instant::now(); if recv_seq==1 { println!("[CLIENT] first multicast frame seq={seq}"); } }
                            // Metrics update every 100ms
                            if last_metrics_push.elapsed().as_millis() >= 100 {
                                let avg_lat = if latency_samples>0 { latency_acc/(latency_samples as f64) } else { metrics_latency.load() };
                                metrics_latency.store(avg_lat);
                                metrics_jitter.store(jitter_ewma_ns/1_000_000.0);
                                // packet loss ratio = lost / (received + lost)
                                let lost = loss_acc; let total = (recv_seq as f64) + lost; if total>0.0 { metrics_loss.store(lost/total); }
                                metrics_late.store(late_drop_count as f64);
                                last_metrics_push = std::time::Instant::now();
                            }
                        }, Err(ref e) if e.kind()==std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(10)); }, Err(e) => { eprintln!("[CLIENT][UDP][ERR] recv: {e}"); break } }
                }
                // Drain remaining frames
                while let Some(Reverse(f)) = heap.pop() {
                    let out = f.data; if tx.send(out.clone()).is_err() { break; }
                    if frame_pool.len()<POOL_CAPACITY { frame_pool.push(out); }
                }
                eprintln!("[CLIENT][UDP] thread exit"); alive.store(false, Ordering::SeqCst);
            });
        }
    } else { println!("[CLIENT] No audio params yet; output not started"); }
    Ok(state)
}

/// Spawn audio output thread (f32 only).
fn spawn_output_thread(dev: cpal::Device, rx: Receiver<Vec<f32>>, running: Arc<AtomicBool>, params: AudioParams) -> CbSender<()> {
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
    thread::spawn(move || {
    let running_outer = running.clone();
    if let Ok(cfg) = dev.default_output_config() {
        let sample_format = cfg.sample_format();
        let config: cpal::StreamConfig = cfg.clone().into();
        match sample_format {
            cpal::SampleFormat::F32 => {
                let mut leftover: Vec<f32> = Vec::new();
                let out_channels = config.channels.max(1);
                let rx_clone = rx.clone();
                let in_channels = params.channels.max(1);
                // Jitter prebuffer: fill ~20ms before start
                let prebuffer_frames: usize = (params.sample_rate as f32 * 0.02) as usize; // 20ms
                let mut started = false;
                let mut underruns: u64 = 0; let mut last_report = std::time::Instant::now();
                let build_res = dev.build_output_stream(&config, move |out: &mut [f32], _| {
                    if !running.load(Ordering::Relaxed) { return; }
                    let needed_frames = out.len() / out_channels as usize;
                    if !started {
                        // Prebuffer phase: accumulate until threshold
                        while leftover.len() < prebuffer_frames {
                            match rx_clone.try_recv() { Ok(mut frames) => { leftover.append(&mut frames); }, Err(_) => break }
                        }
                        if leftover.len() >= prebuffer_frames {
                            started = true;
                            println!("[CLIENT] jitter buffer filled: {} frames (target {})", leftover.len(), prebuffer_frames);
                        } else {
                            // Not enough yet: keep filling, output silence
                            while leftover.len() < needed_frames {
                                match rx_clone.try_recv() { Ok(mut frames) => { leftover.append(&mut frames); }, Err(_) => break }
                            }
                            for s in out.iter_mut() { *s = 0.0; }
                            return;
                        }
                    } else {
                        // Steady state: ensure one callback worth of frames
                        while leftover.len() < needed_frames {
                            match rx_clone.try_recv() { Ok(mut frames) => { leftover.append(&mut frames); }, Err(_) => break }
                        }
                    }
                    let mut produced = 0usize;
                    for frame_index in 0..needed_frames {
                        if frame_index < leftover.len() { let sample_mono = leftover[frame_index];
                            // Upmix / downmix (currently mono already)
                            for ch in 0..out_channels { out[produced + ch as usize] = if in_channels==1 { sample_mono } else { sample_mono }; }
                            produced += out_channels as usize;
                        } else { // zero fill remainder
                            for ch in 0..out_channels { out[produced + ch as usize] = 0.0; }
                            produced += out_channels as usize;
                            underruns += 1;
                        }
                    }
                    // Consume frames
                    if needed_frames <= leftover.len() { leftover.drain(0..needed_frames); } else { leftover.clear(); }
                    if last_report.elapsed().as_secs_f32() > 5.0 { println!("[CLIENT] playback stats: leftover={} underruns={}", leftover.len(), underruns); last_report = std::time::Instant::now(); }
                }, move |e| eprintln!("[CLIENT][OUTPUT][ERR] {e}"), None);
                if let Ok(stream) = build_res { if let Err(e) = stream.play() { eprintln!("[CLIENT][OUTPUT][ERR] play: {e}"); } else { println!("[CLIENT][OUTPUT] stream started"); }
                    // Wait for stop
                    loop {
                        if !running_outer.load(Ordering::Relaxed) { break; }
                        if stop_rx.recv_timeout(Duration::from_millis(200)).is_ok() { break; }
                    }
                    if let Err(e) = stream.pause() { eprintln!("[CLIENT][OUTPUT] pause err: {e}"); } else { println!("[CLIENT][OUTPUT] stream paused"); }
                }
            }
            _ => { println!("[CLIENT] Unsupported output sample format: {:?}", sample_format); }
        }
    }
    println!("[CLIENT][OUTPUT] thread exit");
    });
    stop_tx
}

/// Periodic heartbeat + timeout detection + coordinated shutdown.
fn heartbeat_loop(stream_arc: Arc<std::sync::Mutex<TcpStream>>, key: String, connected: Arc<AtomicBool>, output_running: Arc<AtomicBool>, udp_alive: Arc<AtomicBool>, output_stop_tx: Arc<Mutex<Option<CbSender<()>>>>, reason: Arc<Mutex<Option<String>>>, event_sender: Option<EventSender<String>>) {
    use std::io::{Write, Read};
    let mut buf = [0u8; 256];
    let mut last_ok = std::time::Instant::now();
    const HEART_INTERVAL: Duration = Duration::from_secs(1);
    const HEART_TIMEOUT: Duration = Duration::from_secs(5); // 超过 5 秒未收到 OK 认为超时
    while connected.load(Ordering::Relaxed) {
        if let Ok(mut stream) = stream_arc.lock() {
            let _ = stream.write_all(format!("HEART {key}\n").as_bytes());
            match stream.read(&mut buf) {
                Ok(0) => { println!("[CLIENT][HEART] server closed"); if let Ok(mut r)=reason.lock(){ let msg: String = "服务器连接关闭".into(); *r=Some(msg.clone()); if let Some(ref tx)=event_sender { let _=tx.send(format!("DISCONNECT:{msg}")); } } connected.store(false, Ordering::SeqCst); break; },
                Ok(n) => {
                    let s = String::from_utf8_lossy(&buf[..n]);
                    if s.contains("SERVER_STOP") { println!("[CLIENT] server stop detected"); if let Ok(mut r)=reason.lock(){ let msg: String = "服务器已停止".into(); *r=Some(msg.clone()); if let Some(ref tx)=event_sender { let _=tx.send(format!("DISCONNECT:{msg}")); } } connected.store(false, Ordering::SeqCst); break; }
                    if s.contains("OK") { last_ok = std::time::Instant::now(); }
                },
                Err(e) if e.kind()==std::io::ErrorKind::WouldBlock => { /* no data this round */ },
                Err(e) => { eprintln!("[CLIENT][HEART] read err: {e}"); }
            }
        }
        if last_ok.elapsed() > HEART_TIMEOUT {
            println!("[CLIENT][HEART] timeout > {}s -> disconnect", HEART_TIMEOUT.as_secs()); if let Ok(mut r)=reason.lock(){ let msg=format!("心跳超时{}s", HEART_TIMEOUT.as_secs()); *r=Some(msg.clone()); if let Some(ref tx)=event_sender { let _=tx.send(format!("DISCONNECT:{msg}")); } }
            connected.store(false, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(HEART_INTERVAL);
    }
    // trigger full stop for output & udp
    output_running.store(false, Ordering::SeqCst);
    udp_alive.store(false, Ordering::SeqCst);
    if let Ok(mut guard) = output_stop_tx.lock() { if let Some(tx)=guard.take() { let _ = tx.send(()); } }
    if let Ok(mut stream) = stream_arc.lock() { let _ = stream.write_all(b"DISCONNECT\n"); }
}

/// Manual disconnect sequence.
pub fn disconnect(state: &ClientState) {
    state.connected.store(false, Ordering::SeqCst);
    state.output_running.store(false, Ordering::SeqCst);
    state.udp_thread_alive.store(false, Ordering::SeqCst);
    if let Ok(mut guard)=state.output_stop_tx.lock() { if let Some(tx)=guard.take() { let _ = tx.send(()); } }
    if let Ok(mut r)=state.disconnection_reason.lock() { if r.is_none() { *r=Some("手动断开".into()); } }
    if let Some(ctrl) = &state.ctrl { if let Ok(mut s)=ctrl.lock() { let _ = s.write_all(b"DISCONNECT\n"); } }
}
