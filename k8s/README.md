# Kubernetes Production Deployment Guide

**NTP Time JSON API** - Production-grade Kubernetes deployment

---

## Quick Start

```bash
# 1. Create namespace
kubectl create namespace ntp-time-api

# 2. Apply all manifests
kubectl apply -f k8s/ -n ntp-time-api

# 3. Verify deployment
kubectl get pods -n ntp-time-api

# 4. Test service
kubectl port-forward svc/ntp-time-api 8080:8080 -n ntp-time-api
curl http://localhost:8080/time
```

---

## Manifest Files

### 1. ConfigMap (`configmap.yaml`)

Stores NTP server configuration:

```yaml
data:
  ntp_servers: "time.google.com:123,time.cloudflare.com:123,time.windows.com:123,pool.ntp.org:123"
```

**Customization**:
```bash
# Edit NTP servers
kubectl edit configmap ntp-time-api-config -n ntp-time-api

# Restart pods to pick up changes
kubectl rollout restart deployment/ntp-time-api -n ntp-time-api
```

---

### 2. Deployment (`deployment.yaml`)

Main application deployment with 3 replicas.

**Key Features**:
- ✅ 3 replicas for high availability
- ✅ Non-root security context (UID 1000)
- ✅ Read-only root filesystem
- ✅ All capabilities dropped
- ✅ Liveness, readiness, and startup probes
- ✅ Resource limits (CPU: 200m, Memory: 128Mi)
- ✅ Prometheus annotations

**Scaling**:
```bash
# Manual scaling
kubectl scale deployment ntp-time-api --replicas=5 -n ntp-time-api

# Auto-scaling (HPA)
kubectl autoscale deployment ntp-time-api \
  --cpu-percent=70 \
  --min=3 \
  --max=10 \
  -n ntp-time-api
```

---

### 3. Service (`service.yaml`)

ClusterIP service exposing port 8080.

**Access Methods**:

**Internal (ClusterIP)**:
```bash
# From within cluster
curl http://ntp-time-api.ntp-time-api.svc.cluster.local:8080/time
```

**External (LoadBalancer)** - Edit service type:
```yaml
spec:
  type: LoadBalancer
```

**External (Ingress)** - Create ingress:
```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: ntp-time-api
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
spec:
  tls:
  - hosts:
    - time.example.com
    secretName: ntp-time-api-tls
  rules:
  - host: time.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: ntp-time-api
            port:
              number: 8080
```

---

### 4. ServiceMonitor (`servicemonitor.yaml`)

Prometheus Operator integration.

**Prerequisites**:
- Prometheus Operator installed
- ServiceMonitor CRD available

**Verify scraping**:
```bash
# Check ServiceMonitor
kubectl get servicemonitor ntp-time-api -n ntp-time-api

# Query Prometheus
curl 'http://prometheus:9090/api/v1/query?query=http_requests_total{app="ntp-time-api"}'
```

---

## Production Configuration

### Resource Sizing

**Default** (suitable for moderate load):
```yaml
resources:
  requests:
    cpu: 50m
    memory: 64Mi
  limits:
    cpu: 200m
    memory: 128Mi
```

**High Traffic** (>10k RPS):
```yaml
resources:
  requests:
    cpu: 100m
    memory: 128Mi
  limits:
    cpu: 500m
    memory: 256Mi
```

### Environment Variables

**Production Tuning**:
```yaml
env:
  # Logging
  - name: LOG_LEVEL
    value: "warn"  # Reduce verbosity in production
  - name: LOG_FORMAT
    value: "json"  # Structured logging

  # NTP Configuration
  - name: SYNC_INTERVAL
    value: "30"  # Sync every 30s
  - name: MAX_STALENESS
    value: "120"  # Alert if time >2min stale
  - name: MAX_CONSECUTIVE_FAILURES
    value: "10"  # Disable server after 10 failures

  # HTTP Configuration
  - name: REQUEST_TIMEOUT
    value: "5"  # 5 second timeout
  - name: BODY_LIMIT_BYTES
    value: "1024"  # 1KB limit
```

---

## High Availability Setup

### 1. Pod Disruption Budget

Ensure minimum availability during voluntary disruptions:

```yaml
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: ntp-time-api-pdb
spec:
  minAvailable: 2  # At least 2 pods always running
  selector:
    matchLabels:
      app: ntp-time-api
```

Apply:
```bash
kubectl apply -f - <<EOF
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: ntp-time-api-pdb
  namespace: ntp-time-api
spec:
  minAvailable: 2
  selector:
    matchLabels:
      app: ntp-time-api
EOF
```

### 2. Pod Anti-Affinity

Spread pods across nodes:

```yaml
spec:
  template:
    spec:
      affinity:
        podAntiAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
          - weight: 100
            podAffinityTerm:
              labelSelector:
                matchExpressions:
                - key: app
                  operator: In
                  values:
                  - ntp-time-api
              topologyKey: kubernetes.io/hostname
```

### 3. Horizontal Pod Autoscaler

Auto-scale based on CPU usage:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: ntp-time-api-hpa
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: ntp-time-api
  minReplicas: 3
  maxReplicas: 10
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 70
```

---

## Security Hardening

### 1. Network Policy

Restrict ingress/egress traffic:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: ntp-time-api-netpol
spec:
  podSelector:
    matchLabels:
      app: ntp-time-api
  policyTypes:
  - Ingress
  - Egress
  ingress:
  - from:
    - namespaceSelector: {}  # Allow from any namespace
    ports:
    - protocol: TCP
      port: 8080
  egress:
  - to:
    - namespaceSelector: {}  # Allow to any namespace (for DNS)
  - ports:
    - protocol: UDP
      port: 123  # NTP servers
    - protocol: UDP
      port: 53   # DNS
```

### 2. Pod Security Standards

Label namespace for restricted pod security:

```bash
kubectl label namespace ntp-time-api \
  pod-security.kubernetes.io/enforce=restricted \
  pod-security.kubernetes.io/audit=restricted \
  pod-security.kubernetes.io/warn=restricted
```

---

## Monitoring & Alerting

### Grafana Dashboard

Example queries for dashboards:

**Request Rate**:
```promql
rate(http_requests_total{app="ntp-time-api"}[5m])
```

**Error Rate**:
```promql
rate(http_requests_total{app="ntp-time-api",status=~"5.."}[5m])
```

**Latency (P99)**:
```promql
histogram_quantile(0.99, rate(http_request_duration_seconds_bucket{app="ntp-time-api"}[5m]))
```

**NTP Sync Status**:
```promql
ntp_server_up{app="ntp-time-api"}
```

### Alert Rules

Example Prometheus alert rules:

```yaml
groups:
- name: ntp-time-api
  rules:
  - alert: HighErrorRate
    expr: rate(http_requests_total{app="ntp-time-api",status=~"5.."}[5m]) > 0.05
    for: 5m
    annotations:
      summary: "High error rate detected"

  - alert: NTPSyncFailing
    expr: ntp_consecutive_failures{app="ntp-time-api"} > 10
    for: 5m
    annotations:
      summary: "NTP sync failing"

  - alert: PodDown
    expr: kube_deployment_status_replicas_available{deployment="ntp-time-api"} < 2
    for: 1m
    annotations:
      summary: "Less than 2 pods available"
```

---

## Troubleshooting

### Pods Not Starting

**Check events**:
```bash
kubectl describe pod -l app=ntp-time-api -n ntp-time-api
```

**Check logs**:
```bash
kubectl logs -l app=ntp-time-api -n ntp-time-api --tail=100
```

**Common issues**:
- Image pull errors → Check registry credentials
- CrashLoopBackOff → Check logs for errors
- OOMKilled → Increase memory limits

### Service Not Responding

**Check service endpoints**:
```bash
kubectl get endpoints ntp-time-api -n ntp-time-api
```

**Port-forward test**:
```bash
kubectl port-forward svc/ntp-time-api 8080:8080 -n ntp-time-api
curl http://localhost:8080/time
```

### NTP Sync Failing

**Check logs**:
```bash
kubectl logs -l app=ntp-time-api -n ntp-time-api | grep -i ntp
```

**Check network**:
```bash
# Test NTP connectivity from pod
kubectl exec -it deployment/ntp-time-api -n ntp-time-api -- /bin/sh
# (Won't work with distroless - use debug container)
```

**Common issues**:
- Firewall blocking UDP/123 → Allow NTP traffic
- Invalid NTP servers → Update ConfigMap
- Network policy blocking → Check egress rules

---

## Upgrades & Rollouts

### Rolling Update

```bash
# Update image
kubectl set image deployment/ntp-time-api \
  ntp-time-api=ntp-time-api:v1.1.0 \
  -n ntp-time-api

# Watch rollout
kubectl rollout status deployment/ntp-time-api -n ntp-time-api
```

### Rollback

```bash
# Rollback to previous version
kubectl rollout undo deployment/ntp-time-api -n ntp-time-api

# Rollback to specific revision
kubectl rollout undo deployment/ntp-time-api --to-revision=2 -n ntp-time-api
```

### Blue-Green Deployment

```bash
# Deploy new version with different label
kubectl apply -f deployment-v2.yaml

# Test new version
kubectl port-forward deployment/ntp-time-api-v2 8080:8080

# Switch service to new version
kubectl patch service ntp-time-api -p '{"spec":{"selector":{"version":"v2"}}}'

# Delete old deployment after verification
kubectl delete deployment ntp-time-api-v1
```

---

## Best Practices

### 1. Always Use Version Tags

Do:
```yaml
image: ntp-time-api:v1.0.0
```

Avoid:
```yaml
image: ntp-time-api:latest
```

### 2. Set Resource Limits
```yaml
resources:
  limits:
    cpu: 200m      # ✅ Prevents CPU starvation
    memory: 128Mi  # ✅ Prevents memory leaks
```

### 3. Use Health Probes
```yaml
livenessProbe: # ✅ Restart unhealthy pods
readinessProbe: # ✅ Don't send traffic to unready pods
startupProbe: # ✅ Allow slow startup
```

### 4. Use ConfigMaps for Configuration
```yaml
env:
- name: NTP_SERVERS
  valueFrom:
    configMapKeyRef:  # ✅ Centralized config
      name: ntp-time-api-config
      key: ntp_servers
```

---

## Additional Resources

- **Security**: See `../SECURITY.md`
- **Deployment Checklist**: See `../DEPLOYMENT_CHECKLIST.md`
- **Examples**: See `../examples/`

---

*Last Updated: December 29, 2025*
