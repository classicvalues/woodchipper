// (C) Copyright 2019 Hewlett Packard Enterprise Development LP

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use base64;
use chrono::{DateTime, offset::Utc};
use reqwest::{
  Certificate, Client, ClientBuilder, RequestBuilder, Identity, IntoUrl
};
use serde::Deserialize;
use serde::de::{self, Visitor, Deserializer};
use serde_json::Value;
use snafu::{ensure, Backtrace, ErrorCompat, ResultExt, Snafu};
use subprocess;

#[derive(Debug, Snafu)]
pub enum Error {
  #[snafu(display(
    "unable to read kubeconfig at {}: {}",
    path.display(), source
  ))]
  ConfigRead {
    path: PathBuf,
    source: std::io::Error
  },

  #[snafu(display(
    "unable to deserialize kubeconfig at {}: {}",
    path.display(), source
  ))]
  ConfigDeserialize {
    path: PathBuf,
    source: serde_yaml::Error
  },

  #[snafu(display(
    "context missing in kubeconfig at {}: {:?}",
    path.display(), context
  ))]
  ContextNotFound {
    path: PathBuf,
    context: Option<String>
  },

  #[snafu(display(
    "could not add auth header: {}", message
  ))]
  InvalidAuthHeader {
    message: String
  },

  #[snafu(display(
    "client certificate is invalid: {}", source
  ))]
  InvalidIdentity {
    source: reqwest::Error
  },

  #[snafu(display(
    "unable to initialize reqwest client: {}", source
  ))]
  ReqwestInit {
    source: reqwest::Error
  },

  #[snafu(display(
    "error executing auth plugin {}: {}",
    command, source
  ))]
  AuthPluginExecError {
    command: String,
    source: subprocess::PopenError
  },

  #[snafu(display(
    "error from auth plugin {}: {}",
    command, message
  ))]
  AuthPluginError {
    command: String,
    message: String
  },

  #[snafu(display(
    "error deserializing result from auth plugin {}: {}",
    command, source
  ))]
  AuthPluginDeserialize {
    command: String,
    source: serde_yaml::Error
  },

  #[snafu(display(
    "error converting pem to der"
  ))]
  CertificateConversionError {
    message: String
  },

  #[snafu(display(
    "certificate could not be parsed from {}: {}",
    context, source
  ))]
  InvalidCertificate {
    context: String,
    source: reqwest::Error
  }
}

/// "Alias" for Vec<u8> to avoid puking raw cert data on Debug
#[derive(Clone)]
pub struct Bytes(Vec<u8>);

impl Deref for Bytes {
  type Target = [u8];

  fn deref(&self) -> &[u8] {
    &self.0
  }
}

impl fmt::Debug for Bytes {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "&u8[{}]", self.0.len())
  }
}

type Result<T, E = Error> = std::result::Result<T, E>;

struct BytesFromPathStr;

impl<'de> Visitor<'de> for BytesFromPathStr {
  type Value = Bytes;

  fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.write_str("a string containing a path to an existing file")
  }

  fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
  where
    E: de::Error
  {
    let f = File::open(s);
    match File::open(s) {
      Ok(mut file) => {
        let mut data = Vec::new();

        match file.read_to_end(&mut data) {
          Ok(_) => Ok(Bytes(data)),
          Err(e) => Err(de::Error::custom(format!(
            "error reading file at {}: {:?}", s, e
          )))
        }
      },
      Err(e) => Err(de::Error::custom(format!(
        "unable to open file at {}: {:?}", s, e
      )))
    }
  }
}

fn de_path_bytes<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
  D: Deserializer<'de>
{
  deserializer.deserialize_str(BytesFromPathStr)
}

struct BytesFromBase64;

impl<'de> Visitor<'de> for BytesFromBase64 {
  type Value = Bytes;

  fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.write_str("a string containing base64 data")
  }

  fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
  where
    E: de::Error
  {
    match base64::decode(s) {
      Ok(data) => {
        Ok(Bytes(data))
      },
      Err(e) => Err(de::Error::custom(format!(
        "unable to decode base64 string: {:?}", e
      )))
    }
  }
}

fn de_base64_bytes<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
  D: Deserializer<'de>
{
  deserializer.deserialize_str(BytesFromBase64)
}

struct BytesFromStr;

impl<'de> Visitor<'de> for BytesFromStr {
  type Value = Bytes;

  fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.write_str("a string containing data")
  }

  fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
  where
    E: de::Error
  {
    Ok(Bytes(s.bytes().collect()))
  }
}

fn de_str_bytes<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
  D: Deserializer<'de>
{
  deserializer.deserialize_str(BytesFromStr)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ClusterCertificateAuthority {
  #[serde(rename_all = "kebab-case")]
  File {
    #[serde(rename = "certificate-authority", deserialize_with = "de_path_bytes")]
    certificate: Bytes
  },

  #[serde(rename_all = "kebab-case")]
  Embedded {
    #[serde(
      rename = "certificate-authority-data",
      deserialize_with = "de_base64_bytes"
    )]
    certificate: Bytes
  }
}

fn default_skip_tls_verify() -> bool {
  false
}

#[serde(rename_all = "kebab-case")]
#[derive(Debug, Deserialize)]
pub struct Cluster {
  server: String,

  #[serde(default = "default_skip_tls_verify")]
  insecure_skip_tls_verify: bool,

  #[serde(flatten)]
  certificate_authority: Option<ClusterCertificateAuthority>
}

#[derive(Debug, Deserialize)]
pub struct ClusterContainer {
  pub name: String,
  pub cluster: Cluster
}

#[derive(Debug, Deserialize)]
pub struct Context {
  pub cluster: String,
  pub namespace: Option<String>,
  pub user: String
}

#[derive(Debug)]
pub struct ResolvedContext<'a> {
  pub namespace: &'a str,
  pub auth: &'a Auth,
  pub cluster: &'a Cluster
}

#[derive(Debug, Deserialize)]
pub struct ContextContainer {
  pub name: String,
  pub context: Context
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecAuth {
  #[serde(rename = "apiVersion")]
  pub api_version: String,
  pub command: String,

  #[serde(default)]
  pub args: Vec<String>,

  #[serde(default)]
  pub env: HashMap<String, String>
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ExecCredentialStatus {
  #[serde(rename_all = "camelCase")]
  Token {
    token: String,
    expiration_timestamp: Option<DateTime<Utc>>
  },

  #[serde(rename_all = "camelCase")]
  CertificateEmbedded {
    #[serde(rename = "clientCertificateData", deserialize_with = "de_str_bytes")]
    certificate: Bytes,

    #[serde(rename = "clientKeyData", deserialize_with = "de_str_bytes")]
    key: Bytes,

    expiration_timestamp: Option<DateTime<Utc>>
  }
}

#[derive(Debug, Deserialize)]
pub struct ExecCredential {
  #[serde(rename = "apiVersion")]
  pub api_version: String,

  pub kind: String,
  pub status: ExecCredentialStatus
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Auth {
  Plain {
    username: String,
    password: String,
  },

  Token {
    token: String,
  },

  #[serde(rename_all = "kebab-case")]
  CertificateFile {
    #[serde(rename = "client-certificate", deserialize_with = "de_path_bytes")]
    certificate: Bytes,

    #[serde(rename = "client-key", deserialize_with = "de_path_bytes")]
    key: Bytes
  },

  #[serde(rename_all = "kebab-case")]
  CertificateEmbedded {
    #[serde(
      rename = "client-certificate-data",
      deserialize_with = "de_base64_bytes"
    )]
    certificate: Bytes,

    #[serde(
      rename = "client-key-data",
      deserialize_with = "de_base64_bytes"
    )]
    key: Bytes
  },

  Exec {
    exec: ExecAuth
  },

  Null {}
}

impl Auth {
  /// Attempts to retrieve an ExecCredential if this is an Auth::Exec, otherwise
  /// returns Some(None)
  pub fn exec(&self) -> Result<Option<ExecCredential>> {
    let exec = if let Auth::Exec { exec } = self {
      exec
    } else {
      return Ok(None);
    };

    let env: Vec<(&str, &str)> = exec.env.iter()
      .map(|(k, v)| (k.as_str(), v.as_str()))
      .collect();

    let capture = subprocess::Exec::cmd(&exec.command)
      .args(&exec.args)
      .env_extend(&env)
      .stdout(subprocess::Redirection::Pipe)
      .stderr(subprocess::Redirection::Pipe)
      .capture()
      .context(AuthPluginExecError { command: exec.command.clone() })?;
    
    if capture.success() {
      let creds: ExecCredential = serde_yaml::from_slice(&capture.stdout)
        .context(AuthPluginDeserialize {
          command: exec.command.clone()
        })?;

      Ok(Some(creds))
    } else {
      Err(Error::AuthPluginError {
        command: exec.command.clone(),
        message: capture.stderr_str()
      })
    }
  }

  /// Attempts to create a reqwest client Identity using the configured auth,
  /// if any exists.
  pub fn identity(&self) -> Result<Option<Identity>> {
    let (cert, key) = match self {
      Auth::CertificateFile { certificate, key } => {
        (certificate, key)
      },
      Auth::CertificateEmbedded { certificate, key} => {
        (certificate, key)
      },
      _ => return Ok(None)
    };

    // reqwest wants these cat'd together
    let mut concat = Vec::with_capacity(cert.len() + key.len());
    concat.extend_from_slice(&cert);
    concat.extend_from_slice(&key);

    // rustls doesn't support ip address hosts
    //  - https://github.com/ctz/hyper-rustls/issues/56
    //  - https://github.com/ctz/rustls/issues/184
    //  - https://github.com/briansmith/webpki/issues/54
    //
    // also, native-tls doesn't support PEMs, or at least if it does, reqwest
    // doesn't expose that functionality
    //
    // I think we'll need to keep the kubectl subprocess workaround handy for
    // this case since it affects basically all non-cloud kubernetes apis

    Identity::from_pem(&concat).context(InvalidIdentity {}).map(Some)
  }

  pub fn token(&self) -> Option<&str> {
    match self {
      Auth::Token { token } => {
        Some(&token)
      },
      _ => None
    }
  }
}

impl Default for Auth {
  fn default() -> Self {
    Auth::Null {}
  }
}

impl From<ExecCredential> for Auth {
  fn from(exec: ExecCredential) -> Self {
    match exec.status {
      ExecCredentialStatus::Token { token, .. } => Auth::Token { token },
      ExecCredentialStatus::CertificateEmbedded { certificate, key, .. } => {
        Auth::CertificateEmbedded {
          certificate, key
        }
      }
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct User {
  #[serde(flatten, default)]
  pub auth: Auth
}

impl Default for User {
  fn default() -> Self {
    User {
      auth: Auth::Null {}
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct UserContainer {
  pub name: String,

  #[serde(default)]
  pub user: User
}

#[derive(Debug, Deserialize)]
pub struct KubernetesConfig {
  #[serde(rename = "apiVersion")]
  pub api_version: String,
  pub kind: String,

  pub clusters: Vec<ClusterContainer>,
  pub contexts: Vec<ContextContainer>,
  pub users: Vec<UserContainer>,

  #[serde(rename = "current-context")]
  pub current_context: Option<String>,

  pub preferences: HashMap<String, Value>
}

impl KubernetesConfig {
  pub fn current_context(&self) -> Option<ResolvedContext> {
    let current_context: &str = match &self.current_context {
      Some(c) => c,
      None => return None
    };

    let context = match self.contexts.iter().find(|c| c.name == current_context) {
      Some(c) => &c.context,
      None => return None
    };

    let auth = match self.users.iter().find(|u| u.name == context.user) {
      Some(u) => &u.user.auth,
      None => return None
    };

    let cluster = match self.clusters.iter().find(|c| c.name == context.cluster) {
      Some(c) => &c.cluster,
      None => return None
    };

    Some(ResolvedContext {
      auth, cluster,

      namespace: context.namespace.as_ref().map(String::as_str).unwrap_or("default")
    })
  }

  pub fn load<P>(path: P) -> Result<KubernetesConfig>
  where
    P: AsRef<Path>
  {
    let path = path.as_ref();
    let file = File::open(path).context(ConfigRead { path })?;
    let reader = BufReader::new(file);

    serde_yaml::from_reader(reader).context(ConfigDeserialize { path })
  }
}

pub struct KubernetesClient {
  server: String,
  namespace: String,

  auth: Auth,
  client: Client
}

impl KubernetesClient {
  pub fn new(context: &ResolvedContext) -> Result<KubernetesClient> {
    let mut builder = Client::builder()
      .use_rustls_tls()
      .use_sys_proxy();;

    // do some basic cleanup of the server, the k8s api likes to reject calls
    // with extra slashes
    let server = context.cluster.server.clone()
      .trim_end_matches("/")
      .to_string();

    // TODO: insert context.auth.exec() call here...

    let mut headers = reqwest::header::HeaderMap::new();

    if let Some(token) = context.auth.token() {
      headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
          .map_err(|e| Error::InvalidAuthHeader {
            message: e.to_string()
          })?
      );
    }

    builder = builder.default_headers(headers);

    if let Some(identity) = context.auth.identity()? {
      builder = builder.identity(identity);
    }

    match &context.cluster.certificate_authority {
      Some(ClusterCertificateAuthority::File { certificate }) => {
        let cert = Certificate::from_pem(&certificate)
          .context(InvalidCertificate {
            context: "certificate-authority".to_owned()
          })?;

        builder = builder.add_root_certificate(cert);
      },
      Some(ClusterCertificateAuthority::Embedded { certificate }) => {
        let cert = Certificate::from_pem(&certificate)
          .context(InvalidCertificate {
            context: "certificate-authority-data".to_owned()
          })?;

        builder = builder.add_root_certificate(cert);
      },
      _ => ()
    };

    let client = KubernetesClient {
      server: server,
      namespace: context.namespace.to_owned(),
      auth: context.auth.clone(),
      client: builder.build().context(ReqwestInit {})?
    };

    Ok(client)
  }

  pub fn get<S: Into<String>>(&self, path: S) -> RequestBuilder {
    self.client.get(&format!(
      "{}/{}",
      self.server, path.into().trim_start_matches("/")
    ))
  }

  pub fn post<S: Into<String>>(&self, path: S) -> RequestBuilder {
    self.client.post(&format!(
      "{}/{}",
      self.server, path.into().trim_start_matches("/")
    ))
  }
}