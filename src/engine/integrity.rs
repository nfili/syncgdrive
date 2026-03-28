//! Vérification de l'intégrité des données post-transfert.
//!
//! Ce module s'assure qu'aucun fichier n'a été corrompu pendant son transit
//! vers Google Drive en comparant l'empreinte cryptographique (MD5) calculée
//! localement avec celle renvoyée par l'API distante.

use crate::remote::UploadResult;
use anyhow::Result;
use md5::{Digest, Md5};
use std::path::Path;

/// Résultat de la vérification d'intégrité cryptographique.
#[derive(Debug, PartialEq)]
pub enum IntegrityResult {
    /// Le fichier distant est bit-à-bit identique au fichier local.
    Ok,
    /// Une corruption a été détectée (les empreintes diffèrent).
    Mismatch {
        local_md5: String,
        remote_md5: String,
    },
}

/// Calcule l'empreinte MD5 locale et la compare avec celle retournée par Google Drive.
///
/// L'API Google Drive utilise nativement MD5 (`md5Checksum`) pour valider
/// l'intégrité des fichiers uploadés. C'est pourquoi ce standard est imposé ici.
pub async fn verify_upload(
    local_path: &Path,
    upload_result: &UploadResult,
) -> Result<IntegrityResult> {
    let local_hash = compute_hash(local_path).await?;

    if local_hash == upload_result.md5_checksum {
        Ok(IntegrityResult::Ok)
    } else {
        Ok(IntegrityResult::Mismatch {
            local_md5: local_hash,
            remote_md5: upload_result.md5_checksum.clone(),
        })
    }
}

/// Calcule l'empreinte MD5 d'un fichier local de manière asynchrone.
pub async fn compute_hash(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path).await?;
    let mut h = Md5::new();
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
            IntegrityResult::Mismatch {
                local_md5,
                remote_md5,
            } => {
                assert_ne!(local_md5, remote_md5, "Les hashs doivent être différents");
                assert_eq!(remote_md5, "mauvais_hash_12345");
                // Le local_md5 sera le vrai MD5 calculé de notre phrase
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
        assert_eq!(
            result,
            IntegrityResult::Ok,
            "Le fichier intact doit être validé"
        );
    }
}