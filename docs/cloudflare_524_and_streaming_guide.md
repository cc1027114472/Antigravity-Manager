# AI 流式代理反向代理规范与 Cloudflare 524 故障排查运维手册

## 一、故障现象与根因剖析

### 1. 现象描述
客户端（如 Cherry Studio、Chatbox、NextChat、Cursor 等）在发起长上下文或带有前置思考的大模型请求时，客户端界面中断并提示：
```html
<title>ukapi.cc | 524: A timeout occurred</title>
Error code 524: A timeout occurred (Cloudflare)
The origin web server timed out responding to this request.
```
同时源站 Nginx 日志记录对应请求的状态码为 `499`（Client Closed Request，即 Cloudflare 主动断开上游连接）。

---

### 2. 核心根因：Nginx 代理缓冲与 Cloudflare 100s 超时冲突

完整的链路时序如下图所示：
```
[客户端] ───> [Cloudflare CDN (100s 倒计时)] ───> [源站 Nginx] ───> [Antigravity 8045] ───> [上游 Google]
                         │                              │
                         │                   (默认开启 proxy_buffering)
                         │                   流式 Token 被积攒在 4KB 缓冲中
                         │                   迟迟不向外推送首字节数据
                         ▼
        (100s 未收到任何响应首字节)
            触发 524 强行熔断
```

* **Cloudflare 限制**：Cloudflare 免费版和专业版对 HTTP/HTTPS 请求的源站响应超时具有不可修改的 **100 秒硬限制**。如果 100 秒内源站没有吐出哪怕 1 个字节，Cloudflare 就会判定源站宕机并返回 524 HTML 页面。
* **Nginx 默认缓冲陷阱**：Nginx 默认配置下 `proxy_buffering on` 是开启的。针对普通 Web 网页，缓冲可以提升传输吞吐量；但对于 **AI Server-Sent Events (SSE)** 流式输出，这会导致生成的字符被死死卡在 Nginx 内存缓冲区（通常 4KB~8KB 才会 Flush 一次）。在思考模型耗时较长、网络轻微延迟或首包生成较慢时，外部 CDN 根本看不到数据流入，直接触发 524 熔断。

---

## 二、方案一实施标准：Nginx 流式直通配置规范

在所有对外提供反向代理服务的节点中（如 `/etc/nginx/sites-available/antigravity-manager.conf`），必须应用如下标准配置：

```nginx
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;
    client_max_body_size 100m;

    location / {
        proxy_pass http://127.0.0.1:8045;
        proxy_http_version 1.1;

        # 【核心防 524 规则】关闭代理缓冲，启用 SSE 流式直通传输
        proxy_buffering off;
        proxy_cache off;
        chunked_transfer_encoding on;
        tcp_nopush off;
        tcp_nodelay on;

        # 保持长连接与 WebSocket 协议升级支持
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        # 超大上下文与思考模型连接超时放宽至 600 秒
        proxy_connect_timeout 60s;
        proxy_read_timeout 600s;
        proxy_send_timeout 600s;
    }
}
```

### 已同步应用的生产节点
* **Node-186** (`186.241.81.6`)：已生效，`nginx -t` 验证通过并已 reload。
* **Node-64** (`38.55.146.64`)：已生效，`nginx -t` 验证通过并已 reload。

---

## 三、运维重要提醒与最佳实践

### 提醒 1：新节点扩容必须应用此模板
未来增加新的反代集群节点或重装系统时，切勿使用系统默认的 Nginx 反代配置，务必将上述包含 `proxy_buffering off;` 的完整配置写入 `/etc/nginx/sites-available/` 并建立软链接。

### 提醒 2：Cloudflare CDN 解析策略建议
如果客户端经常发送数万 Token 的超大提示词、复杂的视觉多模态分析或长时间的 Thinking 推理：
* **首选建议**：登录 Cloudflare 后台，将 API 域名（如 `ukapi.cc`）的解析记录从 **已代理（小黄云）** 切换为 **仅限 DNS（灰色云朵）**。
* **优势**：请求直接打到源站 Nginx，彻底解开 Cloudflare 100 秒的超时封印，享受源站 600 秒的宽松时长。

### 提醒 3：出口动态代理（SOCKS5）健康状态巡检
部分情况下，如果底层分配到的 SOCKS5 代理节点严重丢包（例如日志中频发的 `operation timed out`），会导致代理与 Google 机房连接僵死。请定期关注 Web 后台【动态代理池】，及时剔除变红的不可用出口 IP。
