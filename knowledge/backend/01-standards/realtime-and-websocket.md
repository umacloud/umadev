---
id: realtime-and-websocket
title: 实时通信与 WebSocket 标准（商业级必读）
domain: backend
category: 01-standards
difficulty: advanced
tags: [实时, realtime, websocket, sse, 长连接, 重连, 心跳, 鉴权, 扩展, presence, 推送, 商业级]
quality_score: 93
last_updated: 2026-06-19
---

# 实时通信与 WebSocket 标准（商业级必读）

> 聊天、协作、通知、实时看板需要实时通信。长连接的鉴权、重连、扩展是常见坑。本标准给出商业级要点。

## 1. 选型

- **单向服务端推**（通知、行情、进度）→ 优先 **SSE**（基于 HTTP，简单、自动重连、走代理友好）。
- **双向低延迟**（聊天、协作、游戏）→ **WebSocket**。
- 别为"偶尔刷新"上 WebSocket；轮询/SSE 更省。
- 用成熟库（Socket.IO / ws / SignalR / Phoenix Channels / centrifugo），别手搓协议。

## 2. 鉴权与安全

- **连接建立时鉴权**（握手带 token，校验后才允许）；不要建立后才补鉴权。
- token 过期处理：长连期间 token 过期要能续期或断开重连。
- **每条消息/订阅做授权**：用户只能订阅/收到自己有权的频道（防越权偷听他人房间）。
- 校验消息来源(origin)；限制消息大小/频率防滥用。

## 3. 连接可靠性

- **心跳/ping-pong** 检活，及时清理死连接。
- **客户端自动重连**（指数退避）；重连后**补偿丢失消息**（用 last-event-id/序号拉增量），不要假设连接永不断。
- 消息有序与去重：网络抖动可能乱序/重复，关键消息带序号/id 去重。
- 背压：客户端慢时限制服务端发送速率/缓冲，防内存爆。

## 4. 扩展（多实例）

- WebSocket 有状态 → 多实例下连接分散在不同节点。用**共享 Pub/Sub（Redis/NATS）**广播，让一个节点的消息能推给连在别节点的用户。
- 或用托管实时服务（Pusher/Ably/centrifugo）省去自建扩展。
- 负载均衡支持长连接（sticky 或 L4）；优雅停机时通知客户端重连到别节点。

## 5. Presence 与状态

- 在线状态/正在输入等用集中存储（Redis）维护，多实例一致。
- 用户多设备/多标签页要正确合并在线状态。

## 6. 反模式（出现即不合格）

- 连接不鉴权或建立后才鉴权；订阅/消息不做授权（可偷听他人）。
- 无心跳清理死连接；客户端不自动重连；断连丢消息无补偿。
- 多实例不用共享 Pub/Sub，消息只能推给同节点用户。
- 无背压，慢客户端拖垮服务端；消息无序无去重。
- 为低频更新滥用 WebSocket。

## 7. 最低交付 checklist

- [ ] 按需选 SSE/WebSocket/轮询；用成熟库不手搓。
- [ ] 握手鉴权 + 每条订阅/消息授权 + origin/大小/频率限制。
- [ ] 心跳检活 + 客户端指数退避重连 + 重连补偿(序号/last-event-id) + 去重。
- [ ] 多实例用共享 Pub/Sub 广播；LB 支持长连接；优雅停机引导重连。
- [ ] 背压控制；Presence 集中维护多设备一致。

---
**参考**：WebSocket/SSE 对比、Socket.IO 扩展(Redis adapter)、心跳与重连、Pub/Sub 广播、Presence 设计。
