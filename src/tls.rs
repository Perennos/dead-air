//! Certificate and hostname verification for every HTTPS and SMTP connection.
//!
//! Trust comes from the operating system's store (or `SSL_CERT_FILE` / `SSL_CERT_DIR` when
//! set), falling back to the bundled Mozilla roots when the system store is empty. There is
//! no setting that disables verification.

use std::sync::{Arc, OnceLock};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};

fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

/// The shared verifying client configuration.
pub fn client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(root_store())
            .with_no_client_auth();
        Arc::new(config)
    }))
}

/// The name a certificate must match: a DNS name or an IP address.
pub fn server_name(host: &str) -> Option<ServerName<'static>> {
    ServerName::try_from(host.to_string()).ok()
}
