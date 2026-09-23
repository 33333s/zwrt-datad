//! Unattested, hardware-backed key identity. Enrollment trust belongs to the
//! operator, not this API. No initialization, rotation or cloud upload at boot.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use fs2::FileExt;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, Semaphore},
};
use zeroize::{Zeroize, Zeroizing};

const WORKER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/keymaster-worker"));
const MAX_RECORD: usize = 32 * 1024;
const MAX_BLOB: usize = 8192;
const MAX_OUTPUT: usize = MAX_BLOB + 512;
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
pub const DOMAIN: &[u8] = b"zwrt-datad-identity-v1\0";

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub message: &'static str,
    pub status: u16,
}
impl Error {
    fn uninitialized() -> Self {
        Self {
            code: "identity_not_initialized",
            message: "explicit initialization is required",
            status: 409,
        }
    }
    fn storage() -> Self {
        Self {
            code: "identity_storage_error",
            message: "identity storage is unavailable, insecure or corrupt; existing keys were not replaced",
            status: 500,
        }
    }
    fn backend() -> Self {
        Self {
            code: "identity_backend_unavailable",
            message: "hardware identity backend is unavailable",
            status: 503,
        }
    }
    fn key() -> Self {
        Self {
            code: "identity_key_unusable",
            message: "hardware key is invalid, foreign, unsupported or requires explicit recovery",
            status: 409,
        }
    }
    fn busy() -> Self {
        Self {
            code: "identity_busy",
            message: "another identity operation is running",
            status: 429,
        }
    }
    fn input() -> Self {
        Self {
            code: "invalid_identity_request",
            message: "provide purpose enroll/authenticate and a canonical base64url 32-byte challenge",
            status: 400,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    key_blob: String,
    public_key_spki: String,
}
impl Drop for Record {
    fn drop(&mut self) {
        self.key_blob.zeroize();
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Enroll,
    Authenticate,
}
impl Purpose {
    fn text(&self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Authenticate => "authenticate",
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignRequest {
    pub purpose: Purpose,
    pub challenge: String,
}

pub struct Identity {
    directory: PathBuf,
    slots: Arc<Semaphore>,
    last_sign: Mutex<Option<Instant>>,
}
impl Identity {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            directory: data_dir.join("identity"),
            slots: Arc::new(Semaphore::new(1)),
            last_sign: Mutex::new(None),
        }
    }
    fn directory(&self, create: bool) -> Result<(), Error> {
        let parent = self.directory.parent().ok_or_else(Error::storage)?;
        if create {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|_| Error::storage())?;
        }
        let owner = unsafe { libc::geteuid() };
        let parent_metadata = fs::symlink_metadata(parent).map_err(|_| Error::storage())?;
        if !parent_metadata.is_dir()
            || parent_metadata.uid() != owner
            || parent_metadata.mode() & 0o022 != 0
        {
            return Err(Error::storage());
        }
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&self.directory) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(_) => return Err(Error::storage()),
            }
        }
        let metadata = fs::symlink_metadata(&self.directory).map_err(|_| Error::storage())?;
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
            return Err(Error::storage());
        }
        Ok(())
    }
    fn lock(&self) -> Result<File, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("operation.lock"))
            .map_err(|_| Error::storage())?;
        secure(&file)?;
        file.try_lock_exclusive().map_err(|_| Error::busy())?;
        Ok(file)
    }
    fn load(&self) -> Result<Record, Error> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("key.json"))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(if self.has_marker() {
                    Error::key()
                } else {
                    Error::uninitialized()
                });
            }
            Err(_) => return Err(Error::storage()),
        };
        secure(&file)?;
        let mut bytes = Vec::new();
        file.take(MAX_RECORD as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::storage())?;
        let record: Result<Record, _> = serde_json::from_slice(&bytes);
        let length = bytes.len();
        bytes.zeroize();
        let record = record.map_err(|_| Error::storage())?;
        if length > MAX_RECORD || record.schema != 1 {
            return Err(Error::storage());
        }
        let (mut blob, _) = decode_record(&record)?;
        blob.zeroize();
        Ok(record)
    }
    fn existing_directory(&self) -> Result<(), Error> {
        if fs::symlink_metadata(&self.directory)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(Error::uninitialized());
        }
        self.directory(false)
    }
    fn has_marker(&self) -> bool {
        fs::symlink_metadata(self.directory.join("initialized-key-id")).is_ok()
    }
    fn ensure_marker(&self, public: &[u8]) -> Result<(), Error> {
        let expected = fingerprint(public);
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("initialized-key-id"))
        {
            Ok(file) => {
                secure(&file)?;
                let mut actual = String::new();
                file.take(65)
                    .read_to_string(&mut actual)
                    .map_err(|_| Error::storage())?;
                if actual != expected {
                    return Err(Error::key());
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.has_marker() => {
                self.publish_new("initialized-key-id", expected.as_bytes())
            }
            Err(_) => Err(Error::storage()),
        }
    }
    fn save_new(&self, record: &Record) -> Result<(), Error> {
        let bytes = Zeroizing::new(serde_json::to_vec(record).map_err(|_| Error::storage())?);
        self.publish_new("key.json", &bytes)
    }
    fn publish_new(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let temp = self
            .directory
            .join(format!(".key-new-{:016x}", rand::random::<u64>()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)
            .map_err(|_| Error::storage())?;
        let result = (|| {
            let result = file.write_all(bytes).and_then(|_| file.sync_all());
            result.map_err(|_| Error::storage())?;
            // Atomic publication with NO replacement of an existing identity.
            fs::hard_link(&temp, self.directory.join(name)).map_err(|_| Error::storage())?;
            Ok(())
        })();
        let removed = fs::remove_file(&temp);
        result?;
        removed.map_err(|_| Error::storage())?;
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error::storage())
    }
    fn worker(&self) -> Result<PathBuf, Error> {
        // Native tests exercise the protocol through a fixture only. Release
        // binaries do not accept this override and never generate software keys.
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("ZWRT_DATAD_IDENTITY_TEST_WORKER") {
            return Ok(path.into());
        }
        if WORKER.is_empty() || !cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            return Err(Error::backend());
        }
        let path = self.directory.join("keymaster-worker");
        if let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            secure(&file)?;
            let mut bytes = Vec::new();
            file.take(WORKER.len() as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::storage())?;
            if bytes == WORKER {
                return Ok(path);
            }
        } else if fs::symlink_metadata(&path).is_ok() {
            return Err(Error::storage());
        }
        let temp = self
            .directory
            .join(format!(".worker-new-{:016x}", rand::random::<u64>()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)
            .map_err(|_| Error::storage())?;
        let result = file
            .write_all(WORKER)
            .and_then(|_| file.sync_all())
            .and_then(|_| fs::rename(&temp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            return Err(Error::storage());
        }
        Ok(path)
    }
    async fn invoke(
        &self,
        op: u8,
        blob: &[u8],
        message: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        if blob.len() > MAX_BLOB || message.len() > 512 {
            return Err(Error::input());
        }
        let worker = self.worker()?;
        let mut input = Zeroizing::new(vec![op]);
        input.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        input.extend_from_slice(&(message.len() as u32).to_be_bytes());
        input.extend_from_slice(blob);
        input.extend_from_slice(message);
        let mut command = Command::new(worker);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(debug_assertions)]
        if let Some(value) = std::env::var_os("ZWRT_DATAD_IDENTITY_TEST_CONFIG") {
            command.env("ZWRT_DATAD_IDENTITY_TEST_CONFIG", value);
        }
        let mut child = command.spawn().map_err(|_| Error::backend())?;
        let mut stdin = child.stdin.take().ok_or_else(Error::backend)?;
        let stdout = child.stdout.take().ok_or_else(Error::backend)?;
        let transaction = async {
            stdin
                .write_all(&input)
                .await
                .map_err(|_| Error::backend())?;
            drop(stdin);
            let mut output = Zeroizing::new(Vec::new());
            stdout
                .take(MAX_OUTPUT as u64 + 1)
                .read_to_end(&mut output)
                .await
                .map_err(|_| Error::backend())?;
            let status = child.wait().await.map_err(|_| Error::backend())?;
            if !status.success() {
                output.zeroize();
                return Err(Error::backend());
            }
            let reply = parse_reply(&output);
            output.zeroize();
            reply
        };
        let result = tokio::time::timeout(Duration::from_secs(8), transaction).await;
        input.zeroize();
        match result {
            Ok(result) => result,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(Error::backend())
            }
        }
    }
    async fn checked_record(&self) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let record = self.load()?;
        let (mut blob, public) = decode_record(&record)?;
        match self.invoke(2, &blob, &[]).await {
            Ok((empty, actual)) if empty.is_empty() && actual == public => {
                if let Err(error) = self.ensure_marker(&public) {
                    blob.zeroize();
                    return Err(error);
                }
                Ok((blob, public))
            }
            Ok(_) => {
                blob.zeroize();
                Err(Error::key())
            }
            Err(error) => {
                blob.zeroize();
                Err(error)
            }
        }
    }
    pub async fn initialize(&self) -> Result<Value, Error> {
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::busy())?;
        self.directory(true)?;
        let _lock = self.lock()?;
        match self.load() {
            Ok(_) => {
                let (mut blob, public) = self.checked_record().await?;
                blob.zeroize();
                let mut value = public_json(&public);
                value["created"] = json!(false);
                return Ok(value);
            }
            Err(error) if error.code == "identity_not_initialized" && !self.has_marker() => (),
            Err(error) => return Err(error),
        }
        let (mut blob, public) = self.invoke(1, &[], &[]).await?;
        if blob.is_empty() || !valid_public(&public) {
            blob.zeroize();
            return Err(Error::key());
        }
        let message = [
            b"zwrt-datad-identity-self-test-v1\0".as_slice(),
            public.as_slice(),
        ]
        .concat();
        let test = self.invoke(3, &blob, &message).await;
        let record = Record {
            schema: 1,
            key_blob: STANDARD.encode(&blob),
            public_key_spki: STANDARD.encode(&public),
        };
        blob.zeroize();
        let (empty, signature) = test?;
        if !empty.is_empty() || !verify(&public, &message, &signature) {
            return Err(Error::key());
        }
        self.save_new(&record)?;
        self.ensure_marker(&public)?;
        let mut value = public_json(&public);
        value["created"] = json!(true);
        Ok(value)
    }
    pub async fn public_key(&self) -> Result<Value, Error> {
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::busy())?;
        self.existing_directory()?;
        let _lock = self.lock()?;
        let (mut blob, public) = self.checked_record().await?;
        blob.zeroize();
        Ok(public_json(&public))
    }
    pub async fn sign(&self, request: SignRequest) -> Result<Value, Error> {
        let nonce = nonce(&request.challenge)?;
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::busy())?;
        let mut last = self.last_sign.lock().await;
        if last.is_some_and(|at| at.elapsed() < Duration::from_millis(250)) {
            return Err(Error {
                code: "identity_rate_limited",
                message: "too many signing requests",
                status: 429,
            });
        }
        *last = Some(Instant::now());
        drop(last);
        self.existing_directory()?;
        let _lock = self.lock()?;
        let (mut blob, public) = self.checked_record().await?;
        let key_id = fingerprint(&public);
        let message = signing_message(&key_id, request.purpose.text(), &nonce);
        let result = self.invoke(3, &blob, &message).await;
        blob.zeroize();
        let (empty, signature) = result?;
        if !empty.is_empty() || !verify(&public, &message, &signature) {
            return Err(Error::key());
        }
        Ok(
            json!({"key_id":key_id,"algorithm":"ECDSA-P256-SHA256","signature_format":"asn1-der", "attested":false,
            "purpose":request.purpose.text(),"challenge":request.challenge,"signature":STANDARD.encode(signature),"signed_message":STANDARD.encode(message)}),
        )
    }
}

fn secure(file: &File) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::storage())?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
    {
        Err(Error::storage())
    } else {
        Ok(())
    }
}
fn valid_public(public: &[u8]) -> bool {
    public.len() == 91 && public.starts_with(SPKI_PREFIX) && public[26] == 4
}
fn fingerprint(public: &[u8]) -> String {
    Sha256::digest(public)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn verify(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
    valid_public(public)
        && UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &public[26..])
            .verify(message, signature)
            .is_ok()
}
fn nonce(value: &str) -> Result<[u8; 32], Error> {
    if value.len() != 43 {
        return Err(Error::input());
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| Error::input())?;
    if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(Error::input());
    }
    bytes.try_into().map_err(|_| Error::input())
}
fn signing_message(key_id: &str, purpose: &str, nonce: &[u8; 32]) -> Vec<u8> {
    [
        DOMAIN,
        purpose.as_bytes(),
        b"\0",
        key_id.as_bytes(),
        b"\0",
        nonce,
    ]
    .concat()
}
fn decode_record(record: &Record) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut blob = STANDARD
        .decode(&record.key_blob)
        .map_err(|_| Error::storage())?;
    let public = STANDARD
        .decode(&record.public_key_spki)
        .map_err(|_| Error::storage())?;
    if blob.is_empty() || blob.len() > MAX_BLOB || !valid_public(&public) {
        blob.zeroize();
        return Err(Error::storage());
    }
    Ok((blob, public))
}
fn public_json(public: &[u8]) -> Value {
    let encoded = STANDARD.encode(public);
    let lines = encoded
        .as_bytes()
        .chunks(64)
        .map(|b| std::str::from_utf8(b).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let backend = if cfg!(debug_assertions)
        && std::env::var_os("ZWRT_DATAD_IDENTITY_TEST_WORKER").is_some()
    {
        "test-only"
    } else {
        "qcom-keymaster"
    };
    json!({"key_id":fingerprint(public),"key_id_scheme":"sha256-spki","algorithm":"ECDSA-P256-SHA256", "backend":backend,"attested":false,
        "public_key_spki":encoded,"public_key_pem":format!("-----BEGIN PUBLIC KEY-----\n{lines}\n-----END PUBLIC KEY-----\n")})
}
fn parse_reply(output: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error> {
    if output.len() < 17 || output.len() > MAX_OUTPUT || &output[..5] != b"DDKI1" {
        return Err(Error::backend());
    }
    let code = i32::from_be_bytes(output[5..9].try_into().unwrap());
    let a = u32::from_be_bytes(output[9..13].try_into().unwrap()) as usize;
    let b = u32::from_be_bytes(output[13..17].try_into().unwrap()) as usize;
    if a > MAX_BLOB || b > 256 || output.len() != 17 + a + b {
        return Err(Error::backend());
    }
    if code != 0 {
        return Err(if matches!(code, -33 | -62 | -1004) {
            Error::key()
        } else {
            Error::backend()
        });
    }
    Ok((output[17..17 + a].to_vec(), output[17 + a..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_and_protocol_are_strict() {
        let encoded = URL_SAFE_NO_PAD.encode([7; 32]);
        assert_eq!(nonce(&encoded).unwrap(), [7; 32]);
        assert!(nonce(&(encoded + "=")).is_err());
        assert!(nonce("a").is_err());
        assert!(parse_reply(b"logs before protocol").is_err());
        let mut frame = b"DDKI1".to_vec();
        frame.extend_from_slice(&0i32.to_be_bytes());
        frame.extend_from_slice(&u32::MAX.to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes());
        assert!(parse_reply(&frame).is_err());
        let enroll = signing_message("key", "enroll", &[1; 32]);
        assert_ne!(enroll, signing_message("key", "authenticate", &[1; 32]));
        assert_ne!(enroll, signing_message("other", "enroll", &[1; 32]));
    }
}
