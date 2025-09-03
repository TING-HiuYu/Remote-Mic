# Security & Encryption

## 1. Threat Model (当前假设)
- 局域网内部对手可被动监听 / 主动注入。
- 暂无客户端身份鉴别；任意主机可尝试连接。
- 目标: 基础保密性 (可选) + 完整性保护 (AEAD Tag)。

## 2. 预共享密钥 (PSK) 模式
- 启动服务器时输入 PSK -> 派生对称密钥。
- 未输入则明文传输 (GUI 显示 Disabled)。

### 2.1 Key Derivation
```
key = SHA256( psK || salt[8] )  // 取前 32 bytes
```
- salt: 会话随机 8 字节，随握手 (ENC <salt_hex>) 下发。
- 客户端收到后若本地同样提供 PSK -> 派生相同 key。

### 2.2 AEAD 加密
- 算法: XChaCha20-Poly1305 (24-byte nonce)。
- Nonce 结构:
```
[0..8)   = salt
[8..12)  = seq(u32) 大端
[12..20) = ts_ns(u64) (低 8 bytes)
[20..24) = 预留 / 当前未使用
```
- AAD: 重新构建的 22 字节帧头 (payload_len 已写入密文长度)。
- 密文: 原始 payload + 16 字节 Poly1305 tag。

### 2.3 客户端状态
`enc_status` (AtomicI32):
- 0: 明文 / 尚未派生 (服务器加密但客户端未输入 PSK)
- 1: 成功解密至少一帧
- -1: 解密失败 (计入 decrypt_fail 并展示 Key Error)

### 2.4 失败处理
- 解密失败 -> 丢弃该帧。
- 重复失败不额外放大日志 (只计数并首次切换状态)。

## 3. 完整性与重放
- AEAD Tag 提供 payload + header AAD 完整性校验。
- 重放窗口未实现 (nonce 由 seq+ts_ns 组成，重复概率低)。
- 可改进: 记录最近 N 个 (seq, ts_ns) 检测重复。

## 4. 日志策略
- 仅首次显示启用/缺失 PSK 警告。
- 解密失败打印 seq + 错误类型。
- 建议后续使用 `tracing` 进行级别分类与过滤。

## 5. 与代码映射
| 功能 | 文件位置 |
|------|----------|
| key 派生 | `server.rs::enable_psk` / `client.rs::connect` |
| 加密发送 | `server.rs::audio_multicast_loop` (重写 header + AEAD) |
| 解密 | `client.rs` UDP 接收线程 (decrypt + enc_status) |
| 状态徽章 | `dioxus_gui.rs` 读取 `enc_status` |

