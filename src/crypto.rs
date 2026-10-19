use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use rand::random;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct SecretBox {
    cipher: ChaCha20Poly1305,
}

impl SecretBox {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let key = if path.exists() {
            let encoded = fs::read_to_string(path).context("read master key")?;
            URL_SAFE_NO_PAD
                .decode(encoded.trim())
                .context("decode master key")?
        } else {
            let bytes = random::<[u8; 32]>();
            write_private(path, &URL_SAFE_NO_PAD.encode(bytes))?;
            bytes.to_vec()
        };
        if key.len() != 32 {
            bail!("master key must contain exactly 32 bytes");
        }
        Ok(Self {
            cipher: ChaCha20Poly1305::new_from_slice(&key).expect("validated key length"),
        })
    }

    /// Seals a value for one column of one row: the binding is authenticated
    /// with it, so a copy pasted into another row or column does not open (L7).
    pub fn seal(&self, binding: &Binding, plaintext: &str) -> Result<String> {
        let aad = binding.associated_data();
        let nonce = random::<[u8; 12]>();
        let ciphertext = self
            .cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("secret encryption failed"))?;
        let mut value = nonce.to_vec();
        value.extend(ciphertext);
        Ok(format!("{BOUND}{}", URL_SAFE_NO_PAD.encode(value)))
    }

    /// Opens a value sealed for this binding. Unbound values from before 0.2.0
    /// are refused here; the store re-seals them once when it opens.
    pub fn open(&self, binding: &Binding, encoded: &str) -> Result<String> {
        let Some(encoded) = encoded.strip_prefix(BOUND) else {
            bail!("secret is not bound to its row");
        };
        self.decrypt(encoded, binding.associated_data().as_bytes())
    }

    /// Opens a value sealed before secrets were bound to their rows. Only the
    /// store's one-time re-sealing calls this.
    pub(crate) fn open_unbound(&self, encoded: &str) -> Result<String> {
        self.decrypt(encoded, b"")
    }

    fn decrypt(&self, encoded: &str, aad: &[u8]) -> Result<String> {
        let value = URL_SAFE_NO_PAD.decode(encoded).context("decode secret")?;
        if value.len() < 13 {
            bail!("encrypted secret is truncated");
        }
        let plaintext = self
            .cipher
            .decrypt(
                (&value[..12]).into(),
                Payload {
                    msg: &value[12..],
                    aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("secret decryption failed"))?;
        String::from_utf8(plaintext).context("secret is not UTF-8")
    }
}

/// Marks a value sealed with its binding. Unbound values are bare base64,
/// which never contains a dot.
const BOUND: &str = "v2.";

/// The table, row and column a sealed value belongs to.
pub struct Binding<'a> {
    table: &'static str,
    column: &'static str,
    row: &'a str,
}

impl<'a> Binding<'a> {
    pub fn new(table: &'static str, column: &'static str, row: &'a str) -> Self {
        Self { table, column, row }
    }

    fn associated_data(&self) -> String {
        format!("poundlock|{}|{}|{}", self.table, self.column, self.row)
    }
}

pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random::<[u8; 32]>())
}

pub fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

pub fn sha256_hex_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer).context("read file for sha256")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(digest: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

pub fn token_matches(expected_hash: &str, supplied: &str) -> bool {
    let supplied = token_hash(supplied);
    expected_hash.as_bytes().ct_eq(supplied.as_bytes()).into()
}

pub fn write_private(path: &Path, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create secret directory")?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("create secret file")?;
    writeln!(file, "{value}").context("write secret file")?;
    Ok(())
}

pub fn make_group_readable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).context("read shared secret permissions")?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o640);
        fs::set_permissions(path, permissions).context("set shared secret permissions")?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
impl SecretBox {
    /// A value in the format written before 0.2.0, for migration tests.
    pub(crate) fn encrypt_unbound_for_test(&self, plaintext: &str) -> String {
        let nonce = random::<[u8; 12]>();
        let mut value = nonce.to_vec();
        value.extend(
            self.cipher
                .encrypt((&nonce).into(), plaintext.as_bytes())
                .unwrap(),
        );
        URL_SAFE_NO_PAD.encode(value)
    }
}

#[cfg(test)]
mod secret_box_tests {
    use super::*;

    fn secrets() -> SecretBox {
        let directory = tempfile::tempdir().unwrap();
        SecretBox::load_or_create(&directory.path().join("master-key")).unwrap()
    }

    #[test]
    fn a_sealed_value_opens_only_for_its_own_row_and_column() {
        let secrets = secrets();
        let here = Binding::new("endpoints", "api_key_ciphertext", "a");
        let sealed = secrets.seal(&here, "sk-test").unwrap();
        assert_eq!(secrets.open(&here, &sealed).unwrap(), "sk-test");
        for elsewhere in [
            Binding::new("endpoints", "api_key_ciphertext", "b"),
            Binding::new("jobs", "api_key_ciphertext", "a"),
            Binding::new("endpoints", "source_ciphertext", "a"),
        ] {
            assert!(secrets.open(&elsewhere, &sealed).is_err());
        }
    }

    #[test]
    fn an_unbound_value_opens_only_through_the_re_sealing_path() {
        let secrets = secrets();
        let unbound = secrets.encrypt_unbound_for_test("legacy");
        assert!(!unbound.starts_with(BOUND));
        let here = Binding::new("endpoints", "api_key_ciphertext", "a");
        assert!(secrets.open(&here, &unbound).is_err());
        assert_eq!(secrets.open_unbound(&unbound).unwrap(), "legacy");
    }
}
