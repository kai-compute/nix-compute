use crate::canonical::{canonical_json, sha256_value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::Write, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    pub version: u32,
    pub payload: Value,
    pub signature: SignatureEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    pub algorithm: String,
    pub center_id: String,
    pub key_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyFile {
    pub algorithm: String,
    pub center_id: String,
    pub key_id: String,
    pub private_key: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustStore {
    pub version: u32,
    pub centers: Vec<TrustedKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedKey {
    pub center_id: String,
    pub key_id: String,
    pub public_key: String,
    #[serde(default)]
    pub revoked: bool,
}

pub fn generate_key(path: &Path, center_id: &str, key_id: &str) -> anyhow::Result<()> {
    let signing = SigningKey::generate(&mut OsRng);
    let public = signing.verifying_key();
    let file = KeyFile {
        algorithm: "Ed25519".to_string(),
        center_id: center_id.to_string(),
        key_id: key_id.to_string(),
        private_key: URL_SAFE_NO_PAD.encode(signing.to_bytes()),
        public_key: URL_SAFE_NO_PAD.encode(public.to_bytes()),
    };
    let bytes = serde_json::to_vec_pretty(&file)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&bytes)?;
    Ok(())
}

pub struct SigningIdentity {
    signing: SigningKey,
    center_id: String,
    key_id: String,
}

impl SigningIdentity {
    pub fn load(key_path: &Path) -> anyhow::Result<Self> {
        let key: KeyFile = serde_json::from_slice(&fs::read(key_path)?)?;
        anyhow::ensure!(key.algorithm == "Ed25519", "unsupported signing algorithm");
        let raw = URL_SAFE_NO_PAD.decode(key.private_key)?;
        let raw: [u8; 32] = raw
            .try_into()
            .map_err(|_| anyhow::anyhow!("private key must be 32 bytes"))?;
        let signing = SigningKey::from_bytes(&raw);
        anyhow::ensure!(
            URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes()) == key.public_key,
            "signing key public/private mismatch"
        );
        anyhow::ensure!(
            !key.center_id.is_empty() && !key.key_id.is_empty(),
            "empty signing identity"
        );
        Ok(Self {
            signing,
            center_id: key.center_id,
            key_id: key.key_id,
        })
    }

    pub fn sign(&self, payload: Value) -> anyhow::Result<Attestation> {
        let signature = self.signing.sign(&signature_bytes(
            2,
            &self.center_id,
            &self.key_id,
            &payload,
        )?);
        Ok(Attestation {
            version: 2,
            payload,
            signature: SignatureEnvelope {
                algorithm: "Ed25519".to_string(),
                center_id: self.center_id.clone(),
                key_id: self.key_id.clone(),
                value: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            },
        })
    }
}

fn signature_bytes(
    version: u32,
    center: &str,
    key: &str,
    payload: &Value,
) -> anyhow::Result<Vec<u8>> {
    match version {
        1 => canonical_json(payload),
        2 => canonical_json(
            &serde_json::json!({"domain": "nix-compute-attestation", "version": 2,
            "algorithm": "Ed25519", "center_id": center, "key_id": key, "payload": payload}),
        ),
        _ => anyhow::bail!("unsupported attestation version"),
    }
}

pub fn verify(attestation: &Attestation, trust_path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(
        matches!(attestation.version, 1 | 2),
        "unsupported attestation version"
    );
    anyhow::ensure!(
        attestation.signature.algorithm == "Ed25519",
        "unsupported signature algorithm"
    );
    let trust: TrustStore = serde_json::from_slice(&fs::read(trust_path)?)?;
    anyhow::ensure!(trust.version == 1, "unsupported trust store version");
    let trusted = trust
        .centers
        .iter()
        .find(|key| {
            key.center_id == attestation.signature.center_id
                && key.key_id == attestation.signature.key_id
                && !key.revoked
        })
        .ok_or_else(|| anyhow::anyhow!("signing key is not trusted"))?;
    let public = URL_SAFE_NO_PAD.decode(&trusted.public_key)?;
    let public: [u8; 32] = public
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&public)?;
    let signature = URL_SAFE_NO_PAD.decode(&attestation.signature.value)?;
    let signature = Signature::from_slice(&signature)?;
    key.verify(
        &signature_bytes(
            attestation.version,
            &attestation.signature.center_id,
            &attestation.signature.key_id,
            &attestation.payload,
        )?,
        &signature,
    )?;
    sha256_value(&attestation.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn signed_payload_verifies_against_trust_store() {
        let directory = tempdir().unwrap();
        let key_path = directory.path().join("key.json");
        generate_key(&key_path, "center-a", "key-a").unwrap();
        let key: KeyFile = serde_json::from_slice(&fs::read(&key_path).unwrap()).unwrap();
        let trust_path = directory.path().join("trust.json");
        fs::write(
            &trust_path,
            serde_json::to_vec(&TrustStore {
                version: 1,
                centers: vec![TrustedKey {
                    center_id: key.center_id.clone(),
                    key_id: key.key_id.clone(),
                    public_key: key.public_key,
                    revoked: false,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let attestation = SigningIdentity::load(&key_path)
            .unwrap()
            .sign(serde_json::json!({"job_id": "abc", "target_id": "cuda", "status": "succeeded"}))
            .unwrap();
        assert!(verify(&attestation, &trust_path).is_ok());
        let mut tampered = attestation.clone();
        tampered.payload["target_id"] = "metal".into();
        assert!(verify(&tampered, &trust_path).is_err());
        let mut tampered = attestation.clone();
        tampered.signature.center_id = "another-center".into();
        assert!(verify(&tampered, &trust_path).is_err());
        let mut trust: TrustStore =
            serde_json::from_slice(&fs::read(&trust_path).unwrap()).unwrap();
        let mut alias = trust.centers[0].clone();
        alias.center_id = "another-center".into();
        trust.centers.push(alias);
        fs::write(&trust_path, serde_json::to_vec(&trust).unwrap()).unwrap();
        assert!(verify(&tampered, &trust_path).is_err());
        let mut legacy = attestation.clone();
        legacy.version = 1;
        let signer = SigningIdentity::load(&key_path).unwrap();
        legacy.signature.value = URL_SAFE_NO_PAD.encode(
            signer
                .signing
                .sign(&canonical_json(&legacy.payload).unwrap())
                .to_bytes(),
        );
        assert!(verify(&legacy, &trust_path).is_ok());
        trust.centers[0].revoked = true;
        fs::write(&trust_path, serde_json::to_vec(&trust).unwrap()).unwrap();
        assert!(verify(&attestation, &trust_path).is_err());
        assert!(generate_key(&key_path, "center-b", "key-b").is_err());
    }
}
