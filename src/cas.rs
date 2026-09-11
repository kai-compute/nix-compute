use crate::model::InputArtifact;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn materialize_input(input: &InputArtifact, destination: &Path) -> anyhow::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(destination.parent().unwrap_or(Path::new(".")))?;
    let file = temporary.as_file_mut();
    if let Some(path) = input.uri.strip_prefix("file://") {
        let mut source = fs::File::open(path)?;
        std::io::copy(&mut source, file)?;
    } else if let Some(path) = input.uri.strip_prefix("cas://sha256/") {
        anyhow::ensure!(
            crate::model::digest_valid(path) && path.eq_ignore_ascii_case(&input.sha256),
            "CAS URI must contain the declared SHA256 digest"
        );
        let root = std::env::var_os("NIX_COMPUTE_CAS_ROOT")
            .ok_or_else(|| anyhow::anyhow!("NIX_COMPUTE_CAS_ROOT is required for cas:// inputs"))?;
        let source = PathBuf::from(root).join(path);
        let mut source = fs::File::open(source)?;
        std::io::copy(&mut source, file)?;
    } else if input.uri.starts_with("http://") || input.uri.starts_with("https://") {
        let response = ureq::get(&input.uri).call()?;
        let mut response = response.into_reader();
        std::io::copy(&mut response, file)?;
    } else if input.uri.starts_with("s3://") {
        let endpoint = std::env::var("NIX_COMPUTE_S3_ENDPOINT").map_err(|_| {
            anyhow::anyhow!("s3:// inputs require NIX_COMPUTE_S3_ENDPOINT (anonymous HTTP gateway)")
        })?;
        let url = s3_url(&endpoint, &input.uri)?;
        let response = ureq::get(&url).call()?;
        let mut response = response.into_reader();
        std::io::copy(&mut response, file)?;
    } else {
        anyhow::bail!(
            "unsupported input URI {}; use file://, cas://, http(s)://, or s3://",
            input.uri
        );
    }
    file.flush()?;
    let digest = sha256_file(temporary.path())?;
    anyhow::ensure!(
        digest.eq_ignore_ascii_case(&input.sha256),
        "input digest mismatch: expected {}, got {}",
        input.sha256,
        digest
    );
    use std::os::unix::fs::PermissionsExt;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o444))?;
    temporary.persist_noclobber(destination)?;
    Ok(())
}

pub fn upload_output(path: &Path, destination: &str, digest: &str) -> anyhow::Result<String> {
    let uri = if let Some(root) = destination.strip_prefix("file://") {
        let target = PathBuf::from(root).join("sha256").join(digest);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(path, &target)?;
        format!("file://{}", target.display())
    } else if destination.starts_with("s3://") {
        let endpoint = std::env::var("NIX_COMPUTE_S3_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("s3:// outputs require NIX_COMPUTE_S3_ENDPOINT"))?;
        let url = s3_url(&endpoint, &format!("{destination}/sha256/{digest}"))?;
        ureq::put(&url).send(fs::File::open(path)?)?;
        url
    } else {
        anyhow::bail!(
            "unsupported output destination {}; use file:// or s3://",
            destination
        );
    };
    Ok(uri)
}

fn s3_url(endpoint: &str, uri: &str) -> anyhow::Result<String> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow::anyhow!("invalid s3 URI"))?;
    let endpoint = endpoint.trim_end_matches('/');
    Ok(format!("{endpoint}/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staging_preserves_other_inputs_and_rejects_bad_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"training data").unwrap();
        let digest = sha256_file(&source).unwrap();
        let mut input = InputArtifact {
            uri: format!("file://{}", source.display()),
            sha256: digest,
            path: "data.bin".into(),
        };
        let old = dir.path().join("data.download");
        fs::write(&old, b"another input").unwrap();
        materialize_input(&input, &dir.path().join("data.bin")).unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"another input");
        input.sha256 = "0".repeat(64);
        assert!(materialize_input(&input, &dir.path().join("bad.bin")).is_err());
        assert!(!dir.path().join("bad.bin").exists());
        input.uri = "cas://sha256/../../etc/passwd".into();
        assert!(materialize_input(&input, &dir.path().join("bad.bin")).is_err());
    }
}
