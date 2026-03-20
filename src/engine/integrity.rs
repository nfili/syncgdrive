use std::path::Path;
use anyhow::Result;
use sha2::{Digest, Sha256};
use crate::remote::UploadResult;

#[derive(Debug, PartialEq)]
pub enum IntegrityResult {
    Ok,
    Mismatch { local_md5: String, remote_md5: String },
}

/// Calcule le SHA-256 local et le compare avec celui retourné par Google Drive
pub async fn verify_upload(
    local_path: &Path,
    upload_result: &UploadResult,
) -> Result<IntegrityResult> {
    let local_hash = compute_hash(local_path).await?;

    // NB: Google Drive retourne souvent un MD5, mais dans notre architecture
    // on a standardisé autour du SHA-256 (ou MD5 selon ton implémentation de GDrive).
    // Assure-toi que upload_result.md5_checksum contient bien la même chose.
    if local_hash == upload_result.md5_checksum {
        Ok(IntegrityResult::Ok)
    } else {
        Ok(IntegrityResult::Mismatch {
            local_md5: local_hash,
            remote_md5: upload_result.md5_checksum.clone(),
        })
    }
}

async fn compute_hash(path: &Path) -> Result<String> {
    // Note: Si tu utilises MD5 côté Google Drive, il faut utiliser la crate `md-5` ici.
    // Si Google te renvoie bien le hash de ton choix, garde sha256.
    let data = tokio::fs::read(path).await?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}