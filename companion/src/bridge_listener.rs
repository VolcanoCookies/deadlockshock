use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

pub const BRIDGE_RECORD_PREFIX: &str = "[DEADLOCK_DEATH_HOOK]";
pub const BRIDGE_SCHEMA: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_INCOMPLETE_LINE_BYTES: usize = 256 * 1024;
const READ_BUFFER_BYTES: usize = 16 * 1024;
const TAIL_CHECKPOINT_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookReady {
    pub schema: u32,
    pub session_id: String,
    pub client_time_ms: u64,
    pub poll_interval_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalPlayerDeath {
    pub schema: u32,
    pub session_id: String,
    pub client_time_ms: u64,
    pub sequence: u64,
    pub detection: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BridgeEvent {
    HookReady(HookReady),
    LocalPlayerDeath(LocalPlayerDeath),
}

impl BridgeEvent {
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::HookReady(_) => "hook_ready",
            Self::LocalPlayerDeath(_) => "local_player_death",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "event")]
enum WireEvent {
    #[serde(rename = "hook_ready")]
    HookReady {
        schema: u32,
        session_id: String,
        client_time_ms: u64,
        poll_interval_ms: u64,
    },
    #[serde(rename = "local_player_death")]
    LocalPlayerDeath {
        schema: u32,
        session_id: String,
        client_time_ms: u64,
        sequence: u64,
        detection: String,
    },
}

pub fn parse_bridge_record(record: &str) -> Option<BridgeEvent> {
    let prefix_at = record.find(BRIDGE_RECORD_PREFIX)?;
    let payload = &record[prefix_at + BRIDGE_RECORD_PREFIX.len()..];
    let event: WireEvent = serde_json::from_str(payload).ok()?;
    match event {
        WireEvent::HookReady {
            schema,
            session_id,
            client_time_ms,
            poll_interval_ms,
        } if schema == BRIDGE_SCHEMA => Some(BridgeEvent::HookReady(HookReady {
            schema,
            session_id,
            client_time_ms,
            poll_interval_ms,
        })),
        WireEvent::LocalPlayerDeath {
            schema,
            session_id,
            client_time_ms,
            sequence,
            detection,
        } if schema == BRIDGE_SCHEMA => Some(BridgeEvent::LocalPlayerDeath(LocalPlayerDeath {
            schema,
            session_id,
            client_time_ms,
            sequence,
            detection,
        })),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListenerPhase {
    #[default]
    Stopped,
    WaitingForFile,
    Listening,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ListenerStatus {
    pub phase: ListenerPhase,
    pub configured_path: Option<PathBuf>,
    pub current_error: Option<String>,
    pub last_activity_at: Option<Instant>,
    pub last_event_at: Option<Instant>,
}

impl Default for ListenerStatus {
    fn default() -> Self {
        Self {
            phase: ListenerPhase::Stopped,
            configured_path: None,
            current_error: None,
            last_activity_at: None,
            last_event_at: None,
        }
    }
}

impl ListenerStatus {
    pub fn is_listening(&self) -> bool {
        self.phase == ListenerPhase::Listening
    }

    pub fn idle_for(&self) -> Option<Duration> {
        self.last_activity_at.map(|activity| activity.elapsed())
    }

    pub fn is_live(&self, maximum_idle: Duration) -> bool {
        match self.idle_for() {
            Some(idle) => self.is_listening() && idle <= maximum_idle,
            None => false,
        }
    }
}

struct Worker {
    stop: Sender<()>,
    join: JoinHandle<()>,
}

pub struct ConsoleLogListener {
    status: Arc<Mutex<ListenerStatus>>,
    subscribers: Arc<Mutex<Vec<Sender<BridgeEvent>>>>,
    worker: Option<Worker>,
}

impl Default for ConsoleLogListener {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleLogListener {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(ListenerStatus::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            worker: None,
        }
    }

    pub fn subscribe(&self) -> Receiver<BridgeEvent> {
        let (sender, receiver) = mpsc::channel();
        lock_unpoisoned(&self.subscribers).push(sender);
        receiver
    }

    pub fn start(&mut self, path: PathBuf) -> io::Result<()> {
        if self.worker.is_some()
            && lock_unpoisoned(&self.status).configured_path.as_ref() == Some(&path)
        {
            return Ok(());
        }

        self.stop();
        let (initial_log, skip_first_success) = match open_log_file(&path, true) {
            Ok(log) => (Some(log), false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (None, false),
            Err(_) => (None, true),
        };
        let initial_phase = if initial_log.is_some() {
            ListenerPhase::Listening
        } else {
            ListenerPhase::WaitingForFile
        };
        {
            let mut status = lock_unpoisoned(&self.status);
            status.phase = initial_phase;
            status.configured_path = Some(path.clone());
            status.current_error = None;
            status.last_activity_at = None;
            status.last_event_at = None;
        }

        let status = Arc::clone(&self.status);
        let subscribers = Arc::clone(&self.subscribers);
        let (stop_sender, stop_receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("deadlock-console-listener".to_owned())
            .spawn(move || {
                worker_loop(
                    path,
                    status,
                    subscribers,
                    stop_receiver,
                    initial_log,
                    skip_first_success,
                )
            }) {
            Ok(join) => {
                self.worker = Some(Worker {
                    stop: stop_sender,
                    join,
                });
                Ok(())
            }
            Err(error) => {
                let mut status = lock_unpoisoned(&self.status);
                status.phase = ListenerPhase::Failed;
                status.current_error = Some(format!("could not start listener thread: {error}"));
                Err(error)
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.stop.send(());
            let _ = worker.join.join();
        }
        let mut status = lock_unpoisoned(&self.status);
        status.phase = ListenerPhase::Stopped;
        status.current_error = None;
    }

    pub fn status(&self) -> ListenerStatus {
        lock_unpoisoned(&self.status).clone()
    }

    pub fn configured_path(&self) -> Option<PathBuf> {
        self.status().configured_path
    }

    pub fn current_error(&self) -> Option<String> {
        self.status().current_error
    }

    pub fn is_listening(&self) -> bool {
        self.status().is_listening()
    }

    pub fn idle_for(&self) -> Option<Duration> {
        self.status().idle_for()
    }

    pub fn is_live(&self, maximum_idle: Duration) -> bool {
        self.status().is_live(maximum_idle)
    }
}

impl Drop for ConsoleLogListener {
    fn drop(&mut self) {
        self.stop();
    }
}

struct OpenLog {
    file: File,
    identity: FileIdentity,
    offset: u64,
    checkpoint: Vec<u8>,
    incomplete_line: Vec<u8>,
    discarding_oversized_line: bool,
}

fn worker_loop(
    path: PathBuf,
    status: Arc<Mutex<ListenerStatus>>,
    subscribers: Arc<Mutex<Vec<Sender<BridgeEvent>>>>,
    stop: Receiver<()>,
    mut open_log: Option<OpenLog>,
    mut skip_first_success: bool,
) {
    let mut read_buffer = [0_u8; READ_BUFFER_BYTES];

    loop {
        if stop.try_recv().is_ok() {
            break;
        }

        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                skip_first_success = false;
                open_log = None;
                set_phase(&status, ListenerPhase::WaitingForFile, None);
            }
            Err(error) => {
                set_phase(
                    &status,
                    ListenerPhase::Failed,
                    Some(format!("could not inspect {}: {error}", path.display())),
                );
            }
            Ok(metadata) if !metadata.is_file() => {
                open_log = None;
                set_phase(
                    &status,
                    ListenerPhase::Failed,
                    Some(format!("{} is not a file", path.display())),
                );
            }
            Ok(path_metadata) => {
                let path_identity = FileIdentity::from_metadata(&path_metadata);
                let replaced = match open_log.as_ref() {
                    Some(log) => log.identity != path_identity,
                    None => false,
                };
                if replaced {
                    open_log = None;
                }

                if open_log.is_none() {
                    match open_log_file(&path, skip_first_success) {
                        Ok(log) => {
                            open_log = Some(log);
                            skip_first_success = false;
                            set_phase(&status, ListenerPhase::Listening, None);
                        }
                        Err(error) => {
                            set_phase(
                                &status,
                                ListenerPhase::Failed,
                                Some(format!("could not open {}: {error}", path.display())),
                            );
                        }
                    }
                }

                let truncation_error = open_log
                    .as_mut()
                    .and_then(|log| ensure_not_truncated(log, path_metadata.len()).err());
                if let Some(error) = truncation_error {
                    set_phase(
                        &status,
                        ListenerPhase::Failed,
                        Some(format!(
                            "could not check {} for truncation: {error}",
                            path.display()
                        )),
                    );
                } else if let Some(log) = &mut open_log {
                    match read_available(log, &mut read_buffer, &status, &subscribers) {
                        Ok(()) => set_phase(&status, ListenerPhase::Listening, None),
                        Err(error) => {
                            set_phase(
                                &status,
                                ListenerPhase::Failed,
                                Some(format!("could not read {}: {error}", path.display())),
                            );
                        }
                    }
                }
            }
        }

        match stop.recv_timeout(POLL_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn open_log_file(path: &Path, skip_existing: bool) -> io::Result<OpenLog> {
    let mut file = File::open(path)?;
    let identity = FileIdentity::from_metadata(&file.metadata()?);
    let offset = file.seek(if skip_existing {
        SeekFrom::End(0)
    } else {
        SeekFrom::Start(0)
    })?;
    let checkpoint = read_checkpoint(&mut file, offset)?;
    Ok(OpenLog {
        file,
        identity,
        offset,
        checkpoint,
        incomplete_line: Vec::new(),
        discarding_oversized_line: false,
    })
}

fn ensure_not_truncated(log: &mut OpenLog, path_len: u64) -> io::Result<()> {
    if path_len < log.offset || !checkpoint_matches(log)? {
        reset_after_truncation(log)?;
    }
    Ok(())
}

fn reset_after_truncation(log: &mut OpenLog) -> io::Result<()> {
    log.offset = log.file.seek(SeekFrom::Start(0))?;
    log.checkpoint.clear();
    log.incomplete_line.clear();
    log.discarding_oversized_line = false;
    Ok(())
}

fn read_checkpoint(file: &mut File, offset: u64) -> io::Result<Vec<u8>> {
    let checkpoint_len = offset.min(TAIL_CHECKPOINT_BYTES as u64) as usize;
    let mut checkpoint = vec![0; checkpoint_len];
    file.seek(SeekFrom::Start(offset - checkpoint_len as u64))?;
    file.read_exact(&mut checkpoint)?;
    file.seek(SeekFrom::Start(offset))?;
    Ok(checkpoint)
}

fn checkpoint_matches(log: &mut OpenLog) -> io::Result<bool> {
    if log.checkpoint.is_empty() {
        return Ok(true);
    }
    let checkpoint_start = log.offset - log.checkpoint.len() as u64;
    let mut current = [0_u8; TAIL_CHECKPOINT_BYTES];
    log.file.seek(SeekFrom::Start(checkpoint_start))?;
    log.file.read_exact(&mut current[..log.checkpoint.len()])?;
    log.file.seek(SeekFrom::Start(log.offset))?;
    Ok(current[..log.checkpoint.len()] == log.checkpoint)
}

fn update_checkpoint(checkpoint: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= TAIL_CHECKPOINT_BYTES {
        checkpoint.clear();
        checkpoint.extend_from_slice(&bytes[bytes.len() - TAIL_CHECKPOINT_BYTES..]);
        return;
    }
    let excess = checkpoint
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(TAIL_CHECKPOINT_BYTES);
    if excess > 0 {
        checkpoint.drain(..excess);
    }
    checkpoint.extend_from_slice(bytes);
}

fn read_available(
    log: &mut OpenLog,
    read_buffer: &mut [u8],
    status: &Arc<Mutex<ListenerStatus>>,
    subscribers: &Arc<Mutex<Vec<Sender<BridgeEvent>>>>,
) -> io::Result<()> {
    loop {
        let bytes_read = log.file.read(read_buffer)?;
        if bytes_read == 0 {
            return Ok(());
        }
        log.offset += bytes_read as u64;
        update_checkpoint(&mut log.checkpoint, &read_buffer[..bytes_read]);
        lock_unpoisoned(status).last_activity_at = Some(Instant::now());
        consume_bytes(
            &mut log.incomplete_line,
            &mut log.discarding_oversized_line,
            &read_buffer[..bytes_read],
            |event| {
                let mut listener_status = lock_unpoisoned(status);
                if broadcast(subscribers, event) {
                    listener_status.last_event_at = Some(Instant::now());
                }
            },
        );
    }
}

fn consume_bytes(
    incomplete_line: &mut Vec<u8>,
    discarding_oversized_line: &mut bool,
    bytes: &[u8],
    mut deliver: impl FnMut(BridgeEvent),
) {
    for byte in bytes {
        if *discarding_oversized_line {
            if *byte == b'\n' {
                *discarding_oversized_line = false;
            }
            continue;
        }

        if *byte == b'\n' {
            let line = incomplete_line
                .strip_suffix(b"\r")
                .unwrap_or(incomplete_line.as_slice());
            let event = std::str::from_utf8(line).ok().and_then(parse_bridge_record);
            if let Some(event) = event {
                deliver(event);
            }
            incomplete_line.clear();
        } else if incomplete_line.len() < MAX_INCOMPLETE_LINE_BYTES {
            incomplete_line.push(*byte);
        } else {
            incomplete_line.clear();
            *discarding_oversized_line = true;
        }
    }
}

fn broadcast(subscribers: &Arc<Mutex<Vec<Sender<BridgeEvent>>>>, event: BridgeEvent) -> bool {
    let mut delivered = false;
    lock_unpoisoned(subscribers).retain(|subscriber| {
        if subscriber.send(event.clone()).is_ok() {
            delivered = true;
            true
        } else {
            false
        }
    });
    delivered
}

fn set_phase(
    status: &Arc<Mutex<ListenerStatus>>,
    phase: ListenerPhase,
    current_error: Option<String>,
) {
    let mut status = lock_unpoisoned(status);
    status.phase = phase;
    status.current_error = current_error;
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    creation_time: u64,
}

#[cfg(windows)]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::windows::fs::MetadataExt;
        Self {
            creation_time: metadata.creation_time(),
        }
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    created: Option<std::time::SystemTime>,
}

#[cfg(not(any(unix, windows)))]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            created: metadata.created().ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::mpsc::TryRecvError;

    const READY_RECORD: &str = "[DEADLOCK_DEATH_HOOK]{\"schema\":1,\"event\":\"hook_ready\",\"session_id\":\"abc-1\",\"client_time_ms\":1700000000000,\"poll_interval_ms\":100}";
    const DEATH_RECORD: &str = "[DEADLOCK_DEATH_HOOK]{\"schema\":1,\"event\":\"local_player_death\",\"session_id\":\"abc-1\",\"client_time_ms\":1700000000123,\"sequence\":7,\"detection\":\"top_bar_local_player_dead_class\"}";

    #[test]
    fn parser_accepts_exact_mod_payloads_and_console_decoration() {
        assert_eq!(
            parse_bridge_record(READY_RECORD),
            Some(BridgeEvent::HookReady(HookReady {
                schema: 1,
                session_id: "abc-1".to_owned(),
                client_time_ms: 1_700_000_000_000,
                poll_interval_ms: 100,
            }))
        );
        assert_eq!(
            parse_bridge_record(&format!("[Console] {DEATH_RECORD}")),
            Some(BridgeEvent::LocalPlayerDeath(LocalPlayerDeath {
                schema: 1,
                session_id: "abc-1".to_owned(),
                client_time_ms: 1_700_000_000_123,
                sequence: 7,
                detection: "top_bar_local_player_dead_class".to_owned(),
            }))
        );
    }

    #[test]
    fn parser_ignores_unrelated_unknown_and_malformed_records() {
        assert_eq!(parse_bridge_record("ordinary console output"), None);
        assert_eq!(
            parse_bridge_record("[DEADLOCK_DEATH_HOOK]{\"schema\":2,\"event\":\"hook_ready\"}"),
            None
        );
        assert_eq!(
            parse_bridge_record("[DEADLOCK_DEATH_HOOK]{\"schema\":1,\"event\":\"future_event\"}"),
            None
        );
        assert_eq!(parse_bridge_record("[DEADLOCK_DEATH_HOOK]{bad"), None);
    }

    #[test]
    fn existing_history_is_skipped_and_activity_requires_new_bytes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("console.log");
        fs::write(&path, format!("{READY_RECORD}\n")).expect("write history");
        let mut listener = ConsoleLogListener::new();
        let events = listener.subscribe();
        listener.start(path.clone()).expect("start listener");
        wait_for_phase(&listener, ListenerPhase::Listening);

        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(listener.status().last_activity_at, None);
        append(&path, "unrelated output\n");
        wait_until(|| listener.status().last_activity_at.is_some());
        assert_eq!(listener.status().last_event_at, None);

        append(&path, &format!("{DEATH_RECORD}\n"));
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .expect("death event"),
            parse_bridge_record(DEATH_RECORD).expect("parse expected death")
        );
        assert!(listener.status().last_event_at.is_some());
    }

    #[test]
    fn partial_lines_wait_for_newline_and_broadcast_to_every_subscriber() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("console.log");
        File::create(&path).expect("create log");
        let mut listener = ConsoleLogListener::new();
        let first = listener.subscribe();
        let second = listener.subscribe();
        listener.start(path.clone()).expect("start listener");
        wait_for_phase(&listener, ListenerPhase::Listening);

        append(&path, DEATH_RECORD);
        wait_until(|| listener.status().last_activity_at.is_some());
        assert_eq!(first.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(second.try_recv(), Err(TryRecvError::Empty));
        append(&path, "\n");

        let death = parse_bridge_record(DEATH_RECORD).expect("parse expected death");
        assert_eq!(
            first
                .recv_timeout(Duration::from_secs(2))
                .expect("first death"),
            death
        );
        assert_eq!(
            second
                .recv_timeout(Duration::from_secs(2))
                .expect("second death"),
            death
        );

        let late = listener.subscribe();
        assert_eq!(late.try_recv(), Err(TryRecvError::Empty));
        append(&path, &format!("{READY_RECORD}\n"));
        let ready = parse_bridge_record(READY_RECORD).expect("parse expected ready");
        assert_eq!(
            first
                .recv_timeout(Duration::from_secs(2))
                .expect("first ready"),
            ready
        );
        assert_eq!(
            second
                .recv_timeout(Duration::from_secs(2))
                .expect("second ready"),
            ready
        );
        assert_eq!(
            late.recv_timeout(Duration::from_secs(2))
                .expect("late ready"),
            ready
        );
    }

    #[test]
    fn oversized_records_are_discarded_without_losing_the_next_record() {
        let mut incomplete = Vec::new();
        let mut discarding = false;
        let mut bytes = vec![b'x'; MAX_INCOMPLETE_LINE_BYTES + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(READY_RECORD.as_bytes());
        bytes.push(b'\n');
        let mut events = Vec::new();

        consume_bytes(&mut incomplete, &mut discarding, &bytes, |event| {
            events.push(event)
        });

        assert!(incomplete.len() <= MAX_INCOMPLETE_LINE_BYTES);
        assert!(!discarding);
        assert_eq!(
            events,
            vec![parse_bridge_record(READY_RECORD).expect("parse expected ready")]
        );
    }

    #[test]
    fn missing_file_is_consumed_from_zero_when_created() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("console.log");
        let mut listener = ConsoleLogListener::new();
        let events = listener.subscribe();
        listener.start(path.clone()).expect("start listener");
        wait_for_phase(&listener, ListenerPhase::WaitingForFile);

        fs::write(&path, format!("{READY_RECORD}\n")).expect("create log");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .expect("ready event"),
            parse_bridge_record(READY_RECORD).expect("parse expected ready")
        );
    }

    #[test]
    fn truncation_and_replacement_are_consumed_from_zero() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("console.log");
        File::create(&path).expect("create log");
        let mut listener = ConsoleLogListener::new();
        let events = listener.subscribe();
        listener.start(path.clone()).expect("start listener");
        wait_for_phase(&listener, ListenerPhase::Listening);

        append(&path, "padding that moves the listener offset\n");
        wait_until(|| listener.status().last_activity_at.is_some());
        fs::write(&path, format!("{READY_RECORD}\n")).expect("truncate and rewrite log");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .expect("event after truncation"),
            parse_bridge_record(READY_RECORD).expect("parse expected ready")
        );

        let replacement = temp.path().join("replacement.log");
        fs::write(&replacement, format!("{DEATH_RECORD}\n")).expect("write replacement");
        fs::remove_file(&path).expect("remove old log");
        fs::rename(&replacement, &path).expect("install replacement");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .expect("event after replacement"),
            parse_bridge_record(DEATH_RECORD).expect("parse expected death")
        );
    }

    #[test]
    fn changing_paths_joins_the_old_worker_and_tails_the_new_path() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let first_path = temp.path().join("first.log");
        let second_path = temp.path().join("second.log");
        File::create(&first_path).expect("create first log");
        fs::write(&second_path, format!("{READY_RECORD}\n")).expect("write second history");
        let mut listener = ConsoleLogListener::new();
        let events = listener.subscribe();
        listener
            .start(first_path.clone())
            .expect("start first listener");
        wait_for_phase(&listener, ListenerPhase::Listening);

        listener
            .start(second_path.clone())
            .expect("restart on second path");
        wait_for_phase(&listener, ListenerPhase::Listening);
        assert_eq!(
            listener.configured_path().as_deref(),
            Some(second_path.as_path())
        );
        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        append(&first_path, &format!("{DEATH_RECORD}\n"));
        thread::sleep(POLL_INTERVAL * 2);
        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));

        append(&second_path, &format!("{DEATH_RECORD}\n"));
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .expect("new-path event"),
            parse_bridge_record(DEATH_RECORD).expect("parse expected death")
        );
    }

    #[test]
    fn same_path_is_a_noop_and_stop_joins_the_worker() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("console.log");
        File::create(&path).expect("create log");
        let mut listener = ConsoleLogListener::new();
        let events = listener.subscribe();
        listener.start(path.clone()).expect("start listener");
        wait_for_phase(&listener, ListenerPhase::Listening);
        listener.start(path.clone()).expect("same-path no-op");
        listener.stop();

        assert_eq!(listener.status().phase, ListenerPhase::Stopped);
        append(&path, &format!("{READY_RECORD}\n"));
        thread::sleep(POLL_INTERVAL * 2);
        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        fs::remove_file(path).expect("joined worker released the log file");
    }

    fn append(path: &Path, contents: &str) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open log for append");
        file.write_all(contents.as_bytes()).expect("append log");
        file.flush().expect("flush log");
    }

    fn wait_for_phase(listener: &ConsoleLogListener, phase: ListenerPhase) {
        wait_until(|| listener.status().phase == phase);
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
