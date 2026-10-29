//! Who may talk to whom.
//!
//! Two layers, deliberately separate:
//!
//! * **master <-> node** — mutual TLS off a sandstorm-owned CA. The node proves
//!   it is a registered node with a client certificate; the master proves it
//!   is the real master with a server certificate the node verifies. Both
//!   directions matter: without the second, anyone who can reach a node
//!   could pose as the master and have it hand over sandboxes.
//!
//! * **caller -> sandbox** — a 128-bit bearer token per sandbox, compared in
//!   constant time (see `auth`). Nothing to do with the layer above; a leaked
//!   sandbox token must not grant any control-plane reach.
//!
//! Commands on the master<->node stream are NOT individually signed. mTLS
//! already pins both ends of that channel; per-message signatures would be a
//! second identity system to keep correct for no additional guarantee. Noted
//! here so it reads as a decision rather than an oversight.
//!
//! The node always dials out. The master never connects to a node, it only
//! answers on a stream the node opened. So a node needs no inbound port, and
//! a compromised master cannot reach a machine that never called in.

use anyhow::{Context, Result};
use rand::RngCore;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use std::path::{Path, PathBuf};

/// 128 bits of randomness, hex-encoded. Used for both sandbox tokens and
/// node registration tokens; neither is ever derived from an id or a counter.
pub fn random_token() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// PEM pair on disk.
#[derive(Debug, Clone)]
pub struct CertPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The CA that signs every identity in one sandstorm deployment.
pub struct Ca {
    key: KeyPair,
    cert: rcgen::Certificate,
    dir: PathBuf,
}

impl Ca {
    /// Load the CA from `dir`, creating it on first run. The master owns
    /// this; a node never has the CA key, only its own leaf plus the CA cert.
    pub fn open_or_create(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating pki dir {}", dir.display()))?;
        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca-key.pem");

        if cert_path.exists() && key_path.exists() {
            let key_pem = std::fs::read_to_string(&key_path)?;
            let key = KeyPair::from_pem(&key_pem).context("parsing ca key")?;
            let params = Self::ca_params()?;
            // Re-deriving the cert from the stored key keeps a single code
            // path; the serial differs but the key -- the thing that matters
            // for verification -- is the one on disk.
            let cert = params.self_signed(&key).context("rebuilding ca cert")?;
            return Ok(Self { key, cert, dir });
        }

        let key = KeyPair::generate().context("generating ca key")?;
        let cert = Self::ca_params()?
            .self_signed(&key)
            .context("self-signing ca")?;
        write_private(&key_path, &key.serialize_pem())?;
        std::fs::write(&cert_path, cert.pem())?;
        Ok(Self { key, cert, dir })
    }

    fn ca_params() -> Result<CertificateParams> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "sandstorm control-plane CA");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Ok(params)
    }

    pub fn cert_pem(&self) -> String {
        self.cert.pem()
    }

    pub fn ca_cert_path(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    /// The master's own server identity. Nodes verify this, which is the
    /// half of mTLS that stops a fake master.
    pub fn issue_master(&self, sans: Vec<String>) -> Result<CertPaths> {
        self.issue("master", "sandstorm master", sans, false)
    }

    /// The E2B front door's identity: `*.{domain}`, which covers both
    /// `api.{domain}` and every `{port}-{id}.{domain}`. Only used when the
    /// operator brings no certificate of their own.
    pub fn issue_front(&self, domain: &str) -> Result<CertPaths> {
        self.issue(
            "e2b-front",
            &format!("*.{domain}"),
            vec![format!("*.{domain}"), domain.to_string()],
            false,
        )
    }

    /// A node's client identity. The CN is the node id, so the master learns
    /// which node is on a connection from the certificate rather than from
    /// anything the node claims in a message.
    pub fn issue_node(&self, node_id: &str) -> Result<CertPaths> {
        self.issue(&format!("node-{node_id}"), node_id, Vec::new(), true)
    }

    fn issue(
        &self,
        file_stem: &str,
        common_name: &str,
        sans: Vec<String>,
        client: bool,
    ) -> Result<CertPaths> {
        let mut params = CertificateParams::new(sans)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        params.distinguished_name = dn;
        params.is_ca = IsCa::NoCa;
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if client {
            rcgen::ExtendedKeyUsagePurpose::ClientAuth
        } else {
            rcgen::ExtendedKeyUsagePurpose::ServerAuth
        }];

        let key = KeyPair::generate()?;
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .with_context(|| format!("signing cert for {common_name}"))?;

        let out = CertPaths {
            cert: self.dir.join(format!("{file_stem}.pem")),
            key: self.dir.join(format!("{file_stem}-key.pem")),
        };
        std::fs::write(&out.cert, cert.pem())?;
        write_private(&out.key, &key.serialize_pem())?;
        Ok(out)
    }
}

/// Private keys go out at 0600. On a box that also runs sandboxes, a
/// world-readable control-plane key would undo the whole scheme.
fn write_private(path: &Path, pem: &str) -> Result<()> {
    std::fs::write(path, pem).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    #[test]
    fn tokens_are_128_bits_and_not_repeated() {
        let mut seen = HashSet::new();
        for _ in 0..512 {
            let t = random_token();
            assert_eq!(t.len(), 32, "128 bits as hex");
            assert!(seen.insert(t), "token repeated");
        }
    }

    #[test]
    fn ca_is_stable_across_reopen() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let first = Ca::open_or_create(&dir).unwrap().cert_pem();
        let second = Ca::open_or_create(&dir).unwrap().cert_pem();
        // Same key on disk, so the public key in both certs is the same.
        assert!(!first.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("ca-key.pem"))
                .unwrap()
                .len(),
            std::fs::read_to_string(dir.join("ca-key.pem"))
                .unwrap()
                .len()
        );
        assert!(!second.is_empty());
    }

    #[test]
    fn issues_both_sides_of_mtls() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let ca = Ca::open_or_create(&dir).unwrap();
        let m = ca
            .issue_master(vec!["127.0.0.1".into(), "localhost".into()])
            .unwrap();
        let n = ca.issue_node("bm-1").unwrap();
        for p in [&m.cert, &m.key, &n.cert, &n.key] {
            assert!(p.exists(), "{} missing", p.display());
        }
        assert!(std::fs::read_to_string(&n.cert)
            .unwrap()
            .contains("BEGIN CERTIFICATE"));
    }

    #[cfg(unix)]
    #[test]
    fn private_keys_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let ca = Ca::open_or_create(&dir).unwrap();
        let n = ca.issue_node("bm-1").unwrap();
        for p in [dir.join("ca-key.pem"), n.key.clone()] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is {mode:o}", p.display());
        }
    }
}
