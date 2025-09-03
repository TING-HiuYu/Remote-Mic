# Audio Pipeline & Jitter Buffer

## 1. 端到端路径
```
Input Device -> CPAL Input Stream -> Buffer Pool Slot -> (Server) Frame 打包 -> UDP Multicast
-> (Client) 收包解析 -> Reorder Heap -> Adaptive Jitter Buffer -> Mono Downmix -> Output Stream
```

## 2. Buffer Pool
- 结构: 固定容量 Vec<Mutex<Vec<u8>>> + 空闲索引栈。
- 生产者: 输入回调将 f32 样本打包 (前置 4B payload_len) -> 推送 filled_rx。
- 消费者: `audio_multicast_loop` 取索引 -> 构建帧 -> 发送 -> 归还索引。

## 3. 帧格式 (内嵌音频)
- 明文 (或密文) payload 紧随 22 字节头。
- 当前发送 f32 or i16/u16 直通；客户端统一转换成 f32。

## 4. 自适应参数计算
在客户端 UDP 线程：
1. 校准: 第一帧建立 `base_server_ts` 与本地 `Instant` -> offset 初始 0。
2. transit = arrival_rel - server_rel - offset。
3. jitter EWMA: `J = J + (|D| - J)/16` (D = 相邻 transit 差分)。
4. reorder_delay = clamp( max(5ms, jitter*2.5), <=40ms )。
5. target_buffer = f(jitter_ms) in [10ms, 40ms]; max_buffer = 2*target (<=100ms)。
6. 满足: (ts + reorder_delay <= newest && buffered >= target) 或 溢出 > max -> 释放帧。
7. 迟到丢弃: ts + 2*reorder_delay < newest_ts。

## 5. 预缓冲 (Playback Start)
- 输出线程初始阻塞直到累计 ~20ms 样本 (prebuffer)。
- 若不足 -> 输出静音，继续填充。

## 6. Mono Downmix 策略
- 多声道帧: 逐 frame 求和平均 -> mono。
- 后续可改为: 直通 (保留立体声) / 可配置 downmix 矩阵。

## 7. 音量 & 峰值统计
- RMS: 每批解码样本计算平方和平均求根。
- Peak (近似): 存储 RMS 的最大值并每刷新周期衰减 1%。

## 8. Under-run 处理
- 输出回调若样本不足 -> 填 0 并计数 `underruns` (定期日志)。

## 9. 设计权衡
| 目标 | 取舍 |
|------|------|
| 低延迟 | 动态目标缓冲 (10~40ms) 而非固定 100ms |
| 抗抖动 | 允许最大缓冲到 100ms 极端场景 |
| 简洁 | 无重传/FEC，纯被动平滑 |
| 可观测性 | 定期打印统计 (avg_lat, jitter, target, buffer, late_drop) |

## 10. 与代码映射
| 区域 | 文件/函数 |
|------|-----------|
| 预缓冲播放 | `spawn_output_thread` (client.rs) |
| 抖动 EWMA | UDP 接收线程 (client.rs) 内 transit 计算 |
| 重排堆 | `BinaryHeap<Reverse<BufFrame>>` |
| 自适应目标 | `adjust_targets()` 内逻辑 |
| 迟到丢弃 | newest_ts + 2*reorder_delay 判定 |

