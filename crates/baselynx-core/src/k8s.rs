//! PostgreSQL databases in Kubernetes: port-forward streams and credentials from secrets.
//!
//! The databases come from the config file ([`crate::config`]). baselynx opens its own
//! port-forward stream for every session, so it needs no local port.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Pod, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use serde::{Deserialize, Serialize};
use tokio::io::DuplexStream;
use tokio::sync::Mutex;

use crate::error::{CoreError, Result};
use crate::model::ConnectionConfig;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Where a database lives in Kubernetes and how to get its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubeTarget {
    /// The cluster's name in the config file, e.g. `shop-prod`.
    pub cluster: String,
    /// Absolute path of the kubeconfig.
    pub kubeconfig: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub namespace: String,
    /// The database's name in the config file, e.g. `orders-db`.
    pub forward: String,
    /// A service name, or a pod name when `pod` is set.
    pub service: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pod: bool,
    /// The service port (or the container port of a pod); 0 means the only port.
    #[serde(default)]
    pub remote_port: u16,
    /// The replica service (`ro`), kept for a read-only connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica: Option<String>,
    #[serde(default)]
    pub credentials: KubeCredentials,
}

/// Credentials from a secret in the target namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubeCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Field → secret key: `database`, `username`, `password`, `connection_string`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

impl KubeCredentials {
    fn key<'a>(&'a self, field: &'a str) -> &'a str {
        if let Some(k) = self.fields.get(field) {
            return k;
        }
        match field {
            "connection_string" => "connection-string",
            other => other,
        }
    }
}

impl KubeTarget {
    /// `namespace/service:port`, for the sidebar tooltip and the connection form.
    pub fn describe(&self) -> String {
        let kind = if self.pod { "pod" } else { "svc" };
        let port = if self.remote_port == 0 {
            String::new()
        } else {
            format!(":{}", self.remote_port)
        };
        format!("{} · {}/{kind}/{}{port}", self.cluster, self.namespace, self.service)
    }
}

// ---------------------------------------------------------------- tunnels

/// A kube client per kubeconfig and context, shared by every session.
type ClientMap = HashMap<(String, Option<String>), kube::Client>;

fn clients() -> &'static Mutex<ClientMap> {
    static CLIENTS: OnceLock<Mutex<ClientMap>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pod and container port of a target, resolved once and refreshed after a failure.
fn resolved() -> &'static Mutex<HashMap<String, (String, u16)>> {
    static RESOLVED: OnceLock<Mutex<HashMap<String, (String, u16)>>> = OnceLock::new();
    RESOLVED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn kube_err(context: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Connect(format!("{context}: {e}"))
}

async fn client(t: &KubeTarget) -> Result<kube::Client> {
    let key = (t.kubeconfig.clone(), t.context.clone());
    let mut map = clients().lock().await;
    if let Some(c) = map.get(&key) {
        return Ok(c.clone());
    }
    let kubeconfig = Kubeconfig::read_from(&t.kubeconfig).map_err(|e| kube_err("kubeconfig", e))?;
    let options = KubeConfigOptions {
        context: t.context.clone(),
        ..Default::default()
    };
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|e| kube_err("kubeconfig", e))?;
    let c = kube::Client::try_from(config).map_err(|e| kube_err("kubernetes client", e))?;
    map.insert(key, c.clone());
    Ok(c)
}

fn target_key(t: &KubeTarget) -> String {
    format!(
        "{}|{:?}|{}|{}|{}",
        t.kubeconfig, t.context, t.namespace, t.service, t.remote_port
    )
}

fn pod_ready(pod: &Pod) -> bool {
    let status = pod.status.as_ref();
    status.and_then(|s| s.phase.as_deref()) == Some("Running")
        && status
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// Finds a ready pod and its container port, as kubectl port-forward does: EndpointSlices of the
/// service first, then the service's selector.
async fn resolve(c: &kube::Client, t: &KubeTarget) -> Result<(String, u16)> {
    let ns = t.namespace.as_str();
    if t.pod {
        let pod = Api::<Pod>::namespaced(c.clone(), ns)
            .get(&t.service)
            .await
            .map_err(|e| kube_err(&format!("pod {ns}/{}", t.service), e))?;
        if !pod_ready(&pod) {
            return Err(CoreError::Connect(format!("pod {ns}/{} is not ready", t.service)));
        }
        return Ok((t.service.clone(), t.remote_port));
    }
    let svc = Api::<Service>::namespaced(c.clone(), ns)
        .get(&t.service)
        .await
        .map_err(|e| kube_err(&format!("service {ns}/{}", t.service), e))?;
    let ports = svc.spec.as_ref().and_then(|s| s.ports.clone()).unwrap_or_default();
    let service_port = if t.remote_port == 0 {
        if ports.len() != 1 {
            return Err(CoreError::Connect(format!(
                "service {ns}/{} has {} ports; set remote",
                t.service,
                ports.len()
            )));
        }
        ports.first().cloned()
    } else {
        ports.iter().find(|p| p.port == t.remote_port as i32).cloned()
    };

    let slices = Api::<EndpointSlice>::namespaced(c.clone(), ns)
        .list(&ListParams::default().labels(&format!("kubernetes.io/service-name={}", t.service)))
        .await;
    if let Ok(slices) = slices {
        let mut candidates: Vec<(String, u16)> = Vec::new();
        for slice in slices.items {
            let port = match &service_port {
                Some(sp) => {
                    let name = sp.name.clone().unwrap_or_default();
                    let found = slice
                        .ports
                        .iter()
                        .flatten()
                        .find(|p| p.name.clone().unwrap_or_default() == name)
                        .and_then(|p| p.port);
                    match found {
                        Some(p) => p as u16,
                        None => continue,
                    }
                }
                None => t.remote_port,
            };
            for ep in slice.endpoints.into_iter().flatten() {
                let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true);
                let terminating = ep.conditions.as_ref().and_then(|c| c.terminating).unwrap_or(false);
                if let Some(r) = ep.target_ref.filter(|r| r.kind.as_deref() == Some("Pod"))
                    && ready
                    && !terminating
                    && let Some(name) = r.name
                {
                    candidates.push((name, port));
                }
            }
        }
        candidates.sort();
        return candidates
            .into_iter()
            .next()
            .ok_or_else(|| CoreError::Connect(format!("service {ns}/{} has no ready pods", t.service)));
    }

    // No right to read EndpointSlices: use the selector.
    let selector = svc
        .spec
        .as_ref()
        .and_then(|s| s.selector.clone())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Connect(format!("service {ns}/{} has no selector", t.service)))?;
    let labels = selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let pods = Api::<Pod>::namespaced(c.clone(), ns)
        .list(&ListParams::default().labels(&labels))
        .await
        .map_err(|e| kube_err(&format!("pods of {ns}/{}", t.service), e))?;
    let mut ready: Vec<Pod> = pods.items.into_iter().filter(pod_ready).collect();
    ready.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    let pod = ready
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::Connect(format!("service {ns}/{} has no ready pods", t.service)))?;
    let name = pod.metadata.name.clone().unwrap_or_default();
    let port = match service_port.and_then(|sp| sp.target_port) {
        Some(IntOrString::Int(p)) => p as u16,
        Some(IntOrString::String(port_name)) => pod
            .spec
            .iter()
            .flat_map(|s| s.containers.iter())
            .flat_map(|c| c.ports.iter().flatten())
            .find(|p| p.name.as_deref() == Some(port_name.as_str()))
            .map(|p| p.container_port as u16)
            .ok_or_else(|| CoreError::Connect(format!("pod {name} has no port named {port_name}")))?,
        None => t.remote_port,
    };
    Ok((name, port))
}

/// A new byte stream to the database through the Kubernetes API.
pub async fn open_stream(t: &KubeTarget) -> Result<DuplexStream> {
    let c = client(t).await?;
    let key = target_key(t);
    for attempt in 0..2 {
        let cached = resolved().lock().await.get(&key).cloned();
        let (pod, port) = match cached {
            Some(v) if attempt == 0 => v,
            _ => {
                let v = tokio::time::timeout(TIMEOUT, resolve(&c, t))
                    .await
                    .map_err(|_| CoreError::Connect(format!("timed out finding a pod for {}", t.describe())))??;
                resolved().lock().await.insert(key.clone(), v.clone());
                v
            }
        };
        let pods = Api::<Pod>::namespaced(c.clone(), &t.namespace);
        match tokio::time::timeout(TIMEOUT, pods.portforward(&pod, &[port])).await {
            Ok(Ok(mut pf)) => {
                if let Some(stream) = pf.take_stream(port) {
                    // The forwarder's task keeps running while the stream is open.
                    return Ok(stream);
                }
            }
            Ok(Err(e)) if attempt == 1 => return Err(kube_err(&format!("port-forward to {pod}"), e)),
            Err(_) if attempt == 1 => {
                return Err(CoreError::Connect(format!("timed out opening a port-forward to {pod}")));
            }
            _ => {}
        }
        // The pod may have moved (failover, restart): resolve again once.
        resolved().lock().await.remove(&key);
    }
    Err(CoreError::Connect(format!(
        "could not open a port-forward for {}",
        t.describe()
    )))
}

/// A listener on 127.0.0.1 that opens a port-forward stream for every connection it
/// accepts, for clients that need a host and port (MySQL, pg_dump). It stops when dropped.
pub struct LocalForward {
    pub port: u16,
    task: tokio::task::AbortHandle,
}

impl Drop for LocalForward {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn local_forward(target: KubeTarget) -> Result<LocalForward> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CoreError::Connect(format!("local listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CoreError::Connect(format!("local listener: {e}")))?
        .port();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let target = target.clone();
            tokio::spawn(async move {
                if let Ok(mut stream) = open_stream(&target).await {
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
            });
        }
    })
    .abort_handle();
    Ok(LocalForward { port, task })
}

/// Database, user and password from the target's secret (or what the config gave).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub database: String,
    pub username: String,
    pub password: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "…"))
            .finish()
    }
}

pub async fn credentials(t: &KubeTarget, config: &ConnectionConfig) -> Result<Credentials> {
    let mut out = Credentials {
        database: config.database.clone(),
        username: config.user.clone(),
        password: None,
    };
    let Some(secret_name) = &t.credentials.secret else {
        return Ok(out);
    };
    let c = client(t).await?;
    let secret = tokio::time::timeout(TIMEOUT, Api::<Secret>::namespaced(c, &t.namespace).get(secret_name))
        .await
        .map_err(|_| CoreError::Connect(format!("timed out reading secret {}/{secret_name}", t.namespace)))?
        .map_err(|e| kube_err(&format!("secret {}/{secret_name}", t.namespace), e))?;
    let data = secret.data.unwrap_or_default();
    let field = |name: &str| {
        data.get(t.credentials.key(name))
            .map(|b| String::from_utf8_lossy(&b.0).into_owned())
            .filter(|s| !s.is_empty())
    };
    let mut database = field("database");
    let mut username = field("username");
    let mut password = field("password");
    if (database.is_none() || username.is_none() || password.is_none())
        && let Some(url) = field("connection_string").and_then(|s| crate::model::parse_connection_url(&s))
    {
        database = database.or(url.database);
        username = username.or(url.user);
        password = password.or(url.password);
    }
    if out.database.is_empty() {
        out.database = database.unwrap_or_default();
    }
    if out.username.is_empty() {
        out.username = username.unwrap_or_default();
    }
    out.password = password;
    if out.database.is_empty() || out.username.is_empty() || out.password.is_none() {
        return Err(CoreError::Connect(format!(
            "secret {}/{secret_name} has no database, username or password",
            t.namespace
        )));
    }
    Ok(out)
}
