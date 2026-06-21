---
id: docker-production-playbook
title: Docker 生产环境实战剧本
domain: devops
category: 02-playbooks
difficulty: intermediate
tags: [docker, containerization, devops, production]
quality_score: 91
maintainer: devops-team@umadev.com
last_updated: 2026-03-29
version: 2.0
related_knowledge:
  - kubernetes-patterns
  - microservices-deployment
  - ci-cd-best-practices
prerequisites:
  - docker-fundamentals
  - linux-basics
---

# Docker 生产环境实战剧本

## 概述

本剧本提供在 Kubernetes 生产环境中部署和维护容器化应用的分步指南。涵盖从镜像构建到生产部署的完整流程,包括安全加固、性能优化、监控告警等关键环节。

## 前置条件

### 必需工具
- Docker 24.0+
- Docker Compose 2.0+ (可选)
- Kubernetes 1.28+ (生产集群)
- Helm 3.0+ (包管理器)
- Container Registry (Docker Hub/GCR/ECR)

### 权限要求
- Docker daemon 访问权限
- Kubernetes namespace admin 权限
- Container registry push 权限

## 场景 1: 遵循最佳实践的 Dockerfile 编写

### 目标
编写安全、高效、可维护的生产级 Dockerfile。

### 步骤

#### 1.1 使用多阶段构建

```dockerfile
# 构建阶段
FROM node:20-alpine AS builder

WORKDIR /app

# 安装依赖
COPY package*.json ./
RUN npm ci --only=production

# 复制源代码
COPY . .

# 构建应用
RUN npm run build

# 生产阶段
FROM node:20-alpine AS production

# 安全: 使用非 root 用户
RUN addgroup -g 1001 -S nodejs && \
    adduser -S nextjs -u 1001

WORKDIR /app

# 只复制构建产物
COPY --from=builder /app/.next/standalone ./
COPY --from=builder /app/.next/static ./.next/static
COPY --from=builder /app/package.json ./

# 设置所有权
RUN chown -R nextjs:nodejs /app

USER nextjs

# 健康检查
HEALTHCHECK --interval=30s --timeout=3s --start-period=40s --retries=3 \
  CMD node healthcheck.js || exit 1

EXPOSE 3000

CMD ["node", "server.js"]
```

**关键点**:
- ✅ 多阶段构建减小镜像体积
- ✅ 使用特定版本标签 (node:20-alpine)
- ✅ 非 root 用户运行
- ✅ 健康检查配置

#### 1.2 优化镜像层

```dockerfile
FROM python:3.11-slim

# 安装系统依赖 (单独一层,便于缓存)
RUN apt-get update && apt-get install -y \
    gcc \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 复制依赖文件 (利用 Docker 缓存)
COPY requirements.txt .

# 安装 Python 依赖
RUN pip install --no-cache-dir -r requirements.txt

# 复制应用代码 (最后复制,变化最频繁)
COPY . .

CMD ["python", "app.py"]
```

**优化策略**:
1. **最少化层数**: 合并相关命令
2. **利用缓存**: 不变的层放在前面
3. **清理缓存**: 删除包管理器缓存

#### 1.3 安全加固

```dockerfile
FROM alpine:3.18

# 安装安全更新
RUN apk update && apk upgrade && \
    apk add --no-cache dumb-init && \
    rm -rf /var/cache/apk/*

# 创建只读文件系统
RUN mkdir -p /app /tmp && \
    chmod 755 /app /tmp

WORKDIR /app
COPY --chown=1000:1000 app .

# 安全标签
LABEL maintainer="devops@company.com"
LABEL version="1.0.0"
LABEL security="high"

# 使用 dumb-init 作为 PID 1
ENTRYPOINT ["dumb-init", "--"]

# 只读根文件系统
RUN chmod -R 555 /app

USER 1000

CMD ["./app"]
```

**安全措施**:
- ✅ 只读文件系统
- ✅ 最小化基础镜像
- ✅ 非 root 用户
- ✅ 使用 init 系统

### 验证

```bash
# 构建镜像
docker build -t myapp:v1.0.0 .

# 检查镜像大小
docker images myapp:v1.0.0

# 扫描安全漏洞
docker scout cves myapp:v1.0.0

# 测试运行
docker run --rm -p 3000:3000 myapp:v1.0.0
```

## 场景 2: 镜像优化和缓存策略

### 目标
优化镜像大小并配置有效的缓存策略。

### 步骤

#### 2.1 使用 .dockerignore

```dockerignore
# 依赖目录
node_modules
npm-debug.log
yarn-error.log

# 构建产物
dist
build
.next
out

# Git
.git
.gitignore

# IDE
.vscode
.idea
*.swp
*.swo

# 测试
coverage
.nyc_output
*.test.js
*.spec.js

# 文档
README.md
CHANGELOG.md
docs/

# 环境文件
.env
.env.*
```

#### 2.2 构建缓存优化

```dockerfile
# 利用 BuildKit 缓存
# syntax=docker/dockerfile:1.4

FROM golang:1.21-alpine AS builder

WORKDIR /app

# 先复制 go.mod 和 go.sum (缓存依赖下载)
COPY go.mod go.sum ./
RUN go mod download

# 再复制源代码
COPY . .

# 构建时利用缓存
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 GOOS=linux go build -a -installsuffix cgo -o main .

FROM alpine:3.18

RUN apk --no-cache add ca-certificates

WORKDIR /root/

COPY --from=builder /app/main .

CMD ["./main"]
```

**BuildKit 特性**:
- `--mount=type=cache`: 持久化缓存
- 并行构建
- 更高效的层缓存

#### 2.3 压缩镜像层

```bash
# 导出并导入镜像 (合并层)
docker save myapp:v1 | docker load

# 使用 docker-squash 压缩层
docker-squash myapp:v1 -t myapp:v1-squashed

# 或使用多阶段构建 (推荐)
# 已在步骤 1.1 中演示
```

### 验证

```bash
# 查看镜像层
docker history myapp:v1.0.0

# 检查镜像大小
docker images myapp:v1.0.0

# 分析镜像层
dive myapp:v1.0.0
```

## 场景 3: 生产级 Docker Compose 配置

### 目标
配置高可用、可扩展的生产级 Docker Compose。

### 步骤

#### 3.1 基础 Compose 文件 (docker-compose.yml)

```yaml
version: '3.8'

services:
  app:
    image: myapp:${VERSION:-latest}
    container_name: myapp
    
    restart: unless-stopped
    
    environment:
      - NODE_ENV=production
      - DATABASE_URL=postgres://db:5432/mydb
    
    secrets:
      - db_password
      - api_key
    
    networks:
      - frontend
      - backend
    
    deploy:
      replicas: 3
      update_config:
        parallelism: 1
        delay: 10s
        failure_action: rollback
      rollback_config:
        parallelism: 0
        order: stop-first
      restart_policy:
        condition: on-failure
        delay: 5s
        max_attempts: 3
        window: 120s
    
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3000/health"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 40s
    
    logging:
      driver: "json-file"
      options:
        max-size: "10m"
        max-file: "3"
        labels: "service,environment"

  db:
    image: postgres:15-alpine
    container_name: myapp-db
    
    volumes:
      - postgres_data:/var/lib/postgresql/data
      - ./init.sql:/docker-entrypoint-initdb.d/init.sql:ro
    
    environment:
      - POSTGRES_DB=mydb
      - POSTGRES_USER=myuser
      - POSTGRES_PASSWORD_FILE=/run/secrets/db_password
    
    secrets:
      - db_password
    
    networks:
      - backend
    
    deploy:
      placement:
        constraints:
          - node.role == manager
    
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U myuser"]
      interval: 10s
      timeout: 5s
      retries: 5

  nginx:
    image: nginx:alpine
    container_name: myapp-nginx
    
    volumes:
      - ./nginx.conf:/etc/nginx/nginx.conf:ro
      - ./ssl:/etc/nginx/ssl:ro
    
    ports:
      - "80:80"
      - "443:443"
    
    networks:
      - frontend
    
    deploy:
      replicas: 2
    
    depends_on:
      - app

networks:
  frontend:
    driver: overlay
    attachable: true
  
  backend:
    driver: overlay
    internal: true

volumes:
  postgres_data:
    driver: local

secrets:
  db_password:
    external: true
  api_key:
    external: true
```

#### 3.2 覆盖配置 (docker-compose.prod.yml)

```yaml
version: '3.8'

services:
  app:
    image: ${REGISTRY}/myapp:${VERSION}
    
    deploy:
      replicas: 5
      resources:
        limits:
          cpus: '2'
          memory: 2G
        reservations:
          cpus: '1'
          memory: 1G
    
    environment:
      - NODE_ENV=production
      - LOG_LEVEL=info
      - METRICS_ENABLED=true
    
    logging:
      driver: "fluentd"
      options:
        fluentd-address: "fluentd:24224"
        tag: "myapp.{{.ID}}"

  db:
    deploy:
      resources:
        limits:
          cpus: '4'
          memory: 8G
        reservations:
          cpus: '2'
          memory: 4G
```

#### 3.3 部署命令

```bash
# 创建 secrets
echo "my_secure_password" | docker secret create db_password -
echo "my_api_key" | docker secret create api_key -

# 初始化 swarm (如果尚未初始化)
docker swarm init

# 部署 stack
docker stack deploy -c docker-compose.yml -c docker-compose.prod.yml myapp

# 查看服务状态
docker stack services myapp

# 查看服务日志
docker service logs -f myapp_app

# 扩缩容
docker service scale myapp_app=10

# 更新服务
docker service update --image ${REGISTRY}/myapp:${NEW_VERSION} myapp_app

# 回滚
docker service rollback myapp_app
```

### 验证
```bash
# 检查服务健康
docker stack ps myapp

# 测试负载均衡
for i in {1..10}; do
  curl -s http://localhost/api/health | jq .container_id
done

# 监控资源使用
docker stats
```

## 场景 4: 镜像安全扫描和签名

### 目标
确保生产环境中只运行安全、可信的镜像。

### 步骤

#### 4.1 配置镜像扫描

```bash
# 使用 Docker Scout 扫描
docker scout cves myapp:v1.0.0

# 使用 Trivy 扫描
trivy image myapp:v1.0.0

# 使用 Grype 扫描
grype myapp:v1.0.0

# CI 集成示例 (GitLab CI)
# .gitlab-ci.yml
scan_image:
  stage: security
  image: aquasec/trivy:latest
  script:
    - trivy image --exit-code 1 --severity HIGH,CRITICAL myapp:$CI_COMMIT_SHA
  only:
    - main
```

#### 4.2 配置内容信任 (DCT)

```bash
# 启用 Docker Content Trust
export DOCKER_CONTENT_TRUST=1

# 生成密钥对
docker trust key generate key.pem

# 添加签名密钥
docker trust signer add --key key.pem admin@company.com

# 签名镜像
docker trust sign myapp:v1.0.0

# 验证签名
docker trust inspect --pretty myapp:v1.0.0
```

#### 4.3 镜像策略 enforcement

```yaml
# Kubernetes OPA 策略: 只允许签名镜像
apiVersion: ingresscontroller.opa.k8s.io/v1
kind: Policy
metadata:
  name: signed-images-only
spec:
  modules:
    signed_images:
      |
      | package signed_images
      |
      | import future.keywords.if
      |
      | deny[msg] {
      |   input := {
      |     "request": {
      |       "kind": "kind",
      |       "image": "image"
      |     }
      |   }
      |   
      |   kind := input.request.kind
      |   image := input.request.image
      |   
      |   if kind == "Pod" {
      |     not image_has_valid_signature(image)
      |   }
      | }
      |
      | image_has_valid_signature(image) {
      |   # 检查镜像签名
      |   true # 实际实现需要调用 DCT API
      | }
```

### 验证

```bash
# 运行扫描
docker scout cves myapp:v1.0.0

# 查看签名信息
docker trust inspect --pretty myapp:v1.0.0

# 测试策略 enforcement
kubectl apply -f pod-unsigned-image.yaml
# 应该被拒绝
```

## 场景 5: 监控和日志收集

### 目标
配置全面的监控和日志收集系统。

### 步骤

#### 5.1 Prometheus 指标收集

```yaml
# docker-compose.monitoring.yml
version: '3.8'

services:
  prometheus:
    image: prom/prometheus:v2.45.0
    container_name: prometheus
    
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - prometheus_data:/prometheus
    
    command:
      - '--config.file=/etc/prometheus/prometheus.yml'
      - '--storage.tsdb.path=/prometheus'
      - '--web.console.libraries=/etc/prometheus/console_libraries'
      - '--web.console.templates=/etc/prometheus/consoles'
    
    ports:
      - "9090:9090"
    
    networks:
      - monitoring

  grafana:
    image: grafana/grafana:10.0.0
    container_name: grafana
    
    environment:
      - GF_SECURITY_ADMIN_USER=admin
      - GF_SECURITY_ADMIN_PASSWORD=admin
      - GF_INSTALL_PLUGINS=grafana-clock-panel
    
    volumes:
      - grafana_data:/var/lib/grafana
      - ./grafana/dashboards:/etc/grafana/provisioning/dashboards
    
    ports:
      - "3001:3000"
    
    networks:
      - monitoring

volumes:
  prometheus_data:
  grafana_data:

networks:
  monitoring:
    driver: bridge
```

```yaml
# prometheus.yml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

scrape_configs:
  - job_name: 'prometheus'
    static_configs:
      - targets: ['localhost:9090']
  
  - job_name: 'myapp'
    docker_sd_configs:
      - host: unix:///var/run/docker.sock
        refresh_interval: 5s
        filters:
          - name: label
            values: ["com.docker.compose.service=myapp"]
    relabel_configs:
      - source_labels: [__meta_docker_container_label_com_docker_compose_service]
        target_label: service
```

#### 5.2 应用指标暴露

```python
# Python 应用添加 Prometheus 指标
from prometheus_client import Counter, Histogram, generate_latest
from fastapi import Response

# 定义指标
request_count = Counter('http_requests_total', 'Total HTTP requests', ['method', 'endpoint'])
request_latency = Histogram('http_request_duration_seconds', 'HTTP request latency', ['method', 'endpoint'])

@app.middleware("http")
async def monitor_requests(request, call_next):
    request_count.labels(method=request.method, endpoint=request.url.path).inc()
    
    with request_latency.labels(method=request.method, endpoint=request.url.path).time():
        response = await call_next(request)
    
    return response

@app.get("/metrics")
async def metrics():
    return Response(content=generate_latest(), media_type="text/plain")
```

#### 5.3 日志收集 (ELK Stack)

```yaml
# docker-compose.logging.yml
version: '3.8'

services:
  elasticsearch:
    image: docker.elastic.co/elasticsearch/elasticsearch:8.10.0
    container_name: elasticsearch
    
    environment:
      - discovery.type=single-node
      - ES_JAVA_OPTS=-Xms512m -Xmx512m
      - xpack.security.enabled=false
    
    volumes:
      - elasticsearch_data:/usr/share/elasticsearch/data
    
    ports:
      - "9200:9200"
    
    networks:
      - logging

  logstash:
    image: docker.elastic.co/logstash/logstash:8.10.0
    container_name: logstash
    
    volumes:
      - ./logstash.conf:/usr/share/logstash/pipeline/logstash.conf:ro
    
    ports:
      - "5044:5044"
    
    networks:
      - logging
    
    depends_on:
      - elasticsearch

  kibana:
    image: docker.elastic.co/kibana/kibana:8.10.0
    container_name: kibana
    
    environment:
      - ELASTICSEARCH_HOSTS=http://elasticsearch:9200
    
    ports:
      - "5601:5601"
    
    networks:
      - logging
    
    depends_on:
      - elasticsearch

volumes:
  elasticsearch_data:

networks:
  logging:
    driver: bridge
```

```
# logstash.conf
input {
  tcp {
    port => 5044
    codec => json_lines
  }
}

filter {
  if [type] == "application" {
    grok {
      match => { "message" => "%{TIMESTAMP_ISO8601:timestamp} %{LOGLEVEL:level} %{GREEDYDATA:log}" }
    }
    
    date {
      match => [ "timestamp", "ISO8601" ]
    }
  }
}

output {
  elasticsearch {
    hosts => ["elasticsearch:9200"]
    index => "myapp-logs-%{+YYYY.MM.dd}"
  }
}
```

### 验证
```bash
# 部署监控 stack
docker stack deploy -c docker-compose.monitoring.yml monitoring

# 检查 Prometheus targets
curl http://localhost:9090/api/v1/targets

# 访问 Grafana
open http://localhost:3001

# 测试日志收集
curl -X POST -H "Content-Type: application/json" \
  -d '{"type":"application","message":"Test log"}' \
  logstash:5044

# 在 Kibana 中查看日志
open http://localhost:5601
```

## 故障排查

### 问题 1: 镜像拉取失败

**症状**:
```
Error: image myapp:v1.0.0 not found
```

**解决方案**:
```bash
# 1. 检查镜像是否存在
docker images | grep myapp

# 2. 检查 registry 认证
docker login registry.example.com

# 3. 检查镜像标签
docker tag myapp:v1.0.0 registry.example.com/myapp:v1.0.0
docker push registry.example.com/myapp:v1.0.0

# 4. 验证 push 成功
docker pull registry.example.com/myapp:v1.0.0
```

### 问题 2: 容器频繁重启

**症状**:
```
docker ps -a
CONTAINER ID   STATUS
abc123         Restarting (1) 5 seconds ago
```

**解决方案**:
```bash
# 1. 查看容器日志
docker logs abc123

# 2. 查看退出代码
docker inspect abc123 | jq .[0].State.ExitCode

# 3. 检查健康检查
docker inspect abc123 | jq .[0].Config.Healthcheck

# 4. 资源限制检查
docker stats --no-stream

# 5. 进入容器调试
docker exec -it abc123 sh
```

### 问题 3: 网络连接失败

**症状**:
```
curl: (7) Failed to connect to db port 5432: Connection refused
```

**解决方案**:
```bash
# 1. 检查网络
docker network ls
docker network inspect myapp_backend

# 2. 检查 DNS 解析
docker exec myapp ping db

# 3. 检查端口
docker exec myapp netstat -tulpn | grep 5432

# 4. 检查防火墙规则
iptables -L -n

# 5. 测试连接
docker run --rm -it --network myapp_backend postgres:15-alpine \
  psql -h db -U myuser -d mydb
```

## 验收清单

- [ ] Dockerfile 遵循最佳实践
- [ ] 镜像大小 < 100MB
- [ ] 无高危漏洞 (docker scout cves)
- [ ] 非 root 用户运行
- [ ] 健康检查配置
- [ ] 日志配置正确
- [ ] 资源限制设置
- [ ] Secrets 管理安全
- [ ] 网络隔离配置
- [ ] 监控指标暴露
- [ ] 备份和恢复测试
- [ ] 更新和回滚测试

## 参考资料

### 官方文档
- [Docker Documentation](https://docs.docker.com/)
- [Dockerfile Best Practices](https://docs.docker.com/develop/develop-images/dockerfile_best-practices/)
- [Docker Security](https://docs.docker.com/engine/security/)

### 工具
- [Dive - Image Layer Analysis](https://github.com/wagoodman/dive)
- [Trivy - Vulnerability Scanner](https://github.com/aquasecurity/trivy)
- [Docker Scout](https://docs.docker.com/scout/)

### 最佳实践
- [Docker Production Checklist](https://github.com/docker/docker.github.io/blob/master/production.md)
- [Container Security Best Practices](https://snyk.io/blog/10-docker-image-security-best-practices/)

---

**知识ID**: `docker-production-playbook`  
**领域**: devops  
**类型**: playbooks  
**难度**: intermediate  
**质量分**: 91  
**维护者**: devops-team@umadev.com  
**最后更新**: 2026-03-29
