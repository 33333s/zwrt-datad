use super::fsops::version;
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};
struct Fixture {
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "datad-files-fixture-{:032x}",
                rand::random::<u128>()
            ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.path.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }
    fn revision(&self, p: &Path) -> String {
        version(&fs::symlink_metadata(p).unwrap())
    }
    fn session(&self) -> Session {
        Session::new(&self.path).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn listing_a_directory_alias_returns_actionable_canonical_entry_paths() {
    let f = Fixture::new();
    let real = f.path.join("real-directory");
    fs::create_dir(&real).unwrap();
    let file = real.join("name '中文.txt");
    fs::write(&file, b"original").unwrap();
    let alias = f.path.join("alias-directory");
    symlink(&real, &alias).unwrap();
    let mut session = f.session();
    let listing = call(&mut session, "files.list", json!({"path":alias})).await;
    assert_eq!(listing["path"], alias.to_string_lossy().as_ref());
    assert_eq!(listing["real_path"], real.to_string_lossy().as_ref());
    let entry = &listing["entries"][0];
    assert_eq!(entry["path"], file.to_string_lossy().as_ref());
    let result = upload(
        &mut session,
        &file,
        b"new bytes",
        Some(entry["version"].as_str().unwrap().to_owned()),
    )
    .await;
    assert_eq!(result["entry"]["path"], entry["path"]);
    assert_eq!(fs::read(&file).unwrap(), b"new bytes");
}
async fn call(s: &mut Session, a: &str, p: Value) -> Value {
    s.execute(a, p, true).await.unwrap()
}
async fn upload(s: &mut Session, p: &Path, bytes: &[u8], v: Option<String>) -> Value {
    let mut req = json!({"path":p,"size":bytes.len(),"sha256":sha(bytes)});
    if let Some(v) = v {
        req["expected_version"] = json!(v);
    }
    let begin = call(s, "files.upload.begin", req).await;
    let id = begin["transfer_id"].as_str().unwrap();
    let mut offset = 0;
    for b in bytes.chunks(CHUNK_BYTES) {
        call(
            s,
            "files.upload.chunk",
            json!({"transfer_id":id,"offset":offset,"data":STANDARD.encode(b)}),
        )
        .await;
        offset += b.len();
    }
    call(s, "files.upload.commit", json!({"transfer_id":id})).await
}
#[tokio::test]
async fn basic_operations_and_paginated_unicode_names_preserve_content() {
    let f = Fixture::new();
    let mut s = f.session();
    let dir = f.path.join("目录 'with quotes");
    call(&mut s, "files.mkdir", json!({"path":dir})).await;
    let file = dir.join("file '中文.txt");
    let touched = call(&mut s, "files.touch", json!({"path":file})).await;
    assert_eq!(touched["entry"]["mode"], "0644");
    fs::write(&file, b"retain").unwrap();
    let rev = f.revision(&file);
    call(
        &mut s,
        "files.touch",
        json!({"path":file,"expected_version":rev}),
    )
    .await;
    assert_eq!(fs::read(&file).unwrap(), b"retain");
    let rev = f.revision(&file);
    let changed = call(
        &mut s,
        "files.chmod",
        json!({"path":file,"mode":"640","expected_version":rev}),
    )
    .await;
    assert_eq!(changed["entry"]["mode"], "0640");
    assert_eq!(changed["entry"]["perm"], "rw-r-----");
    let renamed = call(
        &mut s,
        "files.rename",
        json!({"path":file,"name":"renamed 空.txt","expected_version":changed["entry"]["version"]}),
    )
    .await;
    assert_eq!(renamed["entry"]["name"], "renamed 空.txt");
    for n in ["a", "b", "c"] {
        fs::write(dir.join(n), b"").unwrap();
    }
    let mut cursor = Value::Null;
    let mut names = Vec::new();
    loop {
        let mut req = json!({"path":dir,"limit":2});
        if !cursor.is_null() {
            req["cursor"] = cursor;
        }
        let page = call(&mut s, "files.list", req).await;
        names.extend(
            page["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_owned()),
        );
        if page["complete"] == true {
            break;
        }
        cursor = page["next_cursor"].clone();
    }
    assert_eq!(names.len(), 4);
    let disk = call(&mut s, "files.disk", json!({"path":dir})).await;
    assert!(disk["total"].as_u64().unwrap() > 0);
    let rev = f.revision(&dir);
    call(
        &mut s,
        "files.remove",
        json!({"path":dir,"recursive":true,"expected_version":rev}),
    )
    .await;
    assert!(!dir.exists());
}
#[tokio::test]
async fn writes_require_confirmation_and_versions_and_never_silently_overwrite() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("file", b"baseline");
    let stale = f.revision(&p);
    fs::write(&p, b"external").unwrap();
    let e = s
        .execute(
            "files.chmod",
            json!({"path":p,"mode":"777","expected_version":stale}),
            true,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "files_version_conflict");
    assert_eq!(
        s.execute(
            "files.remove",
            json!({"path":p,"recursive":false,"expected_version":f.revision(&p)}),
            false
        )
        .await
        .unwrap_err()
        .code,
        "confirmation_required"
    );
    let q = f.file("target", b"target");
    assert_eq!(
        s.execute(
            "files.rename",
            json!({"path":p,"name":"target","expected_version":f.revision(&p)}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_exists"
    );
    assert_eq!(fs::read(&q).unwrap(), b"target");
    assert_eq!(
        s.execute(
            "files.remove",
            json!({"path":"/","recursive":true,"expected_version":"a".repeat(64)}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_root_protected"
    );
    assert_eq!(fs::read(&p).unwrap(), b"external");
}
#[tokio::test]
async fn streamed_upload_download_empty_and_overwrite_preserve_owner_permissions() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("existing", b"old");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
    let original = fs::metadata(&p).unwrap();
    let data = vec![0x5a; CHUNK_BYTES * 33 + 7];
    let result = upload(&mut s, &p, &data, Some(f.revision(&p))).await;
    assert_eq!(result["sha256"], sha(&data));
    let actual = fs::metadata(&p).unwrap();
    assert_eq!(
        (actual.uid(), actual.gid(), actual.mode() & 0o7777),
        (original.uid(), original.gid(), 0o640)
    );
    let begin = s
        .execute(
            "files.download.begin",
            json!({"path":p,"expected_version":result["entry"]["version"]}),
            false,
        )
        .await
        .unwrap();
    let id = begin["transfer_id"].clone();
    let mut received = Vec::new();
    loop {
        let out = s
            .execute(
                "files.download.chunk",
                json!({"transfer_id":id,"offset":received.len()}),
                false,
            )
            .await
            .unwrap();
        let bytes = STANDARD.decode(out["data"].as_str().unwrap()).unwrap();
        assert!(bytes.len() <= CHUNK_BYTES);
        received.extend(bytes);
        if out["eof"] == true {
            assert_eq!(out["sha256"], sha(&data));
            break;
        }
        assert!(out.get("sha256").is_none());
    }
    assert_eq!(received, data);
    s.execute("files.download.end", json!({"transfer_id":id}), false)
        .await
        .unwrap();
    let empty = f.path.join("empty");
    upload(&mut s, &empty, b"", None).await;
    assert_eq!(fs::read(empty).unwrap(), b"");
}
#[tokio::test]
async fn invalid_offsets_digest_and_cancel_abort_only_owned_temporary_file() {
    let f = Fixture::new();
    let mut s = f.session();
    let target = f.path.join("future");
    let begin = call(
        &mut s,
        "files.upload.begin",
        json!({"path":target,"size":3,"sha256":sha(b"abc")}),
    )
    .await;
    let id = begin["transfer_id"].clone();
    assert_eq!(
        s.execute(
            "files.upload.chunk",
            json!({"transfer_id":id,"offset":1,"data":STANDARD.encode(b"abc")}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_offset_mismatch"
    );
    call(
        &mut s,
        "files.upload.chunk",
        json!({"transfer_id":id,"offset":0,"data":STANDARD.encode(b"bad")}),
    )
    .await;
    assert_eq!(
        s.execute("files.upload.commit", json!({"transfer_id":id}), true)
            .await
            .unwrap_err()
            .code,
        "files_digest_mismatch"
    );
    assert!(!target.exists());
    let begin = call(
        &mut s,
        "files.upload.begin",
        json!({"path":target,"size":0,"sha256":sha(b"")}),
    )
    .await;
    call(
        &mut s,
        "files.upload.abort",
        json!({"transfer_id":begin["transfer_id"]}),
    )
    .await;
    assert!(fs::read_dir(&f.path).unwrap().next().is_none());
    let unrelated = f.file(".datad-file-unrelated", b"keep");
    call(
        &mut s,
        "files.upload.begin",
        json!({"path":target,"size":1,"sha256":sha(b"a")}),
    )
    .await;
    s.cancellation_token().store(true, Ordering::Release);
    assert_eq!(
        s.execute("files.list", json!({"path":f.path}), false)
            .await
            .unwrap_err()
            .code,
        "files_cancelled"
    );
    drop(s);
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    assert_eq!(fs::read_dir(&f.path).unwrap().count(), 1);
}
#[tokio::test]
async fn external_edits_and_symlink_replacement_cannot_be_overwritten_by_commit() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("edit", b"first");
    let req = json!({"path":p,"size":3,"sha256":sha(b"new"),"expected_version":f.revision(&p)});
    let b = call(&mut s, "files.upload.begin", req).await;
    call(
        &mut s,
        "files.upload.chunk",
        json!({"transfer_id":b["transfer_id"],"offset":0,"data":STANDARD.encode(b"new")}),
    )
    .await;
    fs::write(&p, b"external").unwrap();
    assert_eq!(
        s.execute(
            "files.upload.commit",
            json!({"transfer_id":b["transfer_id"]}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_version_conflict"
    );
    assert_eq!(fs::read(&p).unwrap(), b"external");
    let q = f.file("outside", b"outside");
    fs::remove_file(&p).unwrap();
    symlink(&q, &p).unwrap();
    assert_eq!(
        s.execute(
            "files.upload.begin",
            json!({"path":p,"size":3,"sha256":sha(b"new"),"expected_version":f.revision(&p)}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_invalid_type"
    );
    assert_eq!(fs::read(q).unwrap(), b"outside");
}
fn tar_bytes() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(5);
    h.set_mode(0o640);
    h.set_uid(unsafe { libc::geteuid() } as u64);
    h.set_gid(unsafe { libc::getegid() } as u64);
    h.set_cksum();
    builder
        .append_data(&mut h, "folder/中文.txt", &b"hello"[..])
        .unwrap();
    builder.into_inner().unwrap()
}
#[tokio::test]
async fn all_original_archive_codecs_extract_streams_and_directory_compress_roundtrips() {
    let f = Fixture::new();
    let mut s = f.session();
    let tar = tar_bytes();
    for suffix in ["tar", "tar.gz", "tar.bz2", "tar.xz", "zip"] {
        let data = match suffix {
            "tar" => tar.clone(),
            "tar.gz" => {
                let mut z =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
            "tar.bz2" => {
                let mut z = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
            "tar.xz" => {
                let mut z = xz2::write::XzEncoder::new(Vec::new(), 3);
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
            _ => {
                let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
                z.start_file(
                    "folder/中文.txt",
                    zip::write::SimpleFileOptions::default().unix_permissions(0o640),
                )
                .unwrap();
                z.write_all(b"hello").unwrap();
                z.finish().unwrap().into_inner()
            }
        };
        let archive = f.file(&format!("archive.{suffix}"), &data);
        let dest = f.path.join(format!("out-{suffix}"));
        fs::create_dir(&dest).unwrap();
        let out=call(&mut s,"files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false})).await;
        assert_eq!(out["verified"], true);
        assert_eq!(fs::read(dest.join("folder/中文.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::metadata(dest.join("folder/中文.txt")).unwrap().mode() & 0o777,
            0o640
        );
    }
    let source = f.path.join("out-tar");
    let compressed = call(
        &mut s,
        "files.compress",
        json!({"path":source,"expected_version":f.revision(&source)}),
    )
    .await;
    let archive = PathBuf::from(compressed["entry"]["path"].as_str().unwrap());
    let dest = f.path.join("roundtrip");
    fs::create_dir(&dest).unwrap();
    call(&mut s,"files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false})).await;
    assert_eq!(
        fs::read(dest.join("out-tar/folder/中文.txt")).unwrap(),
        b"hello"
    );
}
fn tar_entries(items: &[(&str, tar::EntryType, &str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (p, ty, link, body) in items {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o640);
        h.set_uid(unsafe { libc::geteuid() } as u64);
        h.set_gid(unsafe { libc::getegid() } as u64);
        h.set_size(body.len() as u64);
        h.set_entry_type(*ty);
        if !link.is_empty() {
            h.set_link_name(link).unwrap();
        }
        let bytes = h.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..p.len()].copy_from_slice(p.as_bytes());
        h.set_cksum();
        b.append(&h, *body).unwrap();
    }
    b.into_inner().unwrap()
}
#[tokio::test]
async fn archive_paths_unsafe_links_cycles_and_decoder_metadata_limits_fail_before_publication() {
    let f = Fixture::new();
    let mut s = f.session();
    let dest = f.path.join("dest");
    fs::create_dir(&dest).unwrap();
    for (name, items) in [
        (
            "parent",
            vec![("../escaped", tar::EntryType::Regular, "", &b"bad"[..])],
        ),
        (
            "absolute",
            vec![("/escaped", tar::EntryType::Regular, "", &b"bad"[..])],
        ),
        (
            "absolute-link",
            vec![("link", tar::EntryType::Symlink, "/etc/passwd", &b""[..])],
        ),
        (
            "escape-link",
            vec![("link", tar::EntryType::Symlink, "../../escape", &b""[..])],
        ),
        (
            "link-path",
            vec![
                ("link", tar::EntryType::Symlink, "inside", &b""[..]),
                ("link/file", tar::EntryType::Regular, "", &b"bad"[..]),
            ],
        ),
        (
            "cycle",
            vec![
                ("a", tar::EntryType::Symlink, "b", &b""[..]),
                ("b", tar::EntryType::Symlink, "a", &b""[..]),
            ],
        ),
        (
            "parent-cycle",
            vec![("a", tar::EntryType::Symlink, "a/child", &b""[..])],
        ),
        (
            "hard-outside",
            vec![("a", tar::EntryType::Link, "../outside", &b""[..])],
        ),
        ("device", vec![("a", tar::EntryType::Fifo, "", &b""[..])]),
    ] {
        let archive = f.file(&format!("{name}.tar"), &tar_entries(&items));
        let e=s.execute("files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":true}),true).await.unwrap_err();
        assert_eq!(e.code, "files_archive_unsafe", "{name}");
        assert!(fs::read_dir(&dest).unwrap().next().is_none());
    }
    let body = vec![b'a'; 8193];
    let archive = f.file(
        "extension.tar",
        &tar_entries(&[("././@LongLink", tar::EntryType::GNULongName, "", &body)]),
    );
    assert_eq!(s.execute("files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":true}),true).await.unwrap_err().code,"files_resource_limit");
    assert!(fs::read_dir(&dest).unwrap().next().is_none());
}
#[tokio::test]
async fn archives_retain_safe_relative_links_and_hardlinks_without_following_existing_outside_links()
 {
    let f = Fixture::new();
    let mut s = f.session();
    let dest = f.path.join("dest");
    fs::create_dir(&dest).unwrap();
    let archive = f.file(
        "links.tar",
        &tar_entries(&[
            ("file", tar::EntryType::Regular, "", b"hello"),
            ("hard", tar::EntryType::Link, "file", b""),
            ("sym", tar::EntryType::Symlink, "file", b""),
            ("dangling", tar::EntryType::Symlink, "missing", b""),
        ]),
    );
    let result=call(&mut s,"files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false})).await;
    assert_eq!(result["verified"], true);
    assert_eq!(fs::read(dest.join("sym")).unwrap(), b"hello");
    assert_eq!(
        fs::metadata(dest.join("file")).unwrap().ino(),
        fs::metadata(dest.join("hard")).unwrap().ino()
    );
    assert_eq!(
        fs::read_link(dest.join("dangling")).unwrap(),
        Path::new("missing")
    );
    let outside = f.file("outside", b"outside");
    symlink(&outside, dest.join("prior-link")).unwrap();
    let archive = f.file(
        "prior.tar",
        &tar_entries(&[("new-link", tar::EntryType::Symlink, "prior-link", b"")]),
    );
    assert_eq!(s.execute("files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":true}),true).await.unwrap_err().code,"files_archive_unsafe");
    assert!(!dest.join("new-link").exists());
    assert_eq!(fs::read(outside).unwrap(), b"outside");
}
#[tokio::test]
async fn session_deadline_transfer_caps_pagination_conflicts_and_download_changes_are_truthful() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("download", b"initial");
    let begin = s
        .execute(
            "files.download.begin",
            json!({"path":p,"expected_version":f.revision(&p)}),
            false,
        )
        .await
        .unwrap();
    fs::write(&p, b"changed").unwrap();
    assert_eq!(
        s.execute(
            "files.download.chunk",
            json!({"transfer_id":begin["transfer_id"],"offset":0}),
            false
        )
        .await
        .unwrap_err()
        .code,
        "files_version_conflict"
    );
    call(
        &mut s,
        "files.download.end",
        json!({"transfer_id":begin["transfer_id"]}),
    )
    .await;
    for idx in 0..4 {
        call(
            &mut s,
            "files.upload.begin",
            json!({"path":f.path.join(format!("new-{idx}")),"size":0,"sha256":sha(b"")}),
        )
        .await;
    }
    assert_eq!(
        s.execute(
            "files.upload.begin",
            json!({"path":f.path.join("fifth"),"size":0,"sha256":sha(b"")}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_transfer_limit"
    );
    drop(s);
    let mut s = f.session();
    f.file("second", b"");
    let page = call(&mut s, "files.list", json!({"path":f.path,"limit":1})).await;
    f.file("third", b"");
    assert_eq!(
        s.execute(
            "files.list",
            json!({"path":f.path,"limit":1,"cursor":page["next_cursor"]}),
            false
        )
        .await
        .unwrap_err()
        .code,
        "files_version_conflict"
    );
    s.set_deadline(Instant::now());
    assert_eq!(
        s.execute("files.list", json!({"path":f.path}), false)
            .await
            .unwrap_err()
            .code,
        "files_cancelled"
    );
}
#[tokio::test]
async fn upload_parent_replacement_and_directory_autocreation_are_bound_to_the_intended_parent() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.path.join("new/child/one");
    upload(&mut s, &p, b"one", None).await;
    assert_eq!(fs::read(&p).unwrap(), b"one");
    let target = f.path.join("new/child/two");
    let b = call(
        &mut s,
        "files.upload.begin",
        json!({"path":target,"size":3,"sha256":sha(b"two")}),
    )
    .await;
    call(
        &mut s,
        "files.upload.chunk",
        json!({"transfer_id":b["transfer_id"],"offset":0,"data":STANDARD.encode(b"two")}),
    )
    .await;
    fs::rename(f.path.join("new/child"), f.path.join("moved")).unwrap();
    fs::create_dir(f.path.join("new/child")).unwrap();
    fs::write(&target, b"unrelated").unwrap();
    assert_eq!(
        s.execute(
            "files.upload.commit",
            json!({"transfer_id":b["transfer_id"]}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "files_version_conflict"
    );
    assert_eq!(fs::read(target).unwrap(), b"unrelated");
    assert!(!f.path.join("moved/two").exists());
    assert!(fs::read_dir(f.path.join("moved")).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".datad-file-")
    }));
}
#[tokio::test]
async fn escaped_deep_directory_entries_paginate_by_serialized_bytes_without_false_complete() {
    let f = Fixture::new();
    let mut s = f.session();
    let mut p = f.path.clone();
    for _ in 0..3 {
        p = p.join("'\\\"".repeat(55));
        fs::create_dir(&p).unwrap();
    }
    for i in 0..220 {
        fs::write(p.join(format!("{i:03}-{}", "'\\\"".repeat(45))), b"").unwrap();
    }
    let mut cursor = None;
    let mut count = 0;
    let mut pages = 0;
    loop {
        let mut q = json!({"path":p,"limit":256});
        if let Some(v) = cursor {
            q["cursor"] = v;
        }
        let result = call(&mut s, "files.list", q).await;
        assert!(serde_json::to_vec(&result).unwrap().len() < 220 * 1024);
        count += result["entries"].as_array().unwrap().len();
        pages += 1;
        if result["complete"] == true {
            assert!(result["next_cursor"].is_null());
            break;
        }
        cursor = Some(result["next_cursor"].clone());
    }
    assert_eq!(count, 220);
    assert!(pages > 1);
}
#[test]
fn real_kernel_write_quota_failure_keeps_original_and_removes_partial_transfer() {
    const FLAG: &str = "DATAD_FILES_QUOTA_CHILD";
    if std::env::var_os(FLAG).is_none() {
        let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","files::tests::real_kernel_write_quota_failure_keeps_original_and_removes_partial_transfer","--nocapture"]).env(FLAG,"1").status().unwrap();
        assert!(status.success());
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {let f=Fixture::new();let p=f.file("original",b"baseline");let mut s=f.session();let bytes=vec![b'x';4096];let b=call(&mut s,"files.upload.begin",json!({"path":p,"size":bytes.len(),"sha256":sha(&bytes),"expected_version":f.revision(&p)})).await;let mut old=std::mem::MaybeUninit::<libc::rlimit>::uninit();assert_eq!(unsafe{libc::getrlimit(libc::RLIMIT_FSIZE,old.as_mut_ptr())},0);let old=unsafe{old.assume_init()};let limit=libc::rlimit{rlim_cur:1024,rlim_max:old.rlim_max};unsafe{libc::signal(libc::SIGXFSZ,libc::SIG_IGN);}assert_eq!(unsafe{libc::setrlimit(libc::RLIMIT_FSIZE,&limit)},0);let error=s.execute("files.upload.chunk",json!({"transfer_id":b["transfer_id"],"offset":0,"data":STANDARD.encode(bytes)}),true).await.unwrap_err();assert_eq!(unsafe{libc::setrlimit(libc::RLIMIT_FSIZE,&old)},0);assert_eq!(error.code,"files_no_space");assert_eq!(fs::read(p).unwrap(),b"baseline");assert_eq!(fs::read_dir(&f.path).unwrap().count(),1);});
}
#[tokio::test]
async fn zip_lzma_is_bounded_streaming_and_rejects_an_oversize_dictionary() {
    let f = Fixture::new();
    let mut s = f.session();
    let bytes=STANDARD.decode("UEsDBD8AAgAOACRNQV2GphA2GAAAAAUAAAAIAAAAZmlsZS50eHQJBAUAXQAAgAAANBlJ7o5oIf///7ngAABQSwECPwM/AAIADgAkTUFdhqYQNhgAAAAFAAAACAAAAAAAAAAAAAAAgAEAAAAAZmlsZS50eHRQSwUGAAAAAAEAAQA2AAAAPgAAAAAA").unwrap();
    let archive = f.file("lzma.zip", &bytes);
    let dest = f.path.join("out");
    fs::create_dir(&dest).unwrap();
    call(&mut s,"files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false})).await;
    assert_eq!(fs::read(dest.join("file.txt")).unwrap(), b"hello");
    let mut malicious = bytes;
    malicious[43..47].copy_from_slice(&u32::MAX.to_le_bytes());
    let archive = f.file("dict.zip", &malicious);
    let dest = f.path.join("bad");
    fs::create_dir(&dest).unwrap();
    assert_eq!(s.execute("files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false}),true).await.unwrap_err().code,"files_resource_limit");
    assert!(fs::read_dir(dest).unwrap().next().is_none());
}
#[tokio::test]
async fn compressed_tar_corrupt_or_missing_trailers_cannot_claim_verified_extraction() {
    let f = Fixture::new();
    let mut s = f.session();
    let tar = tar_bytes();
    for suffix in ["gz", "bz2", "xz"] {
        let bytes = match suffix {
            "gz" => {
                let mut z =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
            "bz2" => {
                let mut z = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
            _ => {
                let mut z = xz2::write::XzEncoder::new(Vec::new(), 3);
                z.write_all(&tar).unwrap();
                z.finish().unwrap()
            }
        };
        for truncated in [false, true] {
            let mut bad = bytes.clone();
            if truncated {
                bad.truncate(bad.len() - 3);
            } else {
                let n = bad.len();
                bad[n - 5] ^= 0x80;
            }
            let archive = f.file(&format!("bad-{truncated}.tar.{suffix}"), &bad);
            let dest = f.path.join(format!("out-{suffix}-{truncated}"));
            fs::create_dir(&dest).unwrap();
            assert!(s.execute("files.extract",json!({"path":archive,"destination":dest,"expected_version":f.revision(&archive),"overwrite":false}),true).await.is_err(),"{suffix}/{truncated}");
            assert!(fs::read_dir(dest).unwrap().next().is_none());
        }
    }
}
#[tokio::test]
async fn exchange_detects_an_external_write_in_the_last_check_publish_window_and_rolls_back() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("edit", b"old");
    let b = call(
        &mut s,
        "files.upload.begin",
        json!({"path":p,"size":3,"sha256":sha(b"new"),"expected_version":f.revision(&p)}),
    )
    .await;
    let id = b["transfer_id"].as_str().unwrap().to_owned();
    call(
        &mut s,
        "files.upload.chunk",
        json!({"transfer_id":id,"offset":0,"data":STANDARD.encode(b"new")}),
    )
    .await;
    let external = p.clone();
    s.uploads.get_mut(&id).unwrap().staged.before_publish = Some(Box::new(move || {
        fs::write(external, b"external-last-window").unwrap();
    }));
    assert_eq!(
        s.execute("files.upload.commit", json!({"transfer_id":id}), true)
            .await
            .unwrap_err()
            .code,
        "files_version_conflict"
    );
    assert_eq!(fs::read(p).unwrap(), b"external-last-window");
    assert_eq!(fs::read_dir(&f.path).unwrap().count(), 1);
}
#[tokio::test]
async fn an_edited_new_target_prevents_rollback_and_preserves_displaced_external_update_privately()
{
    let f = Fixture::new();
    let mut s = f.session();
    let p = f.file("edit", b"old");
    let b = call(
        &mut s,
        "files.upload.begin",
        json!({"path":p,"size":3,"sha256":sha(b"new"),"expected_version":f.revision(&p)}),
    )
    .await;
    let id = b["transfer_id"].as_str().unwrap().to_owned();
    call(
        &mut s,
        "files.upload.chunk",
        json!({"transfer_id":id,"offset":0,"data":STANDARD.encode(b"new")}),
    )
    .await;
    let before = p.clone();
    let after = p.clone();
    let staged = &mut s.uploads.get_mut(&id).unwrap().staged;
    staged.before_publish = Some(Box::new(move || {
        fs::write(before, b"displaced-external").unwrap();
    }));
    staged.after_exchange = Some(Box::new(move || {
        fs::write(after, b"another-current-edit").unwrap();
    }));
    assert_eq!(
        s.execute("files.upload.commit", json!({"transfer_id":id}), true)
            .await
            .unwrap_err()
            .code,
        "files_version_conflict"
    );
    assert_eq!(fs::read(&p).unwrap(), b"another-current-edit");
    drop(s);
    let recovery = fs::read_dir(&f.path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".datad-recovery-")
        })
        .unwrap();
    assert_eq!(fs::metadata(&recovery).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::read(recovery.join("original")).unwrap(),
        b"displaced-external"
    );
}
#[tokio::test]
async fn cancelling_recursive_remove_stops_before_deleting_the_remaining_items() {
    let f = Fixture::new();
    let mut s = f.session();
    let dir = f.path.join("many");
    fs::create_dir(&dir).unwrap();
    for i in 0..20 {
        fs::write(dir.join(i.to_string()), b"keep").unwrap();
    }
    let revision = f.revision(&dir);
    super::fsops::cancel_remove_after(1);
    let result = s
        .execute(
            "files.remove",
            json!({"path":dir,"recursive":true,"expected_version":revision}),
            true,
        )
        .await
        .unwrap_err();
    assert_eq!(result.code, "files_cancelled");
    assert!(dir.exists());
    assert_eq!(fs::read_dir(dir).unwrap().count(), 19);
}
#[tokio::test]
async fn long_gnu_names_and_hardlink_compression_preserve_a_round_trip() {
    let f = Fixture::new();
    let mut s = f.session();
    let dir = f.path.join("source");
    let nested = dir.join("d".repeat(150));
    fs::create_dir_all(&nested).unwrap();
    let file = nested.join("f".repeat(150));
    fs::write(&file, b"longname").unwrap();
    fs::hard_link(&file, dir.join("hard")).unwrap();
    let compressed = call(
        &mut s,
        "files.compress",
        json!({"path":dir,"expected_version":f.revision(&dir)}),
    )
    .await;
    let archive = PathBuf::from(compressed["entry"]["path"].as_str().unwrap());
    let out = f.path.join("out");
    fs::create_dir(&out).unwrap();
    call(&mut s,"files.extract",json!({"path":archive,"destination":out,"expected_version":f.revision(&archive),"overwrite":false})).await;
    let long = out
        .join("source")
        .join("d".repeat(150))
        .join("f".repeat(150));
    assert_eq!(fs::read(&long).unwrap(), b"longname");
    assert_eq!(
        fs::metadata(long).unwrap().ino(),
        fs::metadata(out.join("source/hard")).unwrap().ino()
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_zero_length_proc_file_with_bytes_is_not_reported_as_a_verified_empty_download() {
    let f = Fixture::new();
    let mut s = f.session();
    let p = "/proc/self/cmdline";
    let status = call(&mut s, "files.stat", json!({"path":p})).await;
    assert_eq!(status["entry"]["size"], 0);
    let error = s
        .execute(
            "files.download.begin",
            json!({"path":p,"expected_version":status["entry"]["version"]}),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "files_invalid_type");
    assert!(s.downloads.is_empty());
}
