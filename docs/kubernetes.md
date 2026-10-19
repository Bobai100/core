# Running TxWatch on Kubernetes

The Helm chart in [`deploy/helm/txwatch`](../deploy/helm/txwatch) deploys TxWatch with:

| Concern | How the chart handles it |
|---|---|
| Config | `values.config` is rendered into a ConfigMap and mounted read-only at `/config/txwatch.toml` (`TXWATCH_CONFIG`). Changing it rolls the pod (config checksum annotation). |
| Secrets | `secret.values` (or `secret.existingSecret`) is exposed to the container via `envFrom`; reference keys as `${VAR}` in the config, e.g. `webhook_secret = "${WEBHOOK_SECRET}"`. |
| Cursors | A PersistentVolumeClaim is mounted at `/data`; set `cursor_file = "/data/cursors.json"` so restarts resume where they left off. |
| Readiness | With `metrics.enabled`, `/healthz` is the liveness probe and `/readyz` (200 once a poll has recently succeeded) is the readiness probe. |
| Single replica | Cursors are per process, so a second replica would send every alert twice. The Deployment is fixed at `replicas: 1` with the `Recreate` strategy, so the old pod is gone before the new one starts. |
| Shutdown | TxWatch currently shuts down gracefully on SIGINT only. A `preStop` hook sends SIGINT to PID 1 before the kubelet's SIGTERM, so in-flight deliveries finish and cursors are saved within `terminationGracePeriodSeconds`. |

## Install

The probes, ServiceMonitor and alert rules need an image built with the `metrics` feature
(`cargo build --release -p txwatch --features metrics`); the default image does not serve
`/metrics`. Point `image.repository`/`image.tag` at such an image, or set `metrics.enabled=false`.

```bash
helm install txwatch deploy/helm/txwatch \
  --namespace txwatch --create-namespace \
  --set-file config=./txwatch.toml \
  --set secret.values.WEBHOOK_SECRET=change-me
```

Use `secret.create=false` with `secret.existingSecret=<name>` to manage the Secret yourself
(e.g. with External Secrets or Sealed Secrets).

## Prometheus Operator

```bash
helm upgrade txwatch deploy/helm/txwatch --reuse-values \
  --set metrics.serviceMonitor.enabled=true \
  --set metrics.prometheusRule.enabled=true \
  --set metrics.serviceMonitor.labels.release=kube-prometheus-stack
```

The ServiceMonitor scrapes `/metrics` with `job="txwatch"`, which the bundled alert rules
([`deploy/prometheus/txwatch-alerts.yml`](../deploy/prometheus/txwatch-alerts.yml)) expect. Import
[`deploy/grafana/txwatch.json`](../deploy/grafana/txwatch.json) into Grafana for a dashboard.

## Values

See [`values.yaml`](../deploy/helm/txwatch/values.yaml) for every option: `image`, `config`,
`secret`, `env`, `persistence`, `metrics`, `resources`, security contexts and scheduling.
