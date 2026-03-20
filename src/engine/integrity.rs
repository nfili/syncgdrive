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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::UploadResult;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_verify_upload_mismatch() {
        // 1. Créer un fichier local de test
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "Contenu très important de Bella").unwrap();
        let local_path = file.path();

        // 2. Simuler un retour de l'API Google avec un mauvais hash (corruption réseau)
        let bad_upload = UploadResult {
            drive_id: "fichier_corrompu_123".into(),
            md5_checksum: "mauvais_hash_12345".into(),
            size_bytes: 32,
        };

        // 3. Vérifier l'intégrité
        let result = verify_upload(local_path, &bad_upload).await.unwrap();

        // 4. On s'attend à ce que le bouclier détecte le Mismatch
        match result {
            IntegrityResult::Mismatch { local_md5, remote_md5 } => {
                assert_ne!(local_md5, remote_md5, "Les hashs doivent être différents");
                assert_eq!(remote_md5, "mauvais_hash_12345");
                // Le local_md5 sera le vrai SHA-256 calculé de notre phrase
            }
            _ => panic!("Le bouclier d'intégrité n'a pas détecté la corruption !"),
        }
    }

    #[tokio::test]
    async fn test_verify_upload_ok() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "Contenu validé").unwrap();
        let local_path = file.path();

        // On calcule le vrai hash pour simuler un Google Drive honnête
        let expected_hash = compute_hash(local_path).await.unwrap();

        let good_upload = UploadResult {
            drive_id: "fichier_parfait_123".into(),
            md5_checksum: expected_hash,
            size_bytes: 15,
        };

        let result = verify_upload(local_path, &good_upload).await.unwrap();
        assert_eq!(result, IntegrityResult::Ok, "Le fichier intact doit être validé");
    }
}