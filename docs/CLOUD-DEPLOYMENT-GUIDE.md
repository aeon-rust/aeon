# Cloud Deployment Guide — Aeon Multi-Node on DigitalOcean DOKS

> Step-by-step prerequisites, cluster setup, and validation plan for deploying
> a 3-node Aeon Raft cluster on DigitalOcean Kubernetes (DOKS).

---

## 1. Prerequisites by Operating System

### 1.1 Common Tools (All Platforms)

| Tool | Version | Purpose |
|------|---------|---------|
| `kubectl` | >= 1.28 | Kubernetes CLI |
| `helm` | >= 3.14 | Helm chart deployment |
| `doctl` | >= 1.104 | DigitalOcean CLI |
| `docker` | >= 24.0 | Build and push container images |
| `git` | >= 2.40 | Source control |
| `curl` | any | API testing |

### 1.2 Windows

```powershell
# Install via winget (preferred)
winget install Kubernetes.kubectl
winget install Helm.Helm
winget install DigitalOcean.Doctl
winget install Docker.DockerDesktop   # or Rancher Desktop
winget install Git.Git

# Or via Chocolatey
choco install kubernetes-cli helm doctl docker-desktop git -y

# Verify
kubectl version --client
helm version
doctl version
docker --version
```

**Windows-specific notes:**
- WSL2 is required for Docker Desktop / Rancher Desktop
- Rancher Desktop users: switch container engine to **containerd** for K8s
  compatibility (`Preferences → Container Engine → containerd`)
- Docker Desktop users: enable Kubernetes in settings if you want local testing
- `doctl` authenticates via `doctl auth init` — stores token in `%APPDATA%\doctl\config.yaml`
- Use Git Bash or WSL2 terminal for shell scripts in this guide

### 1.3 macOS

```bash
# Install via Homebrew
brew install kubectl helm doctl docker git

# Or install Docker Desktop / Rancher Desktop via cask
brew install --cask docker          # Docker Desktop
# OR
brew install --cask rancher         # Rancher Desktop

# Verify
kubectl version --client
helm version
doctl version
docker --version
```

**macOS-specific notes:**
- Apple Silicon (M1/M2/M3/M4): Aeon Docker image is `linux/amd64` — builds
  run under Rosetta emulation in Docker. Cross-compile with `--platform linux/amd64`
- Docker Desktop or Rancher Desktop required (no native Docker daemon on macOS)
- `doctl` token stored in `~/.config/doctl/config.yaml`

### 1.4 Linux (Ubuntu/Debian)

```bash
# kubectl
curl -LO "https://dl.k8s.io/release/$(curl -L -s https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl"
chmod +x kubectl && sudo mv kubectl /usr/local/bin/

# Helm
curl https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

# doctl
cd /tmp && curl -sL https://github.com/digitalocean/doctl/releases/download/v1.104.0/doctl-1.104.0-linux-amd64.tar.gz | tar xz
sudo mv doctl /usr/local/bin/

# Docker
sudo apt-get update && sudo apt-get install -y docker.io
sudo usermod -aG docker $USER  # then re-login

# Verify
kubectl version --client
helm version
doctl version
docker --version
```

**Linux-specific notes:**
- Docker runs natively — no VM overhead (fastest build + push)
- Ensure your user is in the `docker` group to avoid `sudo docker`
- `doctl` token stored in `~/.config/doctl/config.yaml`
- For Rust cross-compilation (if building locally): `sudo apt install gcc pkg-config libssl-dev cmake`

---

## 2. DigitalOcean Account Setup

### 2.1 API Token

```bash
# Create a personal access token at:
# https://cloud.digitalocean.com/account/api/tokens
# Scope: read + write

# Authenticate doctl
doctl auth init
# Paste your token when prompted

# Verify
doctl account get
```

### 2.2 Container Registry (DOCR)

Aeon images must be accessible to the DOKS cluster. Options:

**Option A: DigitalOcean Container Registry (recommended)**
```bash
# Create registry (one-time, Starter plan = $5/mo, 5 repos, 5GB)
doctl registry create aeon-registry

# Configure Docker to push to DOCR
doctl registry login

# Tag and push
docker tag aeonrust/aeon:latest registry.digitalocean.com/aeon-registry/aeon:latest
docker push registry.digitalocean.com/aeon-registry/aeon:latest

# Integrate with DOKS (allows cluster to pull images)
doctl registry kubernetes-manifest | kubectl apply -f -
```

**Option B: Docker Hub (public)**
```bash
# Images already at docker.io/aeonrust/aeon:latest
# No registry setup needed — DOKS pulls public images directly
```

---

## 3. DOKS Cluster Creation

### 3.1 Cluster Specification

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Region | `nyc1`, `sfo3`, or nearest | Low latency to your location |
| K8s version | 1.30+ | Latest stable, CPU Manager support |
| Node pool name | `aeon-pool` | Descriptive |
| Node size | `c-4` (4 dedicated vCPU, 8GB) | CPU-Optimized for Aeon |
| Node count | 3 | Minimum for Raft quorum |
| Auto-upgrade | Enabled | Security patches |

**Cost estimate:**
- 3x `c-4` nodes: $84/mo each = **$252/mo**
- Alternative: 3x `s-4vcpu-8gb` (shared CPU): $48/mo each = **$144/mo** (adequate for validation)
- DOCR Starter: $5/mo
- Load Balancer (if needed): $12/mo
- **Total (validation)**: ~$161-269/mo

### 3.2 Create the Cluster

```bash
# Create 3-node CPU-Optimized cluster
doctl kubernetes cluster create aeon-cluster \
  --region nyc1 \
  --version 1.30.6-do.0 \
  --node-pool "name=aeon-pool;size=c-4;count=3;auto-scale=false" \
  --wait

# Download kubeconfig
doctl kubernetes cluster kubeconfig save aeon-cluster

# Verify nodes
kubectl get nodes -o wide
```

### 3.3 Enable CPU Manager (Static Policy)

DOKS supports custom kubelet configuration via worker node config. This is
required for Aeon's CPU pinning to work with exclusive cores.

```bash
# Check if CPU Manager is available on your DOKS version
# DOKS 1.28+ supports kubelet config customization

# Apply via DOKS node pool configuration:
doctl kubernetes cluster node-pool update aeon-cluster aeon-pool \
  --kubelet-config '{"cpuManagerPolicy":"static"}'

# Nodes will roll (restart one at a time) to apply the new kubelet config.
# Wait for all nodes to be Ready:
kubectl get nodes -w
```

**Verify CPU Manager is active:**
```bash
# SSH into a node (or use a debug pod) and check:
kubectl debug node/<node-name> -it --image=busybox -- cat /var/lib/kubelet/cpu_manager_state
# Should show "policyName":"static"
```

**Important:** For CPU pinning to take effect, Aeon pods must use **integer CPU
requests that equal limits** (Guaranteed QoS):
```yaml
resources:
  requests:
    cpu: "4"       # Integer, not "4000m"
    memory: "4Gi"
  limits:
    cpu: "4"       # Must equal requests
    memory: "4Gi"
```

---

## 4. Redpanda Deployment

Aeon's primary data path is Kafka-compatible. Deploy Redpanda on the same cluster
or use a managed Kafka service.

### 4.1 Redpanda on DOKS (Co-located)

```bash
# Add Redpanda Helm repo
helm repo add redpanda https://charts.redpanda.com
helm repo update

# Install Redpanda (3-broker, minimal config for validation)
helm install redpanda redpanda/redpanda \
  --namespace redpanda \
  --create-namespace \
  --set statefulset.replicas=3 \
  --set resources.cpu.cores=1 \
  --set resources.memory.container.max=2Gi \
  --set storage.persistentVolume.size=20Gi \
  --set tls.enabled=false \
  --set external.enabled=false \
  --wait --timeout 10m

# Verify
kubectl -n redpanda get pods
# Should show: redpanda-0, redpanda-1, redpanda-2 all Running

# Create test topics
kubectl -n redpanda exec -it redpanda-0 -- rpk topic create aeon-source --partitions 16
kubectl -n redpanda exec -it redpanda-0 -- rpk topic create aeon-sink --partitions 16
```

**Redpanda broker address for Aeon config:**
```
redpanda.redpanda.svc.cluster.local:9093
```

### 4.2 Alternative: Managed Kafka (Aiven, Confluent, MSK)

If using a managed Kafka service, set the broker address in Aeon's Helm values:
```yaml
config:
  brokers: "your-kafka-broker:9092"
```

---

## 5. TLS Certificates

Aeon has two distinct TLS surfaces:

1. **Internal cluster QUIC (port 4470)** — Raft inter-node communication. Uses mutual TLS
   (mTLS) with `rustls`. A self-signed CA is acceptable here because both endpoints are
   controlled Aeon pods. The code enforces `TlsMode::Pem` for multi-node clusters —
   `TlsMode::Auto` (self-signed auto-generated) is explicitly blocked for multi-node.

2. **REST API (port 4471)** — Management API exposed to operators/CI/CD. Aeon's REST API
   serves plain TCP (no built-in TLS). For production, use a Kubernetes Ingress controller
   with CA-issued certificates (e.g., Let's Encrypt via cert-manager) for external access.

For local/dev single-node deployments, `TlsMode::Auto` auto-generates and persists
self-signed certs. For production (single-node or multi-node), always use `TlsMode::Pem`
with proper CA-signed certificates.

**Certificate rotation**: `CertificateStore::reload()` re-reads PEM files from disk without
Aeon restart. Combined with cert-manager auto-renewal, certificates rotate seamlessly.

### 5.1 cert-manager (Recommended)

```bash
# Install cert-manager
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.14.4/cert-manager.yaml

# Wait for cert-manager pods
kubectl -n cert-manager wait --for=condition=Ready pods --all --timeout=120s
```

### 5.2 Self-Signed CA for Internal Cluster Communication

For Raft inter-node QUIC (internal only — not exposed to users):

```yaml
# tls-issuer.yaml
apiVersion: cert-manager.io/v1
kind: Issuer
metadata:
  name: aeon-selfsigned
  namespace: aeon
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: aeon-ca
  namespace: aeon
spec:
  isCA: true
  commonName: aeon-ca
  secretName: aeon-ca-secret
  issuerRef:
    name: aeon-selfsigned
    kind: Issuer
---
apiVersion: cert-manager.io/v1
kind: Issuer
metadata:
  name: aeon-ca-issuer
  namespace: aeon
spec:
  ca:
    secretName: aeon-ca-secret
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: aeon-tls
  namespace: aeon
spec:
  secretName: aeon-tls
  issuerRef:
    name: aeon-ca-issuer
    kind: Issuer
  commonName: aeon
  dnsNames:
    - "aeon-0.aeon-headless.aeon.svc.cluster.local"
    - "aeon-1.aeon-headless.aeon.svc.cluster.local"
    - "aeon-2.aeon-headless.aeon.svc.cluster.local"
    - "*.aeon-headless.aeon.svc.cluster.local"
  duration: 8760h   # 1 year
  renewBefore: 720h # Renew 30 days before expiry
```

```bash
kubectl create namespace aeon
kubectl apply -f tls-issuer.yaml

# Verify certificate is issued
kubectl -n aeon get certificate aeon-tls
# Should show READY=True
```

### 5.3 Ingress TLS for REST API (Production External Access)

Aeon's REST API (port 4471) serves plain TCP. For production external access,
add an Ingress with Let's Encrypt TLS:

```bash
# Install nginx ingress controller (DOKS)
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx
helm install ingress-nginx ingress-nginx/ingress-nginx \
  --namespace ingress-nginx --create-namespace
```

```yaml
# aeon-ingress.yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: your-email@example.com
    privateKeySecretRef:
      name: letsencrypt-prod
    solvers:
      - http01:
          ingress:
            class: nginx
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: aeon-api
  namespace: aeon
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
spec:
  ingressClassName: nginx
  tls:
    - hosts:
        - aeon.yourdomain.com
      secretName: aeon-api-tls
  rules:
    - host: aeon.yourdomain.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: aeon
                port:
                  number: 4471
```

```bash
kubectl apply -f aeon-ingress.yaml

# Point DNS A record for aeon.yourdomain.com to the Ingress LB IP:
kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

**Without a domain**: For validation without a public domain, use `kubectl port-forward`
to access the REST API directly. TLS is not required for port-forwarded traffic.

---

## 6. Deploy Aeon Cluster

### 6.1 Helm Values for Multi-Node

Create `values-doks.yaml`:

```yaml
# values-doks.yaml — 3-node Aeon cluster on DOKS

replicaCount: 1  # Ignored when cluster.enabled=true

image:
  repository: registry.digitalocean.com/aeon-registry/aeon  # or aeonrust/aeon
  tag: latest
  pullPolicy: Always

config:
  apiAddr: "0.0.0.0:4471"
  brokers: "redpanda.redpanda.svc.cluster.local:9093"
  logLevel: info
  artifactDir: /app/artifacts

ports:
  quic: 4470
  api: 4471
  metrics: 4472

service:
  type: ClusterIP

# CPU-pinned resources (Guaranteed QoS for static CPU Manager)
resources:
  requests:
    cpu: "4"
    memory: "4Gi"
  limits:
    cpu: "4"
    memory: "4Gi"

persistence:
  enabled: true
  storageClass: do-block-storage
  accessModes:
    - ReadWriteOnce
  size: 20Gi

# Enable multi-node Raft cluster
cluster:
  enabled: true
  replicas: 3
  partitions: 16
  tls:
    enabled: true
    secretName: aeon-tls

# HPA is disabled for StatefulSet cluster mode
autoscaling:
  enabled: false

# Pod anti-affinity: spread across nodes
affinity:
  podAntiAffinity:
    requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchExpressions:
            - key: app.kubernetes.io/name
              operator: In
              values:
                - aeon
        topologyKey: kubernetes.io/hostname

podSecurityContext:
  runAsNonRoot: true
  runAsUser: 1000
  runAsGroup: 1000
  fsGroup: 1000

securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop:
      - ALL
```

### 6.2 Deploy

```bash
# From the repository root
helm install aeon ./helm/aeon \
  --namespace aeon \
  --create-namespace \
  -f values-doks.yaml \
  --wait --timeout 5m

# Watch pods come up
kubectl -n aeon get pods -w
# Expected:
#   aeon-0   1/1   Running   0   45s
#   aeon-1   1/1   Running   0   30s
#   aeon-2   1/1   Running   0   15s
```

### 6.3 Verify Deployment

```bash
# Check all pods are Running
kubectl -n aeon get pods -o wide

# Check logs for Raft leader election
kubectl -n aeon logs aeon-0 | head -50

# Test REST API
kubectl -n aeon port-forward svc/aeon 4471:4471
# In another terminal:
curl http://localhost:4471/health

# Check headless Service DNS resolution
kubectl -n aeon exec aeon-0 -- nslookup aeon-headless.aeon.svc.cluster.local

# Verify CPU pinning (inside pod)
kubectl -n aeon exec aeon-0 -- cat /proc/self/status | grep Cpus_allowed
```

---

## 7. Validation Plan

### 7.1 Functional Tests (Must Pass)

| # | Test | Command / Method | Expected |
|---|------|-----------------|----------|
| 1 | All pods Running | `kubectl -n aeon get pods` | 3/3 Running |
| 2 | Raft leader elected | Check logs for "became leader" | One leader, two followers |
| 3 | REST API responds | `curl /health` via port-forward | 200 OK |
| 4 | Headless DNS resolves | `nslookup` from pod | All 3 pod FQDNs resolve |
| 5 | Cross-pod QUIC | Check Raft RPC latency in logs | < 5ms intra-cluster |
| 6 | Partition assignment | REST API `/partitions` (future) | 16 partitions across 3 nodes |

### 7.2 Resilience Tests

| # | Test | Method | Expected |
|---|------|--------|----------|
| 7 | Pod restart | `kubectl delete pod aeon-1` | Pod recreated, rejoins cluster |
| 8 | Leader failure | Kill leader pod | New leader elected within timeout |
| 9 | Partition reassignment | Kill node with partitions | Partitions move to surviving nodes |
| 10 | Network partition | NetworkPolicy to isolate pod | Raft handles split, no data loss |

### 7.3 Performance Tests

| # | Test | Method | Expected |
|---|------|--------|----------|
| 11 | Single-partition E2E | aeon-producer → Redpanda → Aeon → Redpanda | > 30K events/sec (batched) |
| 12 | Multi-partition scaling | 16 partitions, measure throughput | Near-linear scaling |
| 13 | CPU pinning verified | `taskset -pc` inside pod | Exclusive core assignment |
| 14 | Sustained load (5 min) | Continuous producer at max rate | Zero event loss, stable latency |
| 15 | Sustained load (1 hr) | Continuous producer at 50% capacity | No memory leaks, stable throughput |

---

## 8. Monitoring

### 8.1 Prometheus + Grafana

```bash
# Install kube-prometheus-stack
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm install monitoring prometheus-community/kube-prometheus-stack \
  --namespace monitoring \
  --create-namespace \
  --set grafana.adminPassword=aeon-admin

# Aeon exposes metrics on port 4472
# Add a ServiceMonitor for Aeon:
```

```yaml
# aeon-service-monitor.yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: aeon
  namespace: aeon
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: aeon
  endpoints:
    - port: metrics
      interval: 15s
```

### 8.2 Key Metrics to Watch

- `aeon_events_processed_total` — total events through the pipeline
- `aeon_pipeline_latency_seconds` — per-event processing latency (P50/P99)
- `aeon_raft_leader_changes_total` — leader election stability
- `aeon_raft_rpc_latency_seconds` — cross-node communication latency
- `aeon_sink_batch_duration_seconds` — sink write performance
- Container CPU/memory usage vs requests/limits

---

## 9. Teardown

```bash
# Remove Aeon
helm uninstall aeon --namespace aeon

# Remove Redpanda
helm uninstall redpanda --namespace redpanda

# Remove monitoring (optional)
helm uninstall monitoring --namespace monitoring

# Delete PVCs (persistent volumes)
kubectl -n aeon delete pvc --all
kubectl -n redpanda delete pvc --all

# Delete the cluster
doctl kubernetes cluster delete aeon-cluster --force

# Delete the container registry (if created)
doctl registry delete aeon-registry --force
```

**Estimated cost if you tear down after validation:** A 3-node `s-4vcpu-8gb`
cluster running for 3 days costs ~$14. CPU-Optimized `c-4` for 3 days: ~$25.

---

## 10. Troubleshooting

### Pod stuck in Pending
```bash
kubectl -n aeon describe pod aeon-0
# Check Events section for scheduling failures
# Common causes: insufficient CPU/memory, PVC binding, node affinity conflicts
```

### Pod in CrashLoopBackOff
```bash
kubectl -n aeon logs aeon-0 --previous
# Check for missing config, TLS cert issues, or Redpanda connectivity
```

### QUIC connection failures between pods
```bash
# Verify UDP port 4470 is open between pods
kubectl -n aeon exec aeon-0 -- nc -u -z aeon-1.aeon-headless.aeon.svc.cluster.local 4470

# Check if CNI supports UDP (Cilium and Calico both do on DOKS)
# Check NetworkPolicy if any exist
kubectl -n aeon get networkpolicy
```

### CPU Manager not assigning exclusive cores
```bash
# Verify Guaranteed QoS
kubectl -n aeon get pod aeon-0 -o jsonpath='{.status.qosClass}'
# Must show: Guaranteed

# Verify integer CPU requests
kubectl -n aeon get pod aeon-0 -o jsonpath='{.spec.containers[0].resources}'
# requests.cpu and limits.cpu must be equal integers (e.g., "4", not "4000m")
```

### Image pull failures
```bash
# If using DOCR, ensure registry integration is set up
doctl registry kubernetes-manifest | kubectl apply -f -

# If using Docker Hub, ensure image is public or imagePullSecrets are configured
kubectl -n aeon get events --sort-by='.lastTimestamp' | grep Pull
```
