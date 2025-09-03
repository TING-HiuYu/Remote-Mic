# Protocol Specification

本文件详细说明 RemoteMic 当前控制与音频帧协议（版本 draft-0）。

## 1. 控制信道 (TCP)
### 1.1 握手响应
```
OK <session_key> <sample_rate> <channels> <fmt_code> <mcast_ip> <mcast_port> [ENC <salt_hex>|NOENC]\n
```
- session_key: 16 字符随机字母数字 (用于心跳验证)。
- sample_rate / channels / fmt_code: 复制服务器当前音频参数。
- mcast_ip / mcast_port: IPv4 组播地址与端口 (服务器固定在 239.0.0.0/8 随机)。
- ENC <salt_hex>: 若启用 PSK 加密，给出 8 字节 salt 的 hex；客户端派生 key。
- NOENC: 未启用加密。

### 1.2 心跳
客户端每 1 秒:
```
HEART <session_key>\n
```
服务器匹配成功回复:
```
OK\n
```
> 5s 未收到 OK -> 客户端超时断开；服务器亦定期移除 5s 未心跳客户端。

### 1.3 断开
- 主动: 客户端发送 `DISCONNECT\n`，服务器回 `BYE` 或直接关闭。
- 服务器停止: 发送 `SERVER_STOP` 或 TCP 关闭，客户端释放资源。

## 2. 音频帧 (UDP Multicast)
所有客户端加入统一组播组；服务器每帧只发送一次。

### 2.1 帧头格式 (22 bytes)
```
magic(2) | seq(u32) | fmt(u8) | ch(u8) | rate(u32) | payload_len(u16) | ts_ns(u64)
```
字段:
- magic: 常量 `FRAME_MAGIC` 用于快速过滤。
- seq: 32 位递增（服务器 wrap；客户端扩展为 u64 统计）。
- fmt: 采样格式代码 (见 `types.rs`).
- ch: 声道数 (u8)。
- rate: 采样率 (u32)。
- payload_len: 后续有效载荷字节数 (若加密则为密文长)。
- ts_ns: 服务器单调时钟起点以来纳秒，用于客户端对齐与延迟估算。

### 2.2 加密时处理
- 仅加密 payload；header 作为 AEAD AAD。
- 重新构建 header 使 `payload_len` = 明文长度 + 16 (tag)。
- Nonce 组成 (XChaCha20 24 bytes):
  - salt[0..8] | seq(u32) | ts_ns(u64) | 保留(4) (当前实现将 ts_ns 高 8 字节放入 8 bytes 区段)
- AAD = 完整 22 字节（含更新后的 payload_len）。

### 2.3 可靠性与乱序
- 不做重传；客户端使用最小堆按 `ts_ns` 重排。
- 超过 `2 * reorder_delay` 仍落后最新 ts 则丢弃。
- 丢包率: lost / (received + lost) 基于 seq gap。

## 3. 自适应抖动缓冲概述
见 `audio_pipeline.md` (transit 差分 EWMA -> jitter -> 目标缓冲 / 重排窗口)。

## 4. 与实现映射
| 组件 | 代码位置 | 说明 |
|------|----------|------|
| 握手解析 | `client.rs::connect` | 解析 OK 行 tokens |
| 心跳 | `heartbeat_loop` | 1s 发送 / 5s 超时 |
| 帧打包 | `server.rs::audio_multicast_loop` | 构造 + 可选加密 |
| 解密 | UDP 接收线程 | 失败计数 + enc_status 更新 |
| 抖动逻辑 | UDP 接收线程 | 动态缓冲与重排 |