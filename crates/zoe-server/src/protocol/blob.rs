//! Blob streaming: the upload of a blob in chunks, and its download.
//!
//! A client without a shared blob directory sends a blob with
//! `storeBlobStart`, one `storeBlobChunk` per chunk, and `storeBlobEnd`. The
//! server writes the chunks into one blob file of the core
//! (`Store::new_blob_file`), through the `Vfs` of the store. `storeBlobEnd`
//! stages the file with `Txn::store_blob`. The session holds the file, so
//! this module only sends blobs.
//!
//! A client gets a blob with `sendBlob`. The server sends
//! `receiveBlobStart`, one `receiveBlobChunk` per chunk of 64 KiB and
//! `receiveBlobStop`, then the reply. The client needs the file before the
//! reply, so this order is part of the protocol.

use std::io::{self, Read};

use rmpv::Value;
use zoe_core::{BlobReader, Oid, Tid, ZoeError};
use zoe_wire::p64;

use super::message;

/// The size of a chunk that `sendBlob` sends. The stock client takes a
/// chunk of any size.
pub(crate) const CHUNK: usize = 64 * 1024;

/// A blob that `sendBlob` sends to the client.
pub(crate) struct Download {
    oid: Oid,
    tid: Tid,
    reader: BlobReader,
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("oid", &self.oid)
            .field("tid", &self.tid)
            .finish_non_exhaustive()
    }
}

impl Download {
    /// Send the blob that `reader` reads, the blob of `oid` at `tid`.
    pub(crate) fn new(oid: Oid, tid: Tid, reader: BlobReader) -> Self {
        Self { oid, tid, reader }
    }

    /// Send the blob with `send`: `receiveBlobStart`, one
    /// `receiveBlobChunk` per chunk of `CHUNK` bytes, then
    /// `receiveBlobStop`. `send` returns false when the client is gone.
    /// Then return `Ok(false)`.
    ///
    /// The reader checks the size and the hash of the blob at the end of
    /// the file. A read that fails returns its error, and
    /// `receiveBlobStop` does not go out. Then the stock client does not
    /// put the bytes in its blob cache. But it keeps a partial `<blob>.dl`
    /// file, and its next `receiveBlobStart` for that blob fails an
    /// assertion. So `sendBlob` checks the whole blob before it sends the
    /// first chunk.
    pub(crate) fn send(mut self, mut send: impl FnMut(Vec<u8>) -> bool) -> Result<bool, ZoeError> {
        let (oid, tid) = (self.oid, self.tid);
        let ids = || vec![p64(oid.0), p64(tid.0)];
        if !send(message::notify("receiveBlobStart", ids())) {
            return Ok(false);
        }
        let mut chunk = vec![0; CHUNK];
        loop {
            let n =
                fill(&mut self.reader, &mut chunk).map_err(|error| read_error(oid, tid, error))?;
            if n > 0 {
                let mut args = ids();
                args.push(Value::Binary(chunk[..n].to_vec()));
                if !send(message::notify("receiveBlobChunk", args)) {
                    return Ok(false);
                }
            }
            // A short chunk is the last one. The reader reached the end of
            // the file and checked the blob.
            if n < CHUNK {
                break;
            }
        }
        Ok(send(message::notify("receiveBlobStop", ids())))
    }
}

/// Read from `reader` until `buf` is full or the reader is at its end.
/// Return the number of bytes read.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Return the error of a failed read of the blob of `oid` at `tid`. A
/// reader fails with `InvalidData` when the file does not match the blob
/// record.
fn read_error(oid: Oid, tid: Tid, error: io::Error) -> ZoeError {
    if error.kind() == io::ErrorKind::InvalidData {
        ZoeError::CorruptBlob { oid, tid }
    } else {
        error.into()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use zoe_core::Store;

    use super::*;
    use crate::protocol::testing::{Client, TIMEOUT, bytes, decode, fresh_dir};
    use crate::serve::{ServeOptions, Server, StorageOptions};

    /// Commit a blob that holds `bytes` for `oid`. Return its tid.
    fn commit_blob(store: &Store, oid: Oid, bytes: &[u8]) -> Tid {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, b"pickle", file).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    /// Check that `frames` are the calls of the download of `bytes`, the
    /// blob of `oid` at `tid`: `receiveBlobStart`, the chunks and
    /// `receiveBlobStop`. Return the length of each chunk.
    fn check_download(frames: &[Vec<u8>], oid: Oid, tid: Tid, bytes_sent: &[u8]) -> Vec<usize> {
        let ids = [p64(oid.0), p64(tid.0)];
        let mut got = Vec::new();
        let mut lengths = Vec::new();
        let last = frames.len() - 1;
        for (n, frame) in frames.iter().enumerate() {
            let (id, flags, method, args) = decode(frame);
            assert_eq!((id, flags), (Value::from(0), Value::Boolean(true)));
            let args = args.as_array().unwrap();
            assert_eq!(args[..2], ids[..], "frame {n}");
            match n {
                0 => assert_eq!((method.as_str(), args.len()), ("receiveBlobStart", 2)),
                n if n == last => assert_eq!((method.as_str(), args.len()), ("receiveBlobStop", 2)),
                _ => {
                    assert_eq!((method.as_str(), args.len()), ("receiveBlobChunk", 3));
                    let chunk = bytes(&args[2]);
                    lengths.push(chunk.len());
                    got.extend_from_slice(chunk);
                }
            }
        }
        assert!(got == bytes_sent, "the chunks hold the blob");
        lengths
    }

    #[test]
    fn a_download_sends_chunks_of_64_kib_and_a_short_last_chunk() {
        let dir = fresh_dir("chunks");
        let store = Store::open(&dir).unwrap();
        let cases: [(usize, &[usize]); 5] = [
            (0, &[]),
            (1, &[1]),
            (CHUNK, &[CHUNK]),
            (2 * CHUNK, &[CHUNK, CHUNK]),
            (3 * CHUNK + 5, &[CHUNK, CHUNK, CHUNK, 5]),
        ];
        for (n, (size, lengths)) in cases.into_iter().enumerate() {
            let oid = Oid(n as u64 + 1);
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let tid = commit_blob(&store, oid, &data);
            let mut frames = Vec::new();
            let download = Download::new(oid, tid, store.open_blob(oid, tid).unwrap());
            let sent = download.send(|frame| {
                frames.push(frame);
                true
            });
            assert!(sent.unwrap(), "size {size}");
            assert_eq!(
                check_download(&frames, oid, tid, &data),
                lengths,
                "size {size}"
            );
        }
    }

    #[test]
    fn a_download_stops_when_the_client_is_gone() {
        let dir = fresh_dir("gone");
        let store = Store::open(&dir).unwrap();
        let tid = commit_blob(&store, Oid(1), &vec![7; 3 * CHUNK]);
        let mut frames = 0;
        let download = Download::new(Oid(1), tid, store.open_blob(Oid(1), tid).unwrap());
        let sent = download.send(|_| {
            frames += 1;
            frames < 2
        });
        assert!(!sent.unwrap());
        assert_eq!(frames, 2, "no frame after the client is gone");
    }

    /// Make the blob file at `path` writable and change its byte at
    /// `offset`.
    fn damage(path: &Path, offset: usize) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut bytes = fs::read(path).unwrap();
        bytes[offset] ^= 0x01;
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_blob_that_does_not_match_its_record_never_gets_receive_blob_stop() {
        let dir = fresh_dir("corrupt");
        let store = Store::open(&dir).unwrap();
        let data = vec![9; 2 * CHUNK + 10];
        let flipped = commit_blob(&store, Oid(1), &data);
        damage(&store.blob_path(Oid(1), flipped).unwrap(), 2 * CHUNK + 3);
        let short = commit_blob(&store, Oid(2), &data);
        let path = store.blob_path(Oid(2), short).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, &data[..CHUNK + 1]).unwrap();

        for (oid, tid) in [(Oid(1), flipped), (Oid(2), short)] {
            let mut methods = Vec::new();
            let download = Download::new(oid, tid, store.open_blob(oid, tid).unwrap());
            let error = download
                .send(|frame| {
                    methods.push(decode(&frame).2);
                    true
                })
                .unwrap_err();
            assert!(
                matches!(error, ZoeError::CorruptBlob { oid: o, tid: t } if o == oid && t == tid),
                "{error}"
            );
            assert!(
                !methods.iter().any(|m| m == "receiveBlobStop"),
                "{methods:?}"
            );
        }
    }

    /// Return the names of the files in `dir`.
    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Return the temporary blob files in the blob temporary directory of
    /// the store in `dir`: the upload files.
    fn uploads(dir: &Path) -> Vec<String> {
        let mut names = files(&dir.join("blobs/tmp"));
        names.retain(|name| name.ends_with(".tmp"));
        names
    }

    #[test]
    fn a_blob_goes_up_and_comes_down_over_the_wire() {
        let dir = fresh_dir("wire");
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let mut client = Client::connect(server.addr());
        let data: Vec<u8> = (0..3 * CHUNK + 12_345).map(|i| (i % 253) as u8).collect();
        let tid = client.commit_blob(Oid(1), &data);
        assert_eq!(uploads(&dir), Vec::<String>::new());

        let id = client.send("sendBlob", vec![p64(1), p64(tid.0)]);
        let mut frames = Vec::new();
        loop {
            let frame = client.read_frame();
            let (got, flags, method, args) = decode(&frame);
            if method == ".reply" {
                // The reply nil comes after the chunks.
                assert_eq!((got, flags, args), (id, Value::from(0), Value::Nil));
                break;
            }
            frames.push(frame);
        }
        let lengths = check_download(&frames, Oid(1), tid, &data);
        assert_eq!(lengths, [CHUNK, CHUNK, CHUNK, 12_345]);
        server.stop();
    }

    #[test]
    fn a_storage_keeps_its_blobs_in_its_blob_directory() {
        let dir = fresh_dir("blob-dir");
        let (path, blobs) = (dir.join("store"), dir.join("blobs"));
        let storage = StorageOptions::new("1", &path).blob_dir(&blobs);
        let server = Server::start(ServeOptions::with_storages([storage])).unwrap();
        let mut client = Client::connect(server.addr());
        let tid = client.commit_blob(Oid(1), b"the bytes of the blob");
        server.stop();
        let options = zoe_core::Options {
            blob_dir: Some(blobs.clone()),
            ..zoe_core::Options::default()
        };
        let store = Store::open_with(&path, options).unwrap();
        let mut blob = String::new();
        store
            .open_blob(Oid(1), tid)
            .unwrap()
            .read_to_string(&mut blob)
            .unwrap();
        assert_eq!(blob, "the bytes of the blob");
        assert!(blobs.is_dir());
        assert!(!path.join("blobs").exists());
    }

    #[test]
    fn a_read_only_storage_serves_the_blobs_of_its_blob_directory() {
        let dir = fresh_dir("read-only-blob-dir");
        let (path, blobs) = (dir.join("store"), dir.join("blobs-elsewhere"));
        let options = zoe_core::Options {
            blob_dir: Some(blobs.clone()),
            ..zoe_core::Options::default()
        };
        let data = b"the bytes of the blob";
        let tid = commit_blob(&Store::open_with(&path, options).unwrap(), Oid(1), data);

        // Without its blob directory, the store does not start. The error
        // names the directory that has no layout file.
        let wrong = StorageOptions::new("1", &path).read_only(true);
        let error = Server::start(ServeOptions::with_storages([wrong]))
            .err()
            .expect("a wrong blob directory fails the start");
        let named = path.join("blobs").display().to_string();
        assert!(error.to_string().contains(&named), "{error}");

        let storage = StorageOptions::new("1", &path)
            .blob_dir(&blobs)
            .read_only(true);
        let server = Server::start(ServeOptions::with_storages([storage])).unwrap();
        let mut client = Client::connect_read_only(server.addr());
        let id = client.send("sendBlob", vec![p64(1), p64(tid.0)]);
        let mut frames = Vec::new();
        loop {
            let frame = client.read_frame();
            let (got, flags, method, args) = decode(&frame);
            if method == ".reply" {
                assert_eq!((got, flags, args), (id, Value::from(0), Value::Nil));
                break;
            }
            frames.push(frame);
        }
        assert_eq!(check_download(&frames, Oid(1), tid, data), [data.len()]);
        server.stop();
        assert!(!path.join("blobs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disconnect_during_an_upload_leaves_no_file() {
        let dir = fresh_dir("disconnect");
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let mut client = Client::connect(server.addr());
        let txn = Value::from(1);
        client.begin(&txn);
        let pickle = Value::Binary(b"pickle".to_vec());
        client.send_async("storea", vec![p64(1), p64(0), pickle, txn]);
        client.send_async("storeBlobStart", vec![]);
        client.send_async("storeBlobChunk", vec![Value::Binary(vec![1; 59_000])]);
        // The calls of a transaction run in order. So after the reply to an
        // abort of another transaction, the upload file holds the chunk.
        client.call("tpc_abort", vec![Value::from(99)]);
        let names = uploads(&dir);
        assert_eq!(names.len(), 1, "{names:?}");
        let file = dir.join("blobs/tmp").join(&names[0]);
        assert_eq!(fs::metadata(file).unwrap().len(), 59_000);

        drop(client);
        wait_for_no_upload(&dir);
        server.stop();
        assert_eq!(files(&dir.join("blobs/tmp")), Vec::<String>::new());
    }

    /// Wait until the blob temporary directory of the store in `dir` has
    /// no upload file.
    fn wait_for_no_upload(dir: &Path) {
        let start = Instant::now();
        while !uploads(dir).is_empty() {
            assert!(start.elapsed() < TIMEOUT, "the upload file stays");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_error_of_a_blob_call_without_a_reply_closes_the_connection() {
        let dir = fresh_dir("wire-close");
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let txn = Value::from(1);
        let chunk = vec![Value::Binary(vec![1; 10])];

        // storeBlobStart without a transaction.
        let mut client = Client::connect(server.addr());
        client.send_async("storeBlobStart", vec![]);
        assert!(client.is_closed(), "storeBlobStart");

        // storeBlobChunk without storeBlobStart.
        let mut client = Client::connect(server.addr());
        client.begin(&txn);
        client.send_async("storeBlobChunk", chunk.clone());
        assert!(client.is_closed(), "storeBlobChunk");

        // storeBlobEnd of another transaction, during an upload. The end of
        // the session removes the upload file.
        let mut client = Client::connect(server.addr());
        client.begin(&txn);
        client.send_async("storeBlobStart", vec![]);
        client.send_async("storeBlobChunk", chunk);
        let pickle = Value::Binary(b"pickle".to_vec());
        let end = vec![p64(1), p64(0), pickle, Value::from(2)];
        client.send_async("storeBlobEnd", end);
        assert!(client.is_closed(), "storeBlobEnd");
        wait_for_no_upload(&dir);

        // The server stays up.
        Client::connect(server.addr()).call("ping", vec![]);
        server.stop();
    }

    #[test]
    fn a_slow_download_does_not_stop_a_commit_and_gets_its_invalidation() {
        let dir = fresh_dir("slow-download");
        // The blob is far larger than the buffers of the sockets and the
        // queues of the connection. So the session of the reader waits for
        // room in its queue while the reader does not read.
        let data: Vec<u8> = (0..256 * CHUNK).map(|i| (i % 241) as u8).collect();
        let tid = commit_blob(&Store::open(&dir).unwrap(), Oid(1), &data);
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let mut reader = Client::connect_slow(server.addr());
        let mut writer = Client::connect(server.addr());

        let id = reader.send("sendBlob", vec![p64(1), p64(tid.0)]);
        let (_, _, method, _) = decode(&reader.read_frame());
        assert_eq!(method, "receiveBlobStart");
        // The reader reads nothing more until the commit ends. A commit
        // that waits for the reader never ends, and the read of the writer
        // times out.
        let committed = writer.commit_blob(Oid(2), b"a small blob");

        let mut got = Vec::new();
        let mut invalidations = Vec::new();
        let mut chunks_before_the_invalidation = None;
        let mut stopped = false;
        loop {
            let (got_id, flags, method, args) = decode(&reader.read_frame());
            match method.as_str() {
                "receiveBlobChunk" => {
                    assert!(!stopped, "a chunk after receiveBlobStop");
                    got.extend_from_slice(bytes(&args.as_array().unwrap()[2]));
                }
                "receiveBlobStop" => stopped = true,
                "invalidateTransaction" => {
                    chunks_before_the_invalidation.get_or_insert(got.len() / CHUNK);
                    invalidations.push(args);
                }
                ".reply" => {
                    assert_eq!((got_id, flags, args), (id, Value::from(0), Value::Nil));
                    break;
                }
                other => panic!("an unexpected message: {other}"),
            }
        }
        assert!(stopped);
        assert!(got == data, "the chunks hold the blob");
        // The invalidation of the commit with a blob names its oid. It came
        // during the download, between two chunks.
        let invalidation = Value::Array(vec![p64(committed.0), Value::Array(vec![p64(2)])]);
        assert_eq!(invalidations, [invalidation]);
        let before = chunks_before_the_invalidation.unwrap();
        assert!(before < 256, "the invalidation came after the last chunk");
        // The connection of the reader stays open.
        reader.call("ping", vec![]);
        server.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Stop a server during a download that the client does not read, in
    /// a directory named after `name`. Check that the server stops within
    /// its shutdown timeout and that the store then opens read-only.
    fn stop_during_a_download(name: &str) {
        let dir = fresh_dir(name);
        let tid = commit_blob(&Store::open(&dir).unwrap(), Oid(1), &vec![3; 256 * CHUNK]);
        let timeout = Duration::from_secs(2);
        let server = Server::start(ServeOptions::new(&dir).shutdown_timeout(timeout)).unwrap();
        let stop_times = server.stop_times();
        let mut reader = Client::connect_slow(server.addr());
        reader.send("sendBlob", vec![p64(1), p64(tid.0)]);
        let (_, _, method, _) = decode(&reader.read_frame());
        assert_eq!(method, "receiveBlobStart");
        // The session waits for room in its queue. The stop waits for the
        // client until the forced close, which ends the session.
        let (stopped, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            server.stop();
            let _ = stopped.send(());
        });
        assert!(
            wait.recv_timeout(TIMEOUT).is_ok(),
            "the server did not stop"
        );
        // The shutdown timeout bounds the time from the start of the stop
        // to the end of its wait for the sessions. That time does not
        // include the spawn of the thread above or the close of the stores.
        let times = stop_times.get().expect("the stop sets its times");
        let waited = times.waited - times.start;
        assert!(waited <= timeout, "{waited:?}");
        // The session ended before the stop closed the store.
        Store::open_read_only(&dir).unwrap();
        drop(reader);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_server_stops_during_a_download_that_the_client_does_not_read() {
        stop_during_a_download("stop-download");
    }

    /// The scenario of the test above, with its checks, runs five times
    /// in a row on a loaded machine: one busy thread for each core runs
    /// during the whole test. Each run has a directory of its own.
    #[test]
    fn the_stop_during_a_download_keeps_its_checks_on_a_loaded_machine() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let busy = Arc::new(AtomicBool::new(true));
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads: Vec<_> = (0..cores)
            .map(|_| {
                let busy = Arc::clone(&busy);
                std::thread::spawn(move || {
                    let mut n: u64 = 0;
                    while busy.load(Ordering::Relaxed) {
                        n = std::hint::black_box(n.wrapping_mul(31).wrapping_add(7));
                    }
                })
            })
            .collect();
        let result = std::panic::catch_unwind(|| {
            for round in 0..5 {
                stop_during_a_download(&format!("stop-download-loaded-{round}"));
            }
        });
        busy.store(false, Ordering::Relaxed);
        for thread in threads {
            thread.join().unwrap();
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Return the path of each blob file under `dir`.
    fn blob_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(blob_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "blob") {
                found.push(path);
            }
        }
        found
    }

    #[test]
    fn a_corrupt_blob_goes_out_as_an_error_reply_and_no_chunk() {
        let dir = fresh_dir("wire-corrupt");
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let mut client = Client::connect(server.addr());
        let tid = client.commit_blob(Oid(1), &vec![5; CHUNK + 1]);
        let [path] = &blob_files(&dir.join("blobs"))[..] else {
            panic!("one blob file");
        };
        damage(path, 0);

        let id = client.send("sendBlob", vec![p64(1), p64(tid.0)]);
        // The first frame is the error reply: no receiveBlobStart before it.
        let (got, flags, method, args) = decode(&client.read_frame());
        assert_eq!(
            (got, flags, method.as_str()),
            (id, Value::from(2), ".reply")
        );
        let args = args.as_array().unwrap();
        let class = args[0].as_str();
        assert_eq!(class, Some("ZODB.POSException.StorageSystemError"));
        let text = args[1].as_array().unwrap()[0].as_str().unwrap();
        assert!(text.starts_with("corrupt blob"), "{text}");

        // A blob that the store does not have is a POSKeyError.
        let id = client.send("sendBlob", vec![p64(2), p64(tid.0)]);
        let (got, flags, _, args) = decode(&client.read_frame());
        assert_eq!((got, flags), (id, Value::from(2)));
        let error = Value::Array(vec![
            Value::from("ZODB.POSException.POSKeyError"),
            Value::Array(vec![p64(2)]),
        ]);
        assert_eq!(args, error);
        // The connection stays open.
        client.call("ping", vec![]);
        server.stop();
    }
}
