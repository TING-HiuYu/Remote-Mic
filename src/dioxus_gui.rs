//! Dioxus desktop GUI.
use crate::{audio, buffers::AudioBufferPool, client, lang, server};
use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::unbounded;
use dioxus::prelude::*;
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

// 全局深色扁平主题 CSS (设计令牌 + 扁平化，无大阴影)
const GLOBAL_DARK_CSS: &str = r#":root {
    color-scheme: dark;
    --color-bg: #111213;
    --color-bg-alt: #161718;
    --color-panel: #1d1f21;
    --color-panel-alt: #222527;
    --color-border: #272a2d;
    --color-border-hover: #33373b;
    --color-text: #dddddd;
    --color-text-dim: #9aa0a6;
    --color-accent: #3d82f7;
    --color-accent-hover: #4d8eff;
    --color-danger: #d9534f;
    --radius-sm: 4px;
    --radius-md: 8px;
    --radius-lg: 12px;
    --focus-ring: 0 0 0 2px rgba(61,130,247,0.35);
    --transition: .16s cubic-bezier(.4,0,.2,1);
    --shadow-elev-1: 0 0 0 1px var(--color-border);
}
html,body { background:var(--color-bg); color:var(--color-text); font-family: 'Inter', 'SF Pro Text', 'Segoe UI', Arial, Helvetica, sans-serif; -webkit-font-smoothing:antialiased; }
body,div,span,label { box-sizing:border-box; }
input,select,textarea { background:var(--color-panel); color:var(--color-text); border:1px solid var(--color-border); border-radius:var(--radius-sm); padding:6px 8px; font-size:13px; font-family:inherit; line-height:1.25; transition:var(--transition); }
input[readonly] { background:var(--color-bg-alt); color:var(--color-text-dim); }
input:hover,select:hover,textarea:hover { border-color:var(--color-border-hover); }
input:focus,select:focus,textarea:focus { outline:none; border-color:var(--color-accent); box-shadow:var(--focus-ring); }
button { background:var(--color-panel); color:var(--color-text); border:1px solid var(--color-border); border-radius:var(--radius-sm); padding:6px 14px; font-size:13px; cursor:pointer; font-weight:500; letter-spacing:.2px; display:inline-flex; align-items:center; justify-content:center; gap:6px; transition:var(--transition); text-align:center; }
button:hover { background:var(--color-panel-alt); border-color:var(--color-border-hover); }
button:active { transform:translateY(1px); }
button:focus { outline:none; box-shadow:var(--focus-ring); }
button:disabled { opacity:.45; cursor:not-allowed; }
select { appearance:none; -webkit-appearance:none; background:var(--color-panel); background-image:url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='14' height='14' viewBox='0 0 24 24' fill='none' stroke='%23bfc5cc' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><polyline points='6 9 12 15 18 9'/></svg>"); background-repeat:no-repeat; background-position:right 8px center; background-size:14px 14px; padding-right:30px; position:relative; cursor:pointer; }
select:hover { border-color:var(--color-border-hover); }
select:focus { border-color:var(--color-accent); box-shadow:var(--focus-ring); }
select option { background:var(--color-panel); color:var(--color-text); }
.panel input,.panel select { width:auto; }
::-webkit-scrollbar { width:10px; height:10px; }
::-webkit-scrollbar-track { background:var(--color-bg-alt); }
::-webkit-scrollbar-thumb { background:#2b2f32; border-radius:6px; border:2px solid var(--color-bg-alt); }
::-webkit-scrollbar-thumb:hover { background:#3a4044; }
.panel { background:var(--color-panel); border:1px solid var(--color-border); border-radius:var(--radius-lg); padding:14px 16px 12px 16px; position:relative; gap:10px; }
.panel-title { font-weight:600; font-size:13px; letter-spacing:.5px; text-transform:uppercase; color:var(--color-text-dim); position:absolute; top:-10px; left:14px; padding:0 10px; background:var(--color-bg); border:1px solid var(--color-border); border-radius:20px; line-height:20px; }
.metric-bar { background:#2a2d30; border-radius:6px; height:14px; position:relative; overflow:hidden; }
.metric-bar-fill { position:absolute; inset:0; width:0; background:linear-gradient(90deg,#2e8b57,#f0ad4e,#d9534f); transition:width .12s linear; }
.metric-peak { position:absolute; top:0; bottom:0; width:2px; background:#fff; opacity:.9; box-shadow:0 0 4px #fff; }
.client-item { background:var(--color-panel-alt); border:1px solid var(--color-border); border-radius:var(--radius-sm); padding:6px 8px; font-size:12px; display:flex; align-items:center; gap:12px; transition:var(--transition); }
.client-item:hover { border-color:var(--color-border-hover); }
.vol-numbers { font-size:11px; color:var(--color-text-dim); }
table { border-collapse:collapse; }
/* label 与输入控件的水平留白 */
.panel span + input,
.panel span + select,
.panel span + textarea { margin-left:6px; }
#root span + select { margin-left:6px; }
"#;

/// Launch the desktop application.
pub fn run() -> anyhow::Result<()> {
    dioxus_desktop::launch::launch(
        app,
        vec![],
        vec![Box::new(dioxus_desktop::Config::default())],
    );
}

/// Top-level application state mirrored into the UI.
struct AppState {
    current_lang: String,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    sel_input: usize,
    sel_output: usize,
    server_ip_list: Vec<String>,
    sel_server_ip: usize,
    server_port: u16,
    server_running: bool,
    server_state: server::ServerState,
    buffer_pool: Arc<AudioBufferPool>,
    client_state: Option<client::ClientState>,
    client_server_ip: String,
    client_server_port: String,
    error_message: Option<String>,
    event_rx: Option<UnboundedReceiver<String>>, // 客户端事件接收
    metrics_tick: Instant,
    mic_test_done: bool,
    mic_available: bool,
    net_test_done: bool,
    net_available: bool,
    server_psk: String,        // 服务器预共享密钥输入
    client_psk: String,        // 客户端预共享密钥输入
}

impl AppState {
    /// Collect initial devices, network interfaces and allocate buffer pool.
    fn new() -> Self {
        let (inputs, outputs) = audio::list_devices()
            .map(|(i, o)| {
                (
                    i.into_iter().map(|d| audio::device_name(&d)).collect(),
                    o.into_iter().map(|d| audio::device_name(&d)).collect(),
                )
            })
            .unwrap_or((vec![], vec![]));
        let mut ips: Vec<String> = get_if_addrs::get_if_addrs()
            .map(|ifs| {
                let mut v: Vec<String> = ifs
                    .into_iter()
                    .filter_map(|i| {
                        let ip = i.ip();
                        if ip.is_ipv4() {
                            Some(ip.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                v.sort();
                v.dedup();
                v
            })
            .unwrap_or_else(|_| vec![]);
        if !ips.iter().any(|s| s == "0.0.0.0") {
            ips.insert(0, "0.0.0.0".into());
        }
        // 选择第一个既不是 0.0.0.0 也不是 127.0.0.1 的地址作为默认
        let default_sel = ips
            .iter()
            .enumerate()
            .find_map(|(i, ip)| {
                if ip != "0.0.0.0" && ip != "127.0.0.1" {
                    Some(i)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let port = crate::net::pick_free_port().unwrap_or(50000);
    let pool = AudioBufferPool::new(64);
        let (_tx, _rx) = unbounded::<usize>();
        Self {
            current_lang: "zh".into(),
            input_devices: inputs,
            output_devices: outputs,
            sel_input: 0,
            sel_output: 0,
            server_ip_list: ips,
            sel_server_ip: default_sel,
            server_port: port,
            server_running: false,
            server_state: server::ServerState::new(),
            buffer_pool: pool,
            // previously used audio buffer notification channels (now managed server-side)
            client_state: None,
            client_server_ip: String::new(),
            client_server_port: String::new(),
            error_message: None,
            event_rx: None,
            metrics_tick: Instant::now(),
            mic_test_done: false,
            mic_available: false,
            net_test_done: false,
            net_available: false,
            server_psk: String::new(),
            client_psk: String::new(),
        }
    }
}

/// Root component constructing the full interface and spawning async refresh tasks.
fn app() -> Element {
    let mut st: Signal<AppState> = use_signal(AppState::new);
    // Capability detection trigger (microphone / LAN)
    let cap_trigger = use_signal(|| 0u64);
    {
        let mut st_detect = st.clone();
    let trig_val = *cap_trigger.read(); // dependency anchor
        use_future(move || async move {
            let _ = trig_val; // silence unused
            // Microphone check: enumerate and open default input config
            let mic_ok = match audio::list_devices() {
                Ok((inputs, _)) => {
                    if let Some(dev) = inputs.into_iter().next() { dev.default_input_config().is_ok() } else { false }
                }
                Err(_) => false,
            };
            // LAN check: UDP bind + broadcast
            let net_ok = {
                use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
                let bind_res = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
                if let Ok(sock) = bind_res {
                    let _ = sock.set_broadcast(true);
                    sock.send_to(&[0u8; 4], SocketAddrV4::new(Ipv4Addr::BROADCAST, 65535)).is_ok()
                } else { false }
            };
            let mut w = st_detect.write();
            w.mic_test_done = true;
            // Clear previous microphone error if now available
            if !w.mic_available && mic_ok && w.error_message.as_deref().map_or(false, |m| m.contains("Microphone")) {
                w.error_message = None;
            }
            w.mic_available = mic_ok;
            w.net_test_done = true;
            w.net_available = net_ok;
            if !mic_ok && w.error_message.is_none() {
                w.error_message = Some("Microphone unavailable: permission denied or no input device".into());
            }
        });
    }
    // 客户端列表刷新 tick（仅用于展示服务器当前连接）
    let clients_tick = use_signal(|| 0u64);
    {
        let tick_sig = clients_tick.clone();
        use_future(move || async move {
            let mut t = tick_sig;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                *t.write() += 1; // 触发重渲染
            }
        });
    }
    // 事件驱动：后台异步监听客户端事件通道
    {
        let mut st_events = st.clone();
        use_future(move || async move {
            loop {
                // 尝试取出一个接收器（只取一次）
                let rx_opt = { st_events.write().event_rx.take() };
                if let Some(mut rx) = rx_opt {
                    while let Some(msg) = rx.recv().await {
                        if let Some(rest) = msg.strip_prefix("DISCONNECT:") {
                            {
                                let mut w = st_events.write();
                                if w.error_message.is_none() {
                                    w.error_message = Some(format!(
                                        "{}{rest}",
                                        lang::tr("client.disconnected.prefix")
                                    ));
                                }
                                w.client_state = None; // 清理状态
                            }
                        }
                    }
                } else {
                    // 没有接收器，等待后重试
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        });
    }
    let tr = |k: &str| lang::tr(k);
    let stage = st.read().server_state.stage.load(Ordering::Relaxed);
    let status_key = match stage {
        0 => "server.status.stopped",
        1 => "server.status.listening",
        2 => "server.status.audio_ready",
        _ => "server.status.running",
    };
    // 读取 tick 以建立依赖 (用于刷新已连接客户端列表)
    let _clients_tick_now = *clients_tick.read();
    let connected = st
        .read()
        .client_state
        .as_ref()
        .map(|c| c.connected.load(Ordering::Relaxed))
        .unwrap_or(false);
    let mut st_clone = st.clone();
    // metrics 100ms refresh loop
    {
        let mut st_metrics = st.clone();
        use_future(move || async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                // Just trigger rerender
                st_metrics.write().metrics_tick = Instant::now();
            }
        });
    }
    // 动态窗口标题：根据当前语言刷新 (桌面环境)
    let window = dioxus_desktop::use_window();
    {
        let _st_lang = st.clone(); // 读取以建立依赖
        let win = window.clone();
        use_effect(move || {
            let title = lang::tr("app.title");
            win.set_title(&title);
        });
    }
    return rsx! {
        div {
            style: "padding:12px;font-family:Arial,sans-serif;font-size:14px;max-width:780px;display:flex;flex-direction:column;gap:16px;background:#111;min-height:100vh;color:#ddd;",
            style { {GLOBAL_DARK_CSS} },
                { st.read().error_message.as_ref().map(|msg| rsx!(
                    div { style: "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,0.55);z-index:999;",
                        div { style: "background:#1e1e1e;padding:16px 20px;border-radius:8px;min-width:320px;max-width:480px;box-shadow:0 4px 18px rgba(0,0,0,0.6);display:flex;flex-direction:column;gap:12px;color:#ddd;",
                            h3 { style: "margin:0;font-size:16px;color:#ff5555;", { tr("dialog.error.title") } }
                            pre { style: "white-space:pre-wrap;margin:0;font-size:12px;color:#ccc;", "{msg}" }
                            div { style: "display:flex;justify-content:flex-end;gap:8px;",
                                button { style:"background:#333;color:#eee;border:1px solid #555;padding:6px 14px;border-radius:4px;cursor:pointer;", onclick: move |_| { st.write().error_message=None; }, "OK" }
                            }
                        }
                    }
                )) }
                // Settings panel
                div { class: "panel", style: panel_style(),
                    // floating title
                    div { style: panel_title_style(), {tr("group.setting")} }
                    { let st_read = st.read(); if st_read.mic_test_done && !st_read.mic_available { Some(rsx!(div { style:"font-size:11px;color:#ff7676;background:#2a1212;border:1px solid #5c2323;padding:6px 8px;border-radius:6px;", "Microphone not accessible: allow in OS privacy settings." })) } else { None } }
                    { let st_read = st.read(); if st_read.net_test_done && !st_read.net_available { Some(rsx!(div { style:"font-size:11px;color:#ffbb55;background:#33240f;border:1px solid #5b4018;padding:6px 8px;border-radius:6px;", "LAN may be restricted: check firewall (Windows may need allow)." })) } else { None } }
                    { let st_read = st.read(); if st_read.mic_test_done || st_read.net_test_done { Some(rsx!(div { style:"display:flex;align-items:center;gap:14px;flex-wrap:wrap;margin:4px 0 2px 0;font-size:11px;color:#bbb;", 
                        div { style:"display:flex;align-items:center;gap:6px;", 
                            span { {tr("setting.mic")} }
                            span { style: format!("padding:2px 6px;border-radius:4px;background:{};color:#fff;", if st_read.mic_available {"#216e39"} else {"#b60205"}),
                                { if st_read.mic_available { "OK" } else { "Unavailable" } }
                            }
                        }
                        div { style:"display:flex;align-items:center;gap:6px;", 
                            span { {tr("setting.lan")} }
                            span { style: format!("padding:2px 6px;border-radius:4px;background:{};color:#fff;", if st_read.net_available {"#216e39"} else {"#b60205"}),
                                { if st_read.net_available { "OK" } else { "Limited" } }
                            }
                        }
                        { let mut cap_sig = cap_trigger.clone(); rsx!( button { style:"font-size:11px;padding:4px 10px;border-radius:4px;", onclick: move |_| { let mut w = cap_sig.write(); *w += 1; }, "Retest" } ) }
                    })) } else { None } }
                    div { style: "display:grid;grid-template-columns:1fr 1fr;column-gap:28px;row-gap:12px;align-items:start;",
                        // Left column: input & output devices stacked
                        div { style: "display:flex;flex-direction:column;gap:10px;",
                            div { style: "display:flex;align-items:center;gap:8px;", 
                                span { style: "font-size:12px;color:#bbb;display:inline-block;width:90px;", {tr("audio.input_device")} }
                                select { value: st.read().sel_input.to_string(), disabled: st.read().server_running, oninput: move |e| { if let Ok(v)=e.value().parse::<usize>() { st.write().sel_input=v; } },
                                    { st.read().input_devices.iter().enumerate().map(|(i,name)| rsx!( option { key: "in{i}", value: i.to_string(), "{name}" } )) }
                                }
                            }
                            div { style: "display:flex;align-items:center;gap:8px;", 
                                span { style: "font-size:12px;color:#bbb;display:inline-block;width:90px;", {tr("audio.output_device")} }
                                select { value: st.read().sel_output.to_string(), disabled: connected, oninput: move |e| { if let Ok(v)=e.value().parse::<usize>() { st.write().sel_output=v; } },
                                    { st.read().output_devices.iter().enumerate().map(|(i,name)| rsx!( option { key: "out{i}", value: i.to_string(), "{name}" } )) }
                                }
                            }
                        }
                        // Right column: language + virtual mic guide
                        div { style: "display:flex;flex-direction:column;gap:10px;",
                            button { style: "width:100%;", onclick: move |_| {
                                let msg = tr("dialog.virtual_mic");
                                std::thread::spawn(move || {
                                    let _ = rfd::MessageDialog::new()
                                        .set_title("Info")
                                        .set_description(msg)
                                        .set_level(rfd::MessageLevel::Info)
                                        .set_buttons(rfd::MessageButtons::Ok)
                                        .show();
                                });
                            }, { tr("audio.install_virtual_mic") } }
                            div { style: "display:flex;align-items:center;gap:8px;", 
                                span { style: "font-size:12px;color:#bbb;", {tr("lang.current")} }
                                select { value: st.read().current_lang.clone(), oninput: move |e| {
                                        let new = e.value().to_string();
                                        if new != st.read().current_lang {
                                            lang::reload_lang(&new);
                                            st.write().current_lang = new;
                                            let title = lang::tr("app.title");
                                            window.set_title(&title);
                                        }
                                    },
                                    { let list = lang::available_langs(); rsx!( { list.into_iter().map(|c| {
                                            let label = lang::lang_display(&c);
                                            rsx!( option { value: "{c}", "{label}" } )
                                        }) } ) }
                                }
                            }
                        }
                    }
                }
            div { style: "display:flex;flex-direction:row;gap:16px;width:100%;align-items:flex-start;",
                // Left: server side (panel + clients list)
                div { style: "flex:1;display:flex;flex-direction:column;gap:8px;min-width:0;",
                    div { class: "panel", style: format!("{}flex:1;", panel_style()),
                        div { style: panel_title_style(), {tr("group.server")} }
                        // Server controls
                        div { style: "display:grid;grid-template-columns:auto auto 1fr;column-gap:12px;row-gap:8px;align-items:center;",
                            // Row 1: IP
                            span { style: "font-size:12px;color:#bbb;", {tr("server.ip")} }
                            select { style: "width:130px;", value: st.read().sel_server_ip.to_string(), disabled: st.read().server_running, oninput: move |e| { if let Ok(v)=e.value().parse::<usize>() { st.write().sel_server_ip=v; } },
                                { st.read().server_ip_list.iter().enumerate().map(|(i,ip)| rsx!( option { key: "ip{i}", value: i.to_string(), "{ip}" } )) }
                            }
                            // Buttons container (right side, single row)
                            div { style: "display:flex;flex-direction:column;gap:8px;justify-self:end;align-self:start;", 
                                if !st.read().server_running {
                                    button { onclick: move |_| { if let Err(e)=start_server(st_clone.clone()) { st_clone.write().error_message=Some(format!("启动服务器失败: {e}")); } }, {tr("server.start")} }
                                }
                                if st.read().server_running {
                                    button { onclick: move |_| { let srv_state = st.read().server_state.clone(); server::stop_server(&srv_state); st.write().server_running=false; }, {tr("server.stop")} }
                                }
                            }
                            // Row 2: Port
                            span { style: "font-size:12px;color:#bbb;", {tr("server.port")} }
                            input { style: "width:60px;", readonly: true, value: st.read().server_port.to_string(), oninput: move |e| { if let Ok(v)=e.value().parse() { st.write().server_port=v; } } }
                            div {} // 占位: 让下一行从新行开始
                            // Row 3: PSK (3 cells -> label, input, placeholder)
                            span { style: "font-size:12px;color:#bbb;", { tr("server.psk") } }
                            input { style: "width:130px;", r#type: "password", placeholder: "(可选)", value: st.read().server_psk.clone(), disabled: st.read().server_running, oninput: move |e| { st.write().server_psk = e.value().to_string(); } }
                            div {}
                        }
                        // Server metrics panel (audio params + volume + clients)
                        { let server_running = st.read().server_running; let srv_state = st.read().server_state.clone();
                          if server_running {
                              let params_opt = srv_state.audio_params.lock().clone();
                              let rms = srv_state.current_rms.load();
                              let db = if rms>0.0 { 20.0 * rms.log10() } else { -60.0 }; let norm = (rms.sqrt()).min(1.0);
                              let now = Instant::now();
                              let clients: Vec<(String, Option<u16>, u64)> = srv_state.clients.iter().map(|c| { let age = now.duration_since(c.last_seen).as_secs(); (c.addr.to_string(), c.udp_port, age) }).collect();
                              rsx!(div { style: "margin-top:8px;padding:8px;border:1px solid #2e2e2e;border-radius:6px;display:flex;flex-direction:column;gap:6px;background:#181818;",
                                  div { style: "font-size:12px;font-weight:600;color:#bbb;", { tr("server.metrics.title") } }
                                  { if let Some(p)=params_opt { let fmt_str = match p.sample_format { cpal::SampleFormat::F32=>"f32", cpal::SampleFormat::I16=>"i16", cpal::SampleFormat::U16=>"u16", _=>"f32"}; let enc_active = st.read().server_state.key_bytes.is_some(); let enc_lbl = if enc_active { tr("enc.enabled") } else { tr("enc.disabled") }; rsx!(div { style: "font-size:11px;color:#aaa;display:flex;flex-wrap:wrap;gap:12px;align-items:center;",
                                      span { { format!("SR:{}", p.sample_rate) } }
                                      span { { format!("CH:{}", p.channels) } }
                                      span { { format!("FMT:{}", fmt_str) } }
                                      span { style: format!("padding:2px 6px;border-radius:4px;background:{};color:#fff;font-size:10px;letter-spacing:.5px;", if enc_active { "#216e39" } else { "#555" }), "{enc_lbl}" }
                                  }) } else { rsx!(div { style: "font-size:11px;color:#666;", { tr(status_key) } }) } }
                                  { let peak = srv_state.peak_rms.load(); let peak_norm = (peak.sqrt()).min(1.0); rsx!(div { style: "display:flex;align-items:center;gap:8px;",
                                      span { style: "font-size:12px;min-width:70px;color:#bbb;", { tr("server.metrics.volume") } }
                                      div { style: "flex:1;height:12px;background:#2d2d2d;border-radius:4px;overflow:hidden;position:relative;",
                                          div { style: format!("position:absolute;left:0;top:0;bottom:0;width:{:.2}%;background:linear-gradient(90deg,#2e8b57,#f0ad4e,#d9534f);", norm*100.0) }
                                          div { style: format!("position:absolute;top:0;bottom:0;left:calc({:.2}% - 1px);width:2px;background:#fff;opacity:0.9;box-shadow:0 0 4px #fff;", peak_norm*100.0) }
                                      }
                                      span { style: "font-size:11px;width:70px;text-align:right;color:#ccc;", { format!("{:.3} RMS", rms) } }
                                      span { style: "font-size:11px;width:60px;text-align:right;color:#ccc;", { format!("{:.1} dB", db) } }
                                  }) }
                                  { if !clients.is_empty() { let total = clients.len(); rsx!(div { style: "display:flex;flex-direction:column;gap:4px;",
                                          div { style: "font-size:12px;color:#bbb;font-weight:600;", { format!("{} ({total})", tr("server.connected_clients")) } }
                                          div { style: "max-height:120px;overflow-y:auto;display:flex;flex-direction:column;gap:4px;",
                                              { clients.into_iter().enumerate().map(|(i,(addr,_udp,_age))| rsx!(div { key: "cli{i}", style: "font-size:12px;padding:4px 6px;border:1px solid #333;border-radius:4px;background:#222;display:flex;gap:12px;align-items:center;",
                                                  span { style: "min-width:150px;color:#ddd;", "{addr}" }
                                              }) ) }
                                          }
                                      }) } else { rsx!(div { style: "font-size:12px;color:#555;", { tr("server.no_clients") } }) } }
                              })
                          } else {
                              rsx!(div { style: "margin-top:8px;font-size:12px;color:#555;", { tr("server.status.stopped") } })
                          }
                        }
                    }
                }
                // Right: client side
                div { style: "flex:1;display:flex;flex-direction:column;gap:8px;min-width:0;",
                    div { class: "panel", style: format!("{}flex:1;", panel_style()),
                        div { style: panel_title_style(), {tr("group.client")} }
                        div { style: "display:grid;grid-template-columns:auto auto 1fr;column-gap:12px;row-gap:8px;align-items:center;",
                            // Row 1: server_ip
                            span { style: "font-size:12px;color:#bbb;", {tr("client.server_ip")} }
                            input { style: "width:130px;", value: st.read().client_server_ip.clone(), disabled: connected, maxlength: "15", oninput: move |e| {
                                    let mut v: String = e.value().chars().filter(|c| c.is_ascii_digit() || *c=='.').collect();
                                    if v.len() > 15 { v.truncate(15); }
                                    st.write().client_server_ip = v;
                                } }
                            // Buttons right side single row
                            div { style: "display:flex;flex-direction:column;gap:8px;justify-self:end;align-self:start;",
                                if !connected { button { onclick: move |_| {
                                        let snapshot = st.read();
                                        let ip = snapshot.client_server_ip.clone();
                                        let port_str = snapshot.client_server_port.clone();
                                        let sel_out = snapshot.sel_output; drop(snapshot);
                                        let ip_trim = ip.trim().to_string(); let port_trim = port_str.trim().to_string();
                                        if ip_trim.is_empty() || port_trim.is_empty() { let mut w = st.write(); w.error_message = Some(tr("error.client.missing_fields")); return; }
                                        if ip_trim.parse::<std::net::IpAddr>().is_err() { let mut w = st.write(); w.error_message = Some(tr("error.client.invalid_ip")); return; }
                                        let port: u16 = match port_trim.parse() { Ok(p) if p>0 => p, _ => { let mut w = st.write(); w.error_message = Some(tr("error.client.invalid_port")); return; } };
                                        let (ev_tx, ev_rx) = unbounded_channel();
                                        let psk_opt = { let p = st.read().client_psk.clone(); if p.trim().is_empty() { None } else { Some(p) } };
                                        match client::connect_with_output(ip_trim, port, sel_out, psk_opt, Some(ev_tx)) { Ok(cs)=> { let mut w=st.write(); w.client_state=Some(cs); w.event_rx=Some(ev_rx); }, Err(e)=> { let mut w=st.write(); w.error_message=Some(format!("连接服务器失败: {e}")); } }
                                    }, {tr("client.connect")} } }
                                if connected { button { onclick: move |_| { if let Some(cs)=&st.read().client_state { client::disconnect(cs); } st.write().client_state=None; }, {tr("client.disconnect")} } }
                            }
                            // Row 2: server_port
                            span { style: "font-size:12px;color:#bbb;", {tr("client.server_port")} }
                            input { style: "width:60px;", value: st.read().client_server_port.clone(), disabled: connected, maxlength: "5", oninput: move |e| { let mut v = e.value().to_string(); if v.len() > 5 { v.truncate(5); } st.write().client_server_port = v; } }
                            div {} // 占位防止 PSK 挤在同一行
                            // Row 3: PSK
                            span { style: "font-size:12px;color:#bbb;", { tr("client.psk") } }
                            input { style: "width:130px;", r#type: "password", placeholder: "(可选)", value: st.read().client_psk.clone(), disabled: connected, oninput: move |e| { st.write().client_psk = e.value().to_string(); } }
                            div {}
                        }
                        // Metrics panel
                        { if let Some(cs)=&st.read().client_state { rsx!(div { style: "margin-top:8px;padding:8px;border:1px solid #2e2e2e;border-radius:6px;display:flex;flex-direction:column;gap:6px;background:#181818;",
                            div { style: "font-size:12px;font-weight:600;color:#bbb;", { tr("client.metrics.title") } }
                            { // server audio params row
                              if let Some(p)=&cs.params {
                                  let fmt_str = match p.sample_format { cpal::SampleFormat::F32 => "f32", cpal::SampleFormat::I16 => "i16", cpal::SampleFormat::U16 => "u16", _=>"f32"};
                                  // 三种状态: 成功(绿色) / 失败(红色: 服务器加密而本地未派生) / 未加密(灰色)
                                  // 优先使用后端共享的整数状态 (避免多线程频繁推送修改)
                                  let status_val = cs.enc_status.load(Ordering::Relaxed);
                                  let (enc_lbl, color) = match status_val {
                                      -1 => (tr("enc.auth_failed"), "#b60205"),
                                      1 => (tr("enc.enabled"), "#216e39"),
                                      _ => (tr("enc.disabled"), if st.read().server_state.key_bytes.is_some() { "#b60205" } else { "#555" }),
                                  };
                                  rsx!(div { style: "font-size:11px;color:#444;display:flex;flex-wrap:wrap;gap:12px;align-items:center;",
                                      span { { format!("SR:{}", p.sample_rate) } }
                                      span { { format!("CH:{}", p.channels) } }
                                      span { { format!("FMT:{}", fmt_str) } }
                                      span { style: format!("padding:2px 6px;border-radius:4px;background:{};color:#fff;font-size:10px;letter-spacing:.5px;", color), "{enc_lbl}" }
                                  })
                              } else { rsx!(div {}) }
                            }
                            // volume bar
                            { let rms = cs.current_rms.load(); let peak = cs.peak_rms.load(); let db = if rms>0.0 { 20.0 * rms.log10() } else { -60.0 }; let norm = (rms.sqrt()).min(1.0); let peak_norm = (peak.sqrt()).min(1.0); rsx!(div { style: "display:flex;align-items:center;gap:8px;",
                                span { style: "font-size:12px;min-width:60px;color:#bbb;", { tr("client.metrics.volume") } }
                                div { style: "flex:1;height:12px;background:#2d2d2d;border-radius:4px;overflow:hidden;position:relative;",
                                    div { style: format!("position:absolute;left:0;top:0;bottom:0;width:{:.2}%;background:linear-gradient(90deg,#2e8b57,#f0ad4e,#d9534f);", norm*100.0) }
                                    div { style: format!("position:absolute;top:0;bottom:0;left:calc({:.2}% - 1px);width:2px;background:#fff;opacity:0.9;box-shadow:0 0 4px #fff;", peak_norm*100.0) }
                                }
                                span { style: "font-size:11px;width:70px;text-align:right;color:#ccc;", { format!("{:.2} RMS", rms) } }
                                span { style: "font-size:11px;width:60px;text-align:right;color:#ccc;", { format!("{:.1} dB", db) } }
                            }) }
                            { let lat = cs.avg_latency_ms.load(); let jit = cs.jitter_ms.load(); let loss = cs.packet_loss.load()*100.0; let late = cs.late_drop.load(); rsx!(div { style: "display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:4px;font-size:12px;",
                                div { { format!("{}: {:.2}", tr("client.metrics.latency"), lat) } }
                                div { { format!("{}: {:.2}", tr("client.metrics.jitter"), jit) } }
                                div { { format!("{}: {:.3}%", tr("client.metrics.loss"), loss) } }
                                div { { format!("{}: {}", tr("client.metrics.late"), late as u64) } }
                            }) }
                        }) } else { rsx!(div { }) } }
                    }
                }
            }
        }
    };
}

/// Start server threads + audio input for selected device.
fn start_server(mut st: Signal<AppState>) -> Result<()> {
    let ip = st
        .read()
        .server_ip_list
        .get(st.read().sel_server_ip)
        .cloned()
        .unwrap_or("0.0.0.0".into());
    let port = st.read().server_port;
    println!("[SERVER] start {ip}:{port}");
    let pool = st.read().buffer_pool.clone();
    let (tx, rx_local) = unbounded();
    let mut srv_state = st.read().server_state.clone();
    // 若用户输入了 PSK, 启用加密
    let psk_opt = st.read().server_psk.clone();
    if !psk_opt.trim().is_empty() {
        srv_state.enable_psk(psk_opt.trim().to_string());
    }
    // 将更新后的加密配置写回 GUI 状态，确保界面能读取 key_bytes
    {
        let mut w = st.write();
        w.server_state = srv_state.clone();
    }
    server::start_server(srv_state.clone(), ip.clone(), port, pool.clone(), rx_local)?;
    st.write().server_running = true;
    // Capture selected input device immediately to avoid using stale selection inside the thread.
    let sel = st.read().sel_input;
    let input_dev = match audio::list_devices() {
        Ok((inputs, _)) => {
            inputs
                .into_iter()
                .enumerate()
                .find_map(|(i, d)| if i == sel { Some(d) } else { None })
        }
        Err(e) => {
            eprintln!("list_devices err: {e}");
            None
        }
    };
    let running_flag = srv_state.input_running.clone();
    running_flag.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        if let Some(dev) = input_dev {
            let flag = running_flag.clone();
            let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
            {
                let mut guard = srv_state.input_stop_tx.lock();
                *guard = Some(stop_tx);
            }
            match audio::build_input_stream(&dev, pool, tx, flag.clone()) {
                Ok(handle) => {
                    let params = handle.params.clone();
                    *srv_state.audio_params.lock() = Some(params);
                    srv_state.stage.store(2, Ordering::SeqCst);
                    // 等待停止信号或标志
                    while flag.load(Ordering::Relaxed) {
                        if stop_rx
                            .recv_timeout(std::time::Duration::from_millis(200))
                            .is_ok()
                        {
                            break;
                        }
                    }
                    // 精确停止: pause
                    if let Err(e) = handle.stream.pause() {
                        eprintln!("[SERVER][INPUT] pause err: {e}");
                    }
                    println!("[SERVER][INPUT] stream paused & thread exit");
                }
                Err(e) => {
                    eprintln!("build input stream failed: {e}");
                }
            }
        } else {
            eprintln!("No input device found for selected index {sel}");
        }
    });
    Ok(())
}

/// Shared inline style for panel container.
fn panel_style() -> &'static str {
    "position:relative;border:1px solid var(--color-border);padding:14px 14px 12px 14px;margin:18px 0 10px 0;border-radius:var(--radius-lg);display:flex;flex-direction:column;gap:12px;background:var(--color-panel);"
}

/// Shared inline style for floating panel title.
fn panel_title_style() -> &'static str {
    "position:absolute;top:-10px;left:14px;padding:0 10px;background:var(--color-bg);font-size:12px;line-height:20px;font-weight:600;color:var(--color-text-dim);border:1px solid var(--color-border);border-radius:20px;text-transform:uppercase;letter-spacing:.5px;"
}
