use std::io;
use std::path::Path;
use std::sync::Mutex;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::keys::ESC_DOUBLE_TAP_WINDOW;
use super::lifecycle::mark_command_started;
use super::lifecycle::update_child_input_detection;
use super::queue_actions::{clear_queue, restore_removed_commands};
use super::queue_sync::{QUEUE_SYNC_INTERVAL, RestoredQueue, sync_queue_from_disk_if_due};
use super::shell_syntax::is_likely_complete_shell_command;
use super::status::{RESUME_CWD_CONFIRM_WINDOW, STATUS_TTL, truncate_for_status};
use super::*;

const ETX: u8 = 0x03;
const FS: u8 = 0x1c;

/// Capture-into-Vec writer for unit tests of dispatch logic.
pub struct VecWriter(pub Arc<Mutex<Vec<u8>>>);
impl Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingWriter;
impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "child closed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type Shell = Box<dyn Write + Send>;

fn panel_delay() -> Duration {
    Duration::from_millis(Config::default().panel.delay_ms)
}

fn handle_key(key: KeyEvent, state: &mut AppState, shell: &mut Shell) -> KeyOutcome {
    keys::handle_key(key, state, &mut ShellInput::detached(shell.as_mut()), None)
}

fn handle_paste(text: String, state: &mut AppState, shell: &mut Shell) {
    keys::handle_paste(text, state, &mut ShellInput::detached(shell.as_mut()));
}

fn handle_command_end(state: &mut AppState, exit_code: Option<i32>, shell: &mut Shell) -> bool {
    lifecycle::handle_command_end(state, exit_code, &mut ShellInput::detached(shell.as_mut()))
}

fn dispatch_next_eligible(state: &mut AppState, prev_exit: Option<i32>, shell: &mut Shell) -> bool {
    lifecycle::dispatch_next_eligible(state, prev_exit, &mut ShellInput::detached(shell.as_mut()))
}

fn toggle_queue_pause(state: &mut AppState, shell: &mut Shell) {
    queue_actions::toggle_queue_pause(state, &mut ShellInput::detached(shell.as_mut()));
}

fn test_queue_path() -> PathBuf {
    static DIR: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().unwrap());
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    DIR.path().join(n.to_string()).join("queue.json")
}

fn make_state() -> AppState {
    AppState {
        config: Config::default(),
        shell_kind: crate::shell_integration::ShellKind::Bash,
        queue_supported: true,
        wait_for_bash_prompt: false,
        pending_bash_exit: None,
        queue: Queue::new(),
        editor: LineEditor::new(),
        shell_state: ShellState::Running,
        manual_passthrough: false,
        auto_passthrough: false,
        child: ChildModes::default(),
        force_queue: false,
        show_help: false,
        terminal_allows_panel: true,
        last_sigint_at: None,
        command_started_at: None,
        command_tty_check_after: None,
        prompt: PromptMirror::default(),
        running_output_tail: OutputTail::default(),
        child_input_active: false,
        pending_quit_at: None,
        pending_clear_at: None,
        last_exit_code: None,
        last_esc_at: None,
        activity: None,
        recovery: Vec::new(),
        status: String::new(),
        status_kind: StatusKind::Notice,
        status_set_at: None,
        queue_dirty: false,
        queue_save_error: None,
        queue_path: test_queue_path(),
        queue_known_items: Vec::new(),
        queue_known_paused: false,
        queue_external_change_pending: false,
        restored_queue_paused_locally: false,
        session_cwd: Some(PathBuf::from("/tmp/cmdq-test")),
        shell_cwd: Some(PathBuf::from("/tmp/cmdq-test")),
        resume_cwd_warning: None,
        resume_cwd_confirmation_started_at: None,
    }
}

#[test]
fn mouse_capture_keeps_running_app_in_passthrough() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay());
    assert!(s.panel_should_be_visible());
    assert!(s.editor_owns_input());

    s.child.mouse_capture = true;
    s.auto_passthrough = true;

    assert!(!s.panel_should_be_visible());
    assert!(!s.editor_owns_input());
}

#[test]
fn child_input_prompt_keeps_plain_keys_with_running_child() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    s.running_output_tail.feed(b"name? ");
    s.child_input_active = true;
    assert!(s.panel_should_be_visible());
    assert!(!s.editor_owns_input());

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    for c in "alice".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
    }
    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"alice\r");
    assert!(!s.child_input_active);
}

#[test]
fn any_key_prompt_releases_after_one_key_answer() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    s.running_output_tail.feed(b"Press any key to continue");
    assert!(s.child_input_prompt_active());
    assert!(!s.editor_owns_input());

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"x");
    assert!(!s.child_input_active);
    assert!(!s.child_input_prompt_active());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"x");
    assert_eq!(s.editor.buffer, "q");
}

#[test]
fn child_input_prompt_clears_after_fresh_output_line() {
    let mut s = make_state();
    s.child_input_active = true;
    s.running_output_tail.feed(b"continue? ");

    update_child_input_detection(&mut s, b"y\r\ncontinuing\r\n");

    assert!(
        !s.child_input_active,
        "single-key prompts should release key routing once the child prints a fresh non-prompt line"
    );
}

#[test]
fn ctrl_c_interrupts_running_child_even_with_dirty_queue_input() {
    let mut s = make_state();
    s.force_queue = true;
    s.editor.insert_str("echo draft");
    assert!(s.editor_owns_input());

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), &[ETX]);
    assert_eq!(s.editor.buffer, "echo draft");
    assert!(s.last_sigint_at.is_some());
}

#[test]
fn ctrl_z_reaches_running_child_even_when_panel_owns_input() {
    let mut s = make_state();
    s.force_queue = true;
    s.editor.insert_str("echo draft");
    assert!(s.editor_owns_input());

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), &[0x1a]);
    assert_eq!(s.editor.buffer, "echo draft");
    assert!(s.status.contains("Ctrl-Z"));
}

#[test]
fn sigint_recently_forwarded_pauses_queue_on_command_end() {
    let mut state = make_state();
    state.queue.push("echo a", false);
    state.last_sigint_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_command_end(&mut state, Some(130), &mut w);
    assert!(state.queue.paused);
    assert_eq!(state.queue.len(), 1, "queued items kept, just paused");
    assert!(buf.lock().unwrap().is_empty(), "no command dispatched");
    assert!(state.status.contains("Ctrl-C"));
}

#[test]
fn exit_130_pauses_even_without_recent_sigint() {
    let mut state = make_state();
    state.queue.push("echo a", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_command_end(&mut state, Some(130), &mut w);
    assert!(state.queue.paused);
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn normal_exit_dispatches_next() {
    let mut state = make_state();
    state.queue.push("echo a", false);
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    assert!(handle_command_end(&mut state, Some(0), &mut w));
    assert!(!state.queue.paused);
    assert!(state.queue.is_empty(), "front item dispatched");
    assert!(matches!(state.shell_state, ShellState::Running));
    assert!(state.command_started_at.is_some());
    let written = buf.lock().unwrap();
    let s = String::from_utf8_lossy(&written);
    assert!(s.contains("echo a"));
}

#[test]
fn concurrent_command_end_only_one_session_claims_item() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut base = Queue::new();
    base.push("echo once", false);
    base.save(&queue_path).unwrap();

    let mut first = make_state();
    first.queue_path = queue_path.clone();
    first.queue = Queue::load_or_default(&queue_path);
    first.queue_known_items = first.queue.item_snapshot();
    first.queue_known_paused = first.queue.paused;

    let mut second = make_state();
    second.queue_path = queue_path.clone();
    second.queue = Queue::load_or_default(&queue_path);
    second.queue_known_items = second.queue.item_snapshot();
    second.queue_known_paused = second.queue.paused;

    let first_buf = Arc::new(Mutex::new(Vec::new()));
    let second_buf = Arc::new(Mutex::new(Vec::new()));
    let mut first_writer: Box<dyn Write + Send> = Box::new(VecWriter(first_buf.clone()));
    let mut second_writer: Box<dyn Write + Send> = Box::new(VecWriter(second_buf.clone()));

    assert!(handle_command_end(&mut first, Some(0), &mut first_writer));
    assert!(!handle_command_end(
        &mut second,
        Some(0),
        &mut second_writer
    ));

    assert_eq!(&*first_buf.lock().unwrap(), b"echo once\n");
    assert!(second_buf.lock().unwrap().is_empty());
    assert!(Queue::load_or_default(&queue_path).is_empty());
    assert!(second.status.contains("another session"));
}

#[test]
fn command_end_does_not_dispatch_item_cleared_by_another_session() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut base = Queue::new();
    base.push("echo stale", false);
    base.save(&queue_path).unwrap();

    let mut state = make_state();
    state.queue_path = queue_path.clone();
    state.queue = Queue::load_or_default(&queue_path);
    state.queue_known_items = state.queue.item_snapshot();
    state.queue_known_paused = state.queue.paused;

    let mut external = Queue::load_or_default(&queue_path);
    external.clear();
    external.save(&queue_path).unwrap();

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    assert!(!handle_command_end(&mut state, Some(0), &mut w));

    assert!(buf.lock().unwrap().is_empty());
    assert!(state.queue.is_empty());
    assert!(state.status.contains("another session"));
}

#[test]
fn dispatch_failure_rolls_claim_back_to_disk_paused() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut base = Queue::new();
    base.push("echo keep-me", false);
    base.save(&queue_path).unwrap();

    let mut state = make_state();
    state.queue_path = queue_path.clone();
    state.queue = Queue::load_or_default(&queue_path);
    state.queue_known_items = state.queue.item_snapshot();
    state.queue_known_paused = state.queue.paused;
    let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

    assert!(!handle_command_end(&mut state, Some(0), &mut w));

    let loaded = Queue::load_or_default(&queue_path);
    assert!(loaded.paused);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.items()[0].command, "echo keep-me");
    assert!(state.status.contains("dispatch failed"));
}

#[test]
fn resume_keeps_dispatch_failure_visible_and_paused() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.queue.push("echo keep", false);
    state.queue.paused = true;
    let mut writer: Box<dyn Write + Send> = Box::new(FailingWriter);
    toggle_queue_pause(&mut state, &mut writer);
    assert!(state.queue.paused);
    assert_eq!(state.queue.len(), 1);
    assert!(state.status.contains("dispatch failed"));
}

#[test]
fn claim_io_failure_pauses_without_writing_to_the_shell() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = make_state();
    let path = temp.path().join("queue.json");
    std::fs::create_dir(&path).unwrap();
    state.queue_path = path;
    state.queue.push("echo keep", false);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut writer: Box<dyn Write + Send> = Box::new(VecWriter(bytes.clone()));
    assert!(!dispatch_next_eligible(&mut state, Some(0), &mut writer));
    assert!(state.queue.paused);
    assert!(state.queue_dirty);
    assert_eq!(state.queue.len(), 1);
    assert!(bytes.lock().unwrap().is_empty());
    assert!(state.status.contains("queue dispatch sync failed"));
}

#[test]
fn invalid_command_is_kept_paused_without_partial_input() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.queue.push("printf before\0after", false);
    state.queue.paused = true;
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut writer: Box<dyn Write + Send> = Box::new(VecWriter(bytes.clone()));
    toggle_queue_pause(&mut state, &mut writer);
    assert!(state.queue.paused);
    assert_eq!(state.queue.len(), 1);
    assert!(bytes.lock().unwrap().is_empty());
    assert!(state.status.contains("NUL bytes"));
}

#[test]
fn command_end_pauses_queue_while_editing() {
    let mut state = make_state();
    state.queue.push("echo old", false);
    state.queue.push("echo next", false);
    state.editor.load_for_edit(0, "echo edited", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    assert!(!handle_command_end(&mut state, Some(0), &mut w));

    assert!(state.queue.paused);
    assert!(state.queue_dirty);
    assert_eq!(state.queue.len(), 2);
    assert_eq!(state.queue.items()[0].command, "echo old");
    assert_eq!(state.editor.editing_index, Some(0));
    assert!(buf.lock().unwrap().is_empty());
    assert!(state.status.contains("paused while editing"));
}

#[test]
fn command_end_pauses_before_dispatch_when_shell_cwd_changed() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.shell_cwd = Some(PathBuf::from("/tmp/current"));
    state.queue.set_origin_cwd("/tmp/original");
    state.queue.push("echo keep-me", false);
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    assert!(!handle_command_end(&mut state, Some(0), &mut w));

    assert!(state.queue.paused);
    assert!(Queue::load_or_default(&state.queue_path).paused);
    assert!(state.status.contains("Ctrl-X to run here"));
    assert!(state.resume_cwd_confirmation_started_at.is_some());
    assert!(buf.lock().unwrap().is_empty());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(!state.queue.paused);
    assert!(state.queue.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
}

#[test]
fn failed_command_skips_foreign_conditional_before_cwd_warning() {
    let mut state = make_state();
    state.shell_cwd = Some(PathBuf::from("/tmp/current"));
    state
        .queue
        .push_with_origin("echo should-skip", true, Some(PathBuf::from("/tmp/other")));
    state.queue.push_with_origin(
        "echo should-run",
        false,
        Some(PathBuf::from("/tmp/current")),
    );
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    assert!(handle_command_end(&mut state, Some(1), &mut w));

    assert!(!state.queue.paused);
    assert!(state.queue.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"echo should-run\n");
    assert!(
        !state.status.contains("/tmp/other"),
        "skipped chained item should not trigger cwd warning"
    );
}

#[test]
fn dispatch_failure_keeps_command_and_pauses_queue() {
    let mut state = make_state();
    state.queue.push("echo keep-me", false);
    state.queue_dirty = true;
    let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

    assert!(!dispatch_next_eligible(&mut state, Some(0), &mut w));

    assert_eq!(state.queue.len(), 1);
    assert_eq!(state.queue.front().unwrap().command, "echo keep-me");
    assert!(state.queue.paused);
    let disk = Queue::load_or_default(&state.queue_path);
    assert!(disk.paused);
    assert_eq!(disk.front().unwrap().command, "echo keep-me");
    assert!(state.status.contains("dispatch failed"));
}

#[test]
fn dispatch_failure_after_conditional_skip_preserves_next_command() {
    let mut state = make_state();
    state.queue.push("echo skipped", true);
    state.queue.push("echo keep-me", false);
    state.queue_dirty = true;
    let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

    assert!(!dispatch_next_eligible(&mut state, Some(1), &mut w));

    assert_eq!(state.queue.len(), 1);
    assert_eq!(state.queue.front().unwrap().command, "echo keep-me");
    assert!(state.queue.paused);
    let disk = Queue::load_or_default(&state.queue_path);
    assert!(disk.paused);
    assert_eq!(disk.front().unwrap().command, "echo keep-me");
}

#[test]
fn dispatch_persists_removed_item_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut state = make_state();
    state.queue.push("echo once", false);
    state.queue.save(&queue_path).unwrap();
    state.queue_known_items = state.queue.item_snapshot();
    state.queue_path = queue_path.clone();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    assert!(dispatch_next_eligible(&mut state, Some(0), &mut w));

    assert_eq!(&*buf.lock().unwrap(), b"echo once\n");
    assert!(state.queue.is_empty());
    assert!(!state.queue_dirty);
    assert!(Queue::load_or_default(&queue_path).is_empty());
}

#[test]
fn resume_paused_queue_at_prompt_dispatches_immediately() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.queue.paused = true;
    state.force_queue = true;
    state.queue.push("echo resumed", false);
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );

    assert!(!state.queue.paused);
    assert!(state.queue.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"echo resumed\n");
    assert!(Queue::load_or_default(&state.queue_path).is_empty());
}

#[test]
fn resume_paused_queue_at_prompt_waits_for_edit_to_finish() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.queue.paused = true;
    state.force_queue = true;
    state.queue.push("echo old", false);
    state.editor.load_for_edit(0, "echo edited", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );

    assert!(state.queue.paused);
    assert_eq!(state.queue.len(), 1);
    assert!(buf.lock().unwrap().is_empty());
    assert!(state.status.contains("finish or cancel"));
}

#[test]
fn resume_paused_queue_at_prompt_does_not_fake_success_for_conditional() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.queue.paused = true;
    state.force_queue = true;
    state.queue.push("echo should-skip", true);
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );

    assert!(!state.queue.paused);
    assert!(
        state.queue.is_empty(),
        "unknown previous exit skips conditional"
    );
    assert!(buf.lock().unwrap().is_empty());
    assert!(state.status.contains("skipped chained item"));
    assert!(Queue::load_or_default(&state.queue_path).is_empty());
}

#[test]
fn old_sigint_does_not_pause() {
    let mut state = make_state();
    state.queue.push("echo a", false);
    state.last_sigint_at = Some(Instant::now() - Duration::from_secs(10));
    state.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_command_end(&mut state, Some(0), &mut w);
    assert!(!state.queue.paused);
    assert!(!buf.lock().unwrap().is_empty(), "stale sigint ignored");
}

#[test]
fn empty_queue_with_sigint_does_not_pause() {
    let mut state = make_state();
    state.last_sigint_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_command_end(&mut state, Some(130), &mut w);
    assert!(
        !state.queue.paused,
        "no queued items → no need to auto-pause"
    );
}

#[test]
fn command_start_clears_stale_prompt_bracketed_paste_mode() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    for c in "echo hi".chars() {
        s.prompt
            .on_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    s.child.bracketed_paste = true;
    s.child_input_active = true;

    mark_command_started(&mut s);

    assert!(matches!(s.shell_state, ShellState::Running));
    assert!(s.command_started_at.is_some());
    assert!(s.prompt.is_empty());
    assert!(!s.child_input_active);
    assert!(
        !s.child.bracketed_paste,
        "paste mode requested by the shell prompt must not leak into the child command"
    );
}

#[test]
fn command_end_clears_child_bracketed_paste_mode() {
    let mut s = make_state();
    s.child.bracketed_paste = true;

    lifecycle::command_ended(&mut s, Some(0));

    assert!(
        !s.child.bracketed_paste,
        "paste mode requested by a child command must not leak back to the shell prompt"
    );
}

#[test]
fn command_end_clears_manual_passthrough_so_prompt_controls_return() {
    let mut s = make_state();
    s.manual_passthrough = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_command_end(&mut s, Some(0), &mut w);
    s.shell_state = ShellState::AtPrompt;
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.manual_passthrough);
    assert!(s.force_queue, "Ctrl-Q should work again at the next prompt");
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn quick_command_does_not_show_panel() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now());
    assert!(!s.panel_should_be_visible());
}

#[test]
fn first_typed_key_during_delay_opens_queue_instead_of_forwarding() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now());
    assert!(!s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor_owns_input(), "queue editor should take focus");
    assert_eq!(s.editor.buffer, "e");
    assert!(
        buf.lock().unwrap().is_empty(),
        "first queue key must not leak to running command"
    );
}

#[test]
fn enter_at_prompt_optimistically_starts_command_for_fast_followup_typing() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for c in "sleep 5".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
    }
    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(matches!(s.shell_state, ShellState::Running));
    assert!(!s.editor_owns_input(), "delay should still suppress flash");

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(s.editor.buffer, "e");
    assert_eq!(&*buf.lock().unwrap(), b"sleep 5\r");
}

#[test]
fn history_navigation_invalidates_prompt_tracker_for_fast_followup_capture() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for c in "echo stale".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
    }
    let _ = handle_key(
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(matches!(s.shell_state, ShellState::AtPrompt));
    assert_eq!(&*buf.lock().unwrap(), b"echo stale\x1b[A\r");
}

#[test]
fn heredoc_prompt_line_does_not_trigger_fast_followup_capture() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for c in "cat <<EOF".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
    }
    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(matches!(s.shell_state, ShellState::AtPrompt));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"cat <<EOF\rh");
}

#[test]
fn multiline_if_continuation_lines_do_not_trigger_fast_followup_capture() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for line in ["if true", "then", "echo body", "fi"] {
        for c in line.chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        assert!(
            s.editor.buffer.is_empty(),
            "{line:?} leaked into queue editor"
        );
    }

    assert!(matches!(s.shell_state, ShellState::AtPrompt));
    assert_eq!(&*buf.lock().unwrap(), b"if true\rthen\recho body\rfi\r");
}

#[test]
fn incomplete_shell_lines_are_not_optimistically_marked_running() {
    assert!(!is_likely_complete_shell_command("echo 'unterminated"));
    assert!(!is_likely_complete_shell_command("for f in *; do"));
    assert!(!is_likely_complete_shell_command("for f in"));
    assert!(!is_likely_complete_shell_command("if true"));
    assert!(!is_likely_complete_shell_command("if true; then"));
    assert!(!is_likely_complete_shell_command("while true"));
    assert!(!is_likely_complete_shell_command("case \"$x\" in"));
    assert!(!is_likely_complete_shell_command("foo() {"));
    assert!(!is_likely_complete_shell_command("echo one |"));
    assert!(!is_likely_complete_shell_command("echo $(printf hi"));
    assert!(!is_likely_complete_shell_command("echo $((1 +"));
    assert!(!is_likely_complete_shell_command("(echo hi"));
    assert!(is_likely_complete_shell_command("sleep 5"));
    assert!(is_likely_complete_shell_command("printf 'ok\\n'"));
    assert!(is_likely_complete_shell_command("echo $(printf hi)"));
    assert!(is_likely_complete_shell_command("echo $((1 + 2))"));
    assert!(is_likely_complete_shell_command("(echo hi)"));
    assert!(is_likely_complete_shell_command(
        "if true; then echo ok; fi"
    ));
    assert!(is_likely_complete_shell_command(
        "for f in *; do echo \"$f\"; done"
    ));
}

#[test]
fn ctrl_c_during_delay_still_forwards_sigint() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.editor_owns_input());
    assert_eq!(&*buf.lock().unwrap(), &[ETX]);
    assert!(s.last_sigint_at.is_some());
}

#[test]
fn help_ctrl_c_still_interrupts_running_child() {
    let mut s = make_state();
    s.show_help = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.show_help);
    assert_eq!(&*buf.lock().unwrap(), &[ETX]);
    assert!(s.last_sigint_at.is_some());
}

#[test]
fn help_ctrl_c_at_prompt_only_dismisses_help() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.show_help = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.show_help);
    assert!(buf.lock().unwrap().is_empty());
    assert!(s.last_sigint_at.is_none());
}

#[test]
fn paste_dismisses_help_then_queues_normally() {
    let mut s = make_state();
    s.force_queue = true;
    s.show_help = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("echo after-help".to_string(), &mut s, &mut w);

    assert!(!s.show_help);
    assert_eq!(s.editor.buffer, "echo after-help");
}

#[test]
fn paste_cancels_pending_quit_confirmation() {
    let mut s = make_state();
    s.force_queue = true;
    s.pending_quit_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("echo not-discarding".to_string(), &mut s, &mut w);

    assert!(s.pending_quit_at.is_none());
    assert_eq!(s.editor.buffer, "echo not-discarding");
}

#[test]
fn paste_at_prompt_updates_tracker_for_fast_followup_capture() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("sleep 5".to_string(), &mut s, &mut w);
    assert_eq!(&*buf.lock().unwrap(), b"sleep 5");

    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(matches!(s.shell_state, ShellState::Running));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"sleep 5\r");
    assert_eq!(s.editor.buffer, "e");
}

#[test]
fn newline_paste_at_prompt_optimistically_starts_command_for_fast_followup() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("sleep 5\n".to_string(), &mut s, &mut w);
    assert!(matches!(s.shell_state, ShellState::Running));
    assert!(s.command_started_at.is_some());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"sleep 5\n");
    assert_eq!(s.editor.buffer, "e");
}

#[test]
fn bracketed_newline_paste_waits_for_shell_confirmation() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.child.bracketed_paste = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("sleep 5\n".to_string(), &mut s, &mut w);

    assert!(matches!(s.shell_state, ShellState::AtPrompt));
    assert!(s.command_started_at.is_none());
    assert_eq!(&*buf.lock().unwrap(), b"\x1b[200~sleep 5\n\x1b[201~");

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"\x1b[200~sleep 5\n\x1b[201~e");
}

#[test]
fn leading_blank_paste_line_still_detects_later_submitted_command() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("\nsleep 5\n".to_string(), &mut s, &mut w);
    assert!(matches!(s.shell_state, ShellState::Running));
    assert!(s.command_started_at.is_some());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"\nsleep 5\n");
    assert_eq!(s.editor.buffer, "e");
}

#[test]
fn leading_comment_paste_line_still_detects_later_submitted_command() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("# setup\nsleep 5\n".to_string(), &mut s, &mut w);
    assert!(matches!(s.shell_state, ShellState::Running));
    assert!(s.command_started_at.is_some());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), b"# setup\nsleep 5\n");
    assert_eq!(s.editor.buffer, "e");
}

#[test]
fn leading_noop_paste_before_heredoc_keeps_continuation_passthrough() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("\n# setup\ncat <<EOF\n".to_string(), &mut s, &mut w);
    assert!(matches!(s.shell_state, ShellState::AtPrompt));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"\n# setup\ncat <<EOF\nh");
}

#[test]
fn incomplete_newline_paste_at_prompt_does_not_capture_continuation() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("cat <<EOF\n".to_string(), &mut s, &mut w);
    assert!(matches!(s.shell_state, ShellState::AtPrompt));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"cat <<EOF\nh");
}

#[test]
fn paste_during_delay_opens_queue_instead_of_forwarding() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("echo pasted\n".to_string(), &mut s, &mut w);

    assert!(s.editor_owns_input(), "paste should reveal queue editor");
    assert_eq!(s.editor.buffer, "echo pasted");
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn long_running_command_shows_panel() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    assert!(s.panel_should_be_visible());
}

#[test]
fn force_queue_bypasses_delay() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now()); // just started
    s.force_queue = true;
    assert!(s.panel_should_be_visible());
}

#[test]
fn tiny_terminal_disables_panel_capture() {
    let mut s = make_state();
    s.force_queue = true;
    s.terminal_allows_panel = false;
    assert!(!s.panel_should_be_visible());
    assert!(!s.editor_owns_input());
}

#[test]
fn ctrl_q_does_not_arm_invisible_force_queue_on_tiny_terminal() {
    let mut s = make_state();
    s.terminal_allows_panel = false;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.force_queue);
    assert!(s.status.contains("too small"));
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn tiny_terminal_ctrl_x_can_resume_paused_queue() {
    let mut s = make_state();
    s.terminal_allows_panel = false;
    s.shell_state = ShellState::AtPrompt;
    s.queue.push("echo tiny", false);
    s.queue.paused = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.queue.paused);
    assert!(s.queue.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"echo tiny\n");
}

#[test]
fn tiny_terminal_ctrl_k_can_clear_paused_queue() {
    let mut s = make_state();
    s.terminal_allows_panel = false;
    s.queue.push("echo tiny", false);
    s.queue.paused = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for _ in 0..2 {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );
    }

    assert!(s.queue.is_empty());
    assert!(!s.queue.paused);
    assert!(s.status.contains("queue cleared"));
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn auto_passthrough_hides_panel_even_when_force_queue() {
    // alt-screen always wins — vim/htop expect the whole screen.
    let mut s = make_state();
    s.force_queue = true;
    s.auto_passthrough = true;
    assert!(!s.panel_should_be_visible());
}

#[test]
fn manual_passthrough_keeps_panel_but_releases_keys() {
    // raw input mode: panel still drawn, but keystrokes go to the PTY.
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    s.manual_passthrough = true;
    assert!(s.panel_should_be_visible());
    assert!(!s.editor_owns_input());
}

#[test]
fn ctrl_backslash_exits_manual_passthrough() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    s.manual_passthrough = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.manual_passthrough);
    assert!(
        buf.lock().unwrap().is_empty(),
        "exit chord should not leak to the child process"
    );
}

#[test]
fn ctrl_backslash_reaches_running_child_when_panel_owns_input() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    assert!(s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert_eq!(&*buf.lock().unwrap(), &[FS]);
    assert!(!s.manual_passthrough);
    assert!(s.status.contains("Ctrl-\\"));
}

#[test]
fn raw_passthrough_sends_ctrl_q_to_child() {
    let mut s = make_state();
    s.force_queue = true;
    s.manual_passthrough = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(s.force_queue, "Ctrl-Q must not toggle cmdq while raw");
    assert_eq!(&*buf.lock().unwrap(), &[0x11]);
}

#[test]
fn auto_passthrough_sends_f1_to_child() {
    let mut s = make_state();
    s.auto_passthrough = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(!s.show_help, "F1 belongs to the alt-screen app");
    assert_eq!(&*buf.lock().unwrap(), b"\x1bOP");
}

#[test]
fn pending_quit_active_within_window() {
    let mut s = make_state();
    assert!(!s.pending_quit_active());
    s.pending_quit_at = Some(Instant::now());
    assert!(s.pending_quit_active());
    s.pending_quit_at = Some(Instant::now() - QUIT_CONFIRM_WINDOW - Duration::from_secs(1));
    assert!(!s.pending_quit_active());
}

#[test]
fn ctrl_k_clear_resets_pause_and_edit_state() {
    let mut s = make_state();
    let id = s.queue.push("echo doomed", false);
    s.queue.paused = true;
    s.force_queue = true;
    s.editor.load_for_edit(0, "echo doomed", false);
    assert_eq!(s.queue.items()[0].id, id);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for _ in 0..2 {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );
    }

    assert!(s.queue.is_empty());
    assert!(!s.queue.paused);
    assert!(s.editor.editing_index.is_none());
    assert!(s.editor.buffer.is_empty());
}

#[test]
fn truncate_for_status_long_command() {
    let long: String = "x".repeat(200);
    let t = truncate_for_status(&long);
    assert!(t.chars().count() <= 61);
    assert!(t.ends_with('…'));
}

#[test]
fn truncate_for_status_short_command() {
    assert_eq!(truncate_for_status("ls -la"), "ls -la");
}

fn esc_press() -> KeyEvent {
    let mut k = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    k.kind = KeyEventKind::Press;
    k
}

#[test]
fn double_esc_toggles_passthrough() {
    let mut s = make_state();
    // Force queue mode so the editor has focus.
    s.force_queue = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    assert!(!s.manual_passthrough);
    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(!s.manual_passthrough, "single Esc must not toggle");
    assert!(s.last_esc_at.is_some());

    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(s.manual_passthrough, "second Esc within window toggles");
    assert!(
        s.last_esc_at.is_none(),
        "timestamp cleared so a 3rd quick Esc starts a fresh window"
    );

    let _ = handle_key(esc_press(), &mut s, &mut w);
    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(
        !s.manual_passthrough,
        "double-Esc again returns to queue mode"
    );
}

#[test]
fn double_esc_at_plain_prompt_does_not_enter_invisible_passthrough() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    let _ = handle_key(esc_press(), &mut s, &mut w);

    assert!(!s.manual_passthrough);
    assert_eq!(&*buf.lock().unwrap(), b"\x1b\x1b");
}

#[test]
fn double_esc_passes_through_in_auto_passthrough() {
    let mut s = make_state();
    s.auto_passthrough = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    let _ = handle_key(esc_press(), &mut s, &mut w);

    assert!(!s.manual_passthrough);
    assert_eq!(&*buf.lock().unwrap(), b"\x1b\x1b");
}

#[test]
fn esc_outside_window_does_not_toggle() {
    let mut s = make_state();
    s.force_queue = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    s.last_esc_at = Some(Instant::now() - ESC_DOUBLE_TAP_WINDOW - Duration::from_millis(50));
    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(
        !s.manual_passthrough,
        "second Esc outside window must not toggle"
    );
}

#[test]
fn non_esc_key_disarms_double_esc_passthrough() {
    let mut s = make_state();
    s.force_queue = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    let _ = handle_key(esc_press(), &mut s, &mut w);

    assert!(
        !s.manual_passthrough,
        "Esc, another key, Esc should not count as double-Esc raw input"
    );
    assert!(
        s.editor.buffer.is_empty(),
        "final Esc should clear editor input"
    );
    assert!(
        buf.lock().unwrap().is_empty(),
        "editor-owned keys should not leak"
    );
}

#[test]
fn esc_that_clears_input_does_not_arm_double_esc_passthrough() {
    let mut s = make_state();
    s.force_queue = true;
    s.editor.insert_str("draft");
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(s.editor.buffer.is_empty());
    assert!(s.last_esc_at.is_none());

    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(
        !s.manual_passthrough,
        "Esc after clearing input should start a fresh double-Esc window"
    );
    assert!(s.last_esc_at.is_some());
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn esc_that_cancels_edit_does_not_arm_double_esc_passthrough() {
    let mut s = make_state();
    s.force_queue = true;
    s.queue.push("echo queued", false);
    s.editor.load_for_edit(0, "echo queued", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(s.editor.editing_index.is_none());
    assert!(s.last_esc_at.is_none());

    let _ = handle_key(esc_press(), &mut s, &mut w);
    assert!(
        !s.manual_passthrough,
        "Esc after canceling edit should start a fresh double-Esc window"
    );
    assert!(s.last_esc_at.is_some());
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn startup_restores_non_empty_queue_paused_and_visible() {
    let mut q = Queue::new();
    q.push("echo keep-me", false);

    let RestoredQueue {
        status,
        resume_cwd_warning: warning,
        paused_locally: local_pause,
    } = prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/cmdq-test")), 0);

    assert!(q.paused);
    assert!(local_pause);
    assert!(status.unwrap().contains("restored 1 command"));
    assert!(warning.is_none());

    let mut s = make_state();
    s.queue = q;
    assert!(
        s.panel_should_be_visible(),
        "restored queues should be visible instead of silently waiting"
    );
}

#[test]
fn startup_status_mentions_active_peer_session() {
    let mut q = Queue::new();
    q.push("echo keep-me", false);

    let RestoredQueue {
        status,
        resume_cwd_warning: warning,
        paused_locally: local_pause,
    } = prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/cmdq-test")), 1);

    assert!(q.paused);
    assert!(local_pause);
    assert!(warning.is_none());
    assert!(status.unwrap().contains("another session active"));
}

#[test]
fn startup_local_pause_does_not_persist_over_live_unpaused_queue() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut live = Queue::new();
    live.push("echo live", false);
    live.save(&queue_path).unwrap();

    let mut restored = Queue::load_or_default(&queue_path);
    let known_paused = restored.paused;
    let RestoredQueue {
        paused_locally: local_pause,
        ..
    } = prepare_queue_for_startup(&mut restored, Some(Path::new("/tmp/cmdq-test")), 0);

    let mut state = make_state();
    state.queue_path = queue_path.clone();
    state.queue = restored;
    state.queue_known_items = state.queue.item_snapshot();
    state.queue_known_paused = known_paused;
    state.restored_queue_paused_locally = local_pause;

    assert!(state.queue.paused);
    assert!(!state.queue_dirty);
    save_queue_if_dirty(&mut state);
    assert!(
        !Queue::load_or_default(&queue_path).paused,
        "opening a second cmdq should not persist a pause over an active unpaused queue"
    );

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut state, &mut last_sync);
    assert!(
        state.queue.paused,
        "the restored session still keeps its own safety pause"
    );
    assert_eq!(state.queue_known_paused, known_paused);

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut state, &mut last_sync);
    assert!(
        state.queue.paused,
        "repeated syncs should not turn a session-local restore pause into a resume"
    );
}

#[test]
fn startup_restored_queue_from_different_cwd_requires_resume_confirmation() {
    let mut q = Queue::new();
    q.set_origin_cwd("/tmp/original");
    q.push("echo keep-me", false);

    let RestoredQueue {
        status,
        resume_cwd_warning: warning,
        paused_locally: local_pause,
    } = prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

    assert!(q.paused);
    assert!(local_pause);
    assert!(status.unwrap().contains("saved in /tmp/original"));
    assert!(warning.unwrap().contains("press Ctrl-X again"));

    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.shell_cwd = Some(PathBuf::from("/tmp/current"));
    state.session_cwd = Some(PathBuf::from("/tmp/current"));
    state.queue = q;
    state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(state.queue.paused);
    assert!(buf.lock().unwrap().is_empty());
    assert!(state.status.contains("/tmp/original"));
    assert!(state.status.contains("/tmp/current"));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(!state.queue.paused);
    assert!(state.queue.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
}

#[test]
fn startup_restored_mixed_cwd_queue_requires_resume_confirmation() {
    let mut q = Queue::new();
    q.push_with_origin("echo local", false, Some(PathBuf::from("/tmp/current")));
    q.push_with_origin("echo remote", false, Some(PathBuf::from("/tmp/remote")));

    let RestoredQueue {
        status,
        resume_cwd_warning: warning,
        paused_locally: local_pause,
    } = prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

    assert!(q.paused);
    assert!(local_pause);
    assert!(status.unwrap().contains("/tmp/remote"));
    assert!(warning.unwrap().contains("/tmp/remote"));
}

#[test]
fn startup_restored_multiple_foreign_origins_mentions_other_dirs() {
    let mut q = Queue::new();
    q.push_with_origin("echo one", false, Some(PathBuf::from("/tmp/one")));
    q.push_with_origin("echo two", false, Some(PathBuf::from("/tmp/two")));

    let RestoredQueue {
        status,
        resume_cwd_warning: warning,
        ..
    } = prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

    assert!(status.unwrap().contains("1 other dirs"));
    assert!(warning.unwrap().contains("1 other dirs"));
}

#[test]
fn resume_cwd_confirmation_cancels_on_other_key() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.shell_cwd = Some(PathBuf::from("/tmp/current"));
    state.queue.paused = true;
    state.queue.set_origin_cwd("/tmp/original");
    state.queue.push("echo keep-me", false);
    state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(state.queue.paused);
    assert!(state.resume_cwd_confirmation_started_at.is_some());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut state,
        &mut w,
    );
    assert!(state.resume_cwd_confirmation_started_at.is_none());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(state.queue.paused);
    assert!(
        state.resume_cwd_confirmation_started_at.is_some(),
        "Ctrl-X after another key should warn again, not resume"
    );
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn resume_cwd_confirmation_expires() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.shell_cwd = Some(PathBuf::from("/tmp/current"));
    state.queue.paused = true;
    state.queue.set_origin_cwd("/tmp/original");
    state.queue.push("echo keep-me", false);
    state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
    state.resume_cwd_confirmation_started_at =
        Some(Instant::now() - RESUME_CWD_CONFIRM_WINDOW - Duration::from_millis(1));
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );

    assert!(state.queue.paused);
    assert!(state.resume_cwd_confirmation_started_at.is_some());
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn resume_rechecks_latest_shell_cwd_before_dispatch() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.session_cwd = Some(PathBuf::from("/tmp/original"));
    state.shell_cwd = Some(PathBuf::from("/tmp/after-rc"));
    state.queue.paused = true;
    state.queue.set_origin_cwd("/tmp/original");
    state.queue.push("echo keep-me", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(state.queue.paused);
    assert!(state.status.contains("/tmp/original"));
    assert!(state.status.contains("/tmp/after-rc"));
    assert!(buf.lock().unwrap().is_empty());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );
    assert!(!state.queue.paused);
    assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
}

#[test]
fn stale_resume_cwd_warning_clears_when_shell_returns_to_origin() {
    let mut state = make_state();
    state.shell_state = ShellState::AtPrompt;
    state.session_cwd = Some(PathBuf::from("/tmp/current"));
    state.shell_cwd = Some(PathBuf::from("/tmp/original"));
    state.queue.paused = true;
    state.queue.set_origin_cwd("/tmp/original");
    state.queue.push("echo keep-me", false);
    state.resume_cwd_warning = Some(
        "queue was saved in /tmp/original; press Ctrl-X again to run here (/tmp/current)"
            .to_string(),
    );
    state.resume_cwd_confirmation_started_at = Some(Instant::now());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut state,
        &mut w,
    );

    assert!(!state.queue.paused);
    assert!(state.resume_cwd_warning.is_none());
    assert!(state.resume_cwd_confirmation_started_at.is_none());
    assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
}

#[test]
fn queued_item_origin_uses_latest_shell_cwd() {
    let mut state = make_state();
    state.force_queue = true;
    state.shell_cwd = Some(PathBuf::from("/tmp/inner-shell"));
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    for c in "echo here".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut state,
            &mut w,
        );
    }
    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
        &mut w,
    );

    assert_eq!(
        state.queue.origin_cwd(),
        Some(Path::new("/tmp/inner-shell"))
    );
    assert_eq!(state.queue.front().unwrap().command, "echo here");
    assert_eq!(
        state.queue.front().unwrap().origin_cwd.as_deref(),
        Some(Path::new("/tmp/inner-shell"))
    );
    assert!(buf.lock().unwrap().is_empty());
}

#[test]
fn queue_save_failure_keeps_dirty_and_surfaces_status() {
    let temp = tempfile::tempdir().unwrap();
    let not_a_dir = temp.path().join("not-a-dir");
    std::fs::write(&not_a_dir, b"file").unwrap();
    let queue_path = not_a_dir.join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path;
    s.queue.push("echo persist-me", false);
    s.queue_dirty = true;

    save_queue_if_dirty(&mut s);

    assert!(s.queue_dirty);
    assert!(s.status.contains("queue save failed"));
    assert!(s.queue_save_error.is_some());
}

#[test]
fn queue_save_merges_unseen_external_items() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut external = Queue::new();
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo local", false);
    s.queue_dirty = true;

    save_queue_if_dirty(&mut s);

    let loaded = Queue::load_or_default(&queue_path);
    let commands: Vec<_> = loaded
        .items()
        .iter()
        .map(|it| it.command.as_str())
        .collect();
    assert!(
        s.queue_dirty,
        "external merge should schedule a follow-up save for the paused state"
    );
    assert!(s.status.contains("merged 1 queued item"));
    assert!(s.status.contains("queue paused"));
    assert_eq!(loaded.len(), 2);
    assert!(commands.contains(&"echo external"));
    assert!(commands.contains(&"echo local"));

    save_queue_if_dirty(&mut s);
    assert!(!s.queue_dirty);
}

#[test]
fn queue_save_reports_corrupt_disk_backup() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    std::fs::write(&queue_path, b"{broken queue").unwrap();

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo local", false);
    s.queue_dirty = true;

    save_queue_if_dirty(&mut s);

    assert!(!s.queue_dirty);
    assert!(s.status.contains("ignored corrupt queue file"));
    assert_eq!(
        Queue::load_or_default(&queue_path).front().unwrap().command,
        "echo local"
    );
}

#[test]
fn queue_save_pauses_after_merging_unseen_external_items() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut external = Queue::new();
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo local", false);
    s.queue_dirty = true;

    save_queue_if_dirty(&mut s);

    assert!(s.queue.paused);
    assert!(
        s.queue_dirty,
        "pause state should be persisted on the next save"
    );
    assert!(s.status.contains("queue paused"));

    save_queue_if_dirty(&mut s);
    assert!(!s.queue_dirty);
    assert!(Queue::load_or_default(&queue_path).paused);
}

#[test]
fn saved_edit_is_not_lost_when_another_session_deletes_item() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");
    let mut base = Queue::new();
    let shared_id = base.push("echo original", false);
    base.save(&queue_path).unwrap();

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue = Queue::load_or_default(&queue_path);
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;
    s.editor
        .load_for_edit(0, s.queue.items()[0].command.as_str(), false);

    let mut external = Queue::load_or_default(&queue_path);
    assert!(external.remove(shared_id).is_some());
    external.save(&queue_path).unwrap();

    s.editor.buffer = "echo local edit".to_string();
    s.editor.cursor = s.editor.buffer.len();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf));

    let outcome = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(matches!(outcome, KeyOutcome::Continue));
    save_queue_if_dirty(&mut s);

    let loaded = Queue::load_or_default(&queue_path);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.items()[0].command, "echo local edit");
    assert!(s.status.contains("kept local edits"));
}

#[test]
fn idle_queue_sync_merges_external_items_and_pauses() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo local", false);
    s.queue.save(&queue_path).unwrap();
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;

    let mut external = Queue::load_or_default(&queue_path);
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    let commands: Vec<_> = s
        .queue
        .items()
        .iter()
        .map(|it| it.command.as_str())
        .collect();
    assert_eq!(commands, vec!["echo local", "echo external"]);
    assert!(s.queue.paused);
    assert!(s.queue_dirty, "auto-pause should be persisted");
    assert!(s.status.contains("merged 1 queued item"));
    assert!(s.status.contains("queue paused"));

    save_queue_if_dirty(&mut s);
    assert!(!s.queue_dirty);
    assert!(Queue::load_or_default(&queue_path).paused);
}

#[test]
fn idle_queue_sync_reports_external_change_during_draft_without_clobbering_input() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.force_queue = true;
    s.queue.push("echo local", false);
    s.queue.save(&queue_path).unwrap();
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;
    s.editor.insert_str("echo draft");

    let mut external = Queue::load_or_default(&queue_path);
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    assert_eq!(s.editor.buffer, "echo draft");
    assert_eq!(s.queue.len(), 1);
    assert!(!s.queue_dirty);
    assert!(s.queue_external_change_pending);
    assert!(s.status.contains("finish or cancel edit to merge"));

    s.editor.reset();
    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    let commands: Vec<_> = s
        .queue
        .items()
        .iter()
        .map(|it| it.command.as_str())
        .collect();
    assert_eq!(commands, vec!["echo local", "echo external"]);
    assert!(!s.queue_external_change_pending);
}

#[test]
fn deferred_external_change_warning_persists_until_edit_can_merge() {
    let mut s = make_state();
    s.queue_external_change_pending = true;
    s.set_status(queue_sync::DEFERRED_QUEUE_CHANGE_STATUS);
    s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));

    s.tick_status();

    assert_eq!(s.status, queue_sync::DEFERRED_QUEUE_CHANGE_STATUS);
    assert!(s.status_set_at.is_some());

    s.set_status("temporary note");
    s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    s.tick_status();

    assert_eq!(s.status, queue_sync::DEFERRED_QUEUE_CHANGE_STATUS);

    s.queue_external_change_pending = false;
    s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    s.tick_status();

    assert!(s.status.is_empty());
    assert!(s.status_set_at.is_none());
}

#[test]
fn deferred_external_change_warning_clears_when_disk_returns_to_known_state() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.force_queue = true;
    s.queue.push("echo local", false);
    s.queue.save(&queue_path).unwrap();
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;
    s.editor.insert_str("echo draft");

    let mut external = Queue::load_or_default(&queue_path);
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);
    assert!(s.queue_external_change_pending);
    assert_eq!(s.status, queue_sync::DEFERRED_QUEUE_CHANGE_STATUS);

    s.queue.save(&queue_path).unwrap();
    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    assert!(!s.queue_external_change_pending);
    assert!(s.status.is_empty());
    assert!(s.status_set_at.is_none());
    assert_eq!(s.editor.buffer, "echo draft");
}

#[test]
fn idle_queue_sync_reports_external_change_during_item_edit_without_clobbering_edit() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo local", false);
    s.queue.save(&queue_path).unwrap();
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;
    s.editor.load_for_edit(0, "echo local changed", false);

    let mut external = Queue::load_or_default(&queue_path);
    external.push("echo external", false);
    external.save(&queue_path).unwrap();

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    assert_eq!(s.editor.buffer, "echo local changed");
    assert_eq!(s.editor.editing_index, Some(0));
    assert_eq!(s.queue.len(), 1);
    assert!(s.queue_external_change_pending);
    assert!(s.status.contains("finish or cancel edit to merge"));
}

#[test]
fn idle_queue_sync_adopts_external_clear() {
    let temp = tempfile::tempdir().unwrap();
    let queue_path = temp.path().join("queue.json");

    let mut s = make_state();
    s.queue_path = queue_path.clone();
    s.queue.push("echo clear me", false);
    s.queue.save(&queue_path).unwrap();
    s.queue_known_items = s.queue.item_snapshot();
    s.queue_known_paused = s.queue.paused;
    std::fs::remove_file(&queue_path).unwrap();

    let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
    sync_queue_from_disk_if_due(&mut s, &mut last_sync);

    assert!(s.queue.is_empty());
    assert!(!s.queue_dirty);
    assert_eq!(s.status, "queue cleared by another session");
    assert!(s.queue_known_items.is_empty());
}

#[test]
fn confirmed_ctrl_d_discards_queue_before_quit() {
    let mut s = make_state();
    s.force_queue = true;
    s.queue.push("echo discard", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let first = handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );
    assert!(matches!(first, KeyOutcome::Continue));
    assert_eq!(s.queue.len(), 1);

    let second = handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(matches!(second, KeyOutcome::Quit));
    assert!(s.queue.is_empty());
    assert!(!s.queue.paused);
    assert!(s.queue_dirty);
}

#[test]
fn paste_in_passthrough_is_plain_until_child_enables_bracketed_paste() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    // Ensure editor doesn't own input → paste goes to PTY.
    assert!(!s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_paste("hello".to_string(), &mut s, &mut w);
    assert_eq!(&*buf.lock().unwrap(), b"hello");
}

#[test]
fn paste_in_passthrough_wraps_when_child_enabled_bracketed_paste() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.child.bracketed_paste = true;
    assert!(!s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_paste("hello".to_string(), &mut s, &mut w);
    let written = buf.lock().unwrap().clone();
    assert!(written.starts_with(b"\x1b[200~"));
    assert!(written.ends_with(b"\x1b[201~"));
    assert!(
        String::from_utf8_lossy(&written).contains("hello"),
        "payload preserved"
    );
}

#[test]
fn paste_to_child_input_prompt_does_not_open_queue() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay() - Duration::from_millis(50));
    s.running_output_tail.feed(b"name? ");
    s.child_input_active = true;
    assert!(s.panel_should_be_visible());
    assert!(!s.editor_owns_input());

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_paste("alice\n".to_string(), &mut s, &mut w);

    assert!(s.editor.buffer.is_empty());
    assert_eq!(&*buf.lock().unwrap(), b"alice\n");
    assert!(!s.child_input_active);
}

#[test]
fn paste_in_queue_mode_inserts_into_editor() {
    let mut s = make_state();
    s.force_queue = true;
    // editor owns input now (force_queue + not auto_passthrough +
    // not manual_passthrough)
    assert!(s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_paste("ls -la".to_string(), &mut s, &mut w);
    assert_eq!(s.editor.buffer, "ls -la");
    assert!(buf.lock().unwrap().is_empty(), "PTY must not see paste");
}

#[test]
fn multiline_paste_in_queue_mode_preserves_block_shape() {
    let mut s = make_state();
    s.force_queue = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_paste("cat <<'EOF'\r\nhello\nEOF\n".to_string(), &mut s, &mut w);

    assert_eq!(s.editor.buffer, "cat <<'EOF'\nhello\nEOF");
    assert!(buf.lock().unwrap().is_empty(), "PTY must not see paste");
}
#[test]
fn ctrl_k_requires_a_second_press_before_clearing_the_queue() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay());
    s.queue.push("echo a", false);
    s.queue.push("echo b", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);

    let _ = handle_key(ctrl_k, &mut s, &mut w);
    assert_eq!(s.queue.len(), 2, "first Ctrl-K must only ask");
    assert!(s.pending_clear_active());
    assert!(s.status.contains("again"), "status: {}", s.status);

    // Any other key withdraws the confirmation.
    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(!s.pending_clear_active());
    assert!(
        !s.status.contains("again"),
        "withdrawn prompt must go: {}",
        s.status
    );
    let _ = handle_key(ctrl_k, &mut s, &mut w);
    assert_eq!(
        s.queue.len(),
        2,
        "a withdrawn confirmation must not carry over"
    );

    let _ = handle_key(ctrl_k, &mut s, &mut w);
    assert!(s.queue.is_empty());
    assert!(!s.pending_clear_active());
    assert!(s.status.contains("queue cleared"));
    assert!(
        buf.lock().unwrap().is_empty(),
        "Ctrl-K must not reach the child"
    );
}

#[test]
fn ctrl_k_on_empty_queue_does_not_arm_a_confirmation() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    let _ = handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(!s.pending_clear_active());
    assert!(s.status.contains("already empty"), "status: {}", s.status);
}

#[test]
fn pending_draft_keeps_panel_open_and_enter_runs_it_at_prompt() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    for c in "echo draft".chars() {
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
    }
    assert_eq!(s.editor.buffer, "echo draft");

    // The running command finishes before Enter.
    s.shell_state = ShellState::AtPrompt;
    s.command_started_at = None;
    s.last_exit_code = Some(0);
    assert!(
        s.panel_should_be_visible(),
        "draft must keep the panel open"
    );
    assert!(s.editor_owns_input(), "keys must keep going to the draft");

    let _ = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(
        s.queue.is_empty(),
        "idle shell should run the draft immediately"
    );
    assert_eq!(&*buf.lock().unwrap(), b"echo draft\n");
    assert!(s.editor.buffer.is_empty());
    assert!(!s.panel_should_be_visible());
}

#[test]
fn esc_discards_pending_draft_and_releases_panel() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.editor.insert_str("echo draft");
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    assert!(s.panel_should_be_visible());

    let _ = handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );

    assert!(s.editor.buffer.is_empty());
    assert!(!s.panel_should_be_visible());
    assert!(
        buf.lock().unwrap().is_empty(),
        "Esc must not leak to the shell"
    );
}

#[test]
fn enter_at_prompt_does_not_dispatch_into_a_paused_or_forced_queue() {
    for (paused, force) in [(true, false), (false, true)] {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.queue.paused = paused;
        s.force_queue = force;
        if paused {
            s.queue.push("echo earlier", false);
        }
        s.editor.insert_str("echo later");
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(
            buf.lock().unwrap().is_empty(),
            "paused={paused} force={force}"
        );
        assert!(s.queue.items().iter().any(|it| it.command == "echo later"));
    }
}
#[test]
fn f2_preserves_draft_and_routes_input_to_program_then_queue() {
    let mut s = make_state();
    s.command_started_at = Some(Instant::now() - panel_delay());
    s.editor.insert_str("echo later");
    s.editor.cursor = 5;
    s.editor.conditional = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_key(
        KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert_eq!(&*buf.lock().unwrap(), b"y");
    handle_key(
        KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(s.editor_owns_input());
    assert_eq!(s.editor.buffer, "echo later");
    assert_eq!(s.editor.cursor, 5);
    assert!(s.editor.conditional);
}

#[test]
fn f2_belongs_to_fullscreen_programs() {
    let mut s = make_state();
    s.auto_passthrough = true;
    s.child.alt_screen = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_key(
        KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert!(!s.manual_passthrough);
    assert!(!buf.lock().unwrap().is_empty());
}

#[test]
fn recovery_notice_does_not_capture_shell_typing_or_paste() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.queue.push("echo later", false);
    clear_queue(&mut s);
    clear_queue(&mut s);
    s.status_set_at = Some(Instant::now() - STATUS_TTL);
    s.tick_status();
    assert!(s.panel_should_be_visible());
    assert!(!s.editor_owns_input());
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    handle_paste("cho hello".into(), &mut s, &mut w);
    assert_eq!(&*buf.lock().unwrap(), b"echo hello");
    assert!(s.editor.buffer.is_empty());
    assert!(s.activity.is_some());
}

#[test]
fn restore_deleted_item_preserves_position_origin_condition_and_draft() {
    let mut s = make_state();
    s.force_queue = true;
    s.queue.push("first", false);
    s.queue
        .push_with_origin("second", true, Some(PathBuf::from("/tmp/elsewhere")));
    s.queue.push("third", false);
    s.editor.insert_str("my draft");
    s.editor.load_for_edit(1, "second", true);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );
    assert_eq!(s.editor.buffer, "my draft");
    restore_removed_commands(&mut s);
    assert!(s.queue.paused);
    assert_eq!(
        s.queue
            .items()
            .iter()
            .map(|i| i.command.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert!(s.queue.items()[1].conditional);
    assert_eq!(
        s.queue.items()[1].origin_cwd.as_deref(),
        Some(std::path::Path::new("/tmp/elsewhere"))
    );
    assert!(
        s.resume_cwd_warning.is_some(),
        "recovery must retain the foreign-directory warning"
    );
    assert!(
        buf.lock().unwrap().is_empty(),
        "recovery must not execute anything"
    );
    restore_removed_commands(&mut s);
    assert_eq!(s.queue.len(), 3, "undo is consumed exactly once");
}

#[test]
fn skipped_batch_remains_recoverable_after_later_command_dispatches() {
    let mut s = make_state();
    s.queue.push("skip one", true);
    s.queue.push("skip two", true);
    s.queue.push("echo run", false);
    s.queue_dirty = true;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    assert!(dispatch_next_eligible(&mut s, Some(7), &mut w));
    assert_eq!(&*buf.lock().unwrap(), b"echo run\n");
    assert_eq!(s.recovery.len(), 2);
    s.status_set_at = Some(Instant::now() - STATUS_TTL);
    s.tick_status();
    assert!(s.activity.as_deref().unwrap().contains("2 skipped"));
    restore_removed_commands(&mut s);
    assert_eq!(s.queue.len(), 2);
    assert!(s.queue.paused);
    assert_eq!(&*buf.lock().unwrap(), b"echo run\n");
}

#[test]
fn ctrl_x_starts_built_queue_without_extra_toggle() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.force_queue = true;
    s.queue.push("echo start", false);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    toggle_queue_pause(&mut s, &mut w);
    assert_eq!(&*buf.lock().unwrap(), b"echo start\n");
    assert!(!s.force_queue);
    assert!(!s.queue.paused);
    assert!(matches!(s.shell_state, ShellState::Running));
}

#[test]
fn ctrl_x_at_an_empty_prompt_is_absorbed() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.command_started_at = None;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

    handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut s,
        &mut w,
    );

    assert!(buf.lock().unwrap().is_empty());
    assert_eq!(s.status, "Nothing queued · Ctrl-Q opens the queue");
}

#[test]
fn ctrl_x_forwarding_can_be_configured_or_preserved_for_a_prompt_chord() {
    for configure_forwarding in [false, true] {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.command_started_at = None;
        s.config.keys.forward_ctrl_x = configure_forwarding;
        if !configure_forwarding {
            for c in "echo ".chars() {
                s.prompt
                    .on_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
        }
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"\x18");
    }
}

#[test]
fn running_an_idle_draft_captures_immediate_followup_typing() {
    let mut s = make_state();
    s.shell_state = ShellState::AtPrompt;
    s.command_started_at = None;
    s.editor.insert_str("sleep 2");
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        &mut s,
        &mut w,
    );
    assert_eq!(&*buf.lock().unwrap(), b"sleep 2\n");
    assert_eq!(s.editor.buffer, "e");
}
