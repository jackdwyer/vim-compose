use byteorder::{NativeEndian, ReadBytesExt, WriteBytesExt};
use indoc::formatdoc;
use log::{error, info, warn};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use scopeguard::defer;
use serde::{Deserialize, Serialize};
use simplelog::{LevelFilter, WriteLogger};
use std::cell::Cell;
use std::fs;
use std::io::{self, stderr, stdin, stdout, Read, Write};
use std::marker::Send;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::thread;
use tempdir::TempDir;

use std::time::Duration;

/// Spawns a thread that runs a provided function. When the thread is finished, the result of
/// running the function is sent via the provided Sender.
fn spawn_thread<T: Send + 'static>(done_sender: Sender<T>, f: impl 'static + Send + FnOnce() -> T) {
    thread::spawn(move || {
        // TODO: Handle panics as well.
        done_sender.send(f()).unwrap();
    });
}

fn spawn_editor(src_path: &Path) -> io::Result<(Child, Pid)> {
    let is_macos = cfg!(target_os = "macos");

    if is_macos {
        spawn_editor_alacritty(src_path)
    } else {
        spawn_editor_gnome(src_path)
    }
}

fn spawn_editor_gnome(src_path: &Path) -> io::Result<(Child, Pid)> {
    // Killing the gnome-terminal process launched here doesn't actually close the terminal,
    // because that is hosted by some daemon process. Instead we return the PID of the process
    // running inside of gnome-terminal. To learn its PID, we perform the following dance:
    //
    // 1. Run a shell child process for gnome-terminal.
    // 2. Print its PID to some temporary file via atomic renaming.
    // 3. Replace the shell process by the editor process.
    // 4. Wait until the PID file appears, then read off the PID from that.
    let src_path_disp = src_path.display();

    let pid_dir = TempDir::new("editor-pid")?;

    let tmp_pid_path = pid_dir.path().join("pid-tmp");
    let tmp_pid_path_disp = tmp_pid_path.display();

    let pid_path = pid_dir.path().join("pid");
    let pid_path_disp = pid_path.display();

    let child = Command::new("gnome-terminal")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--wait")
        .arg("--hide-menubar")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "})
        .spawn()?;
    let mut pid = String::new();
    while pid.is_empty() {
        if pid_path.exists() {
            pid = fs::read_to_string(&pid_path)?;
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let pid = pid.trim().parse::<i32>().unwrap();
    Ok((child, Pid::from_raw(pid)))
}

fn spawn_editor_alacritty(src_path: &Path) -> io::Result<(Child, Pid)> {
    let src_path_disp = src_path.display();

    let pid_dir = TempDir::new("editor-pid")?;

    let tmp_pid_path = pid_dir.path().join("pid-tmp");
    let tmp_pid_path_disp = tmp_pid_path.display();

    let pid_path = pid_dir.path().join("pid");
    let pid_path_disp = pid_path.display();

    let child = Command::new("/Applications/Alacritty.app/Contents/MacOS/alacritty")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("-e")
        .arg("sh")
        .arg("-c")
        .arg(formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "})
        .spawn()?;
    let mut pid = String::new();
    while pid.is_empty() {
        if pid_path.exists() {
            pid = fs::read_to_string(&pid_path)?;
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let pid = pid.trim().parse::<i32>().unwrap();
    Ok((child, Pid::from_raw(pid)))
}

fn spawn_editor_iterm2(src_path: &Path) -> io::Result<(Child, Pid)> {
    // On macOS, we use osascript to launch iTerm2 with a specific command.
    // We need to track the PID of the editor process, similar to the Linux approach.
    let src_path_disp = src_path.display();

    let pid_dir = TempDir::new("editor-pid")?;

    let tmp_pid_path = pid_dir.path().join("pid-tmp");
    let tmp_pid_path_disp = tmp_pid_path.display();

    let pid_path = pid_dir.path().join("pid");
    let pid_path_disp = pid_path.display();

    // Build the shell command that will write PID and exec vim
    let shell_command = formatdoc! {"
        echo $$ > '{tmp_pid_path_disp}'
        mv '{tmp_pid_path_disp}' '{pid_path_disp}'
        exec vim '{src_path_disp}'
    "};

    // Build the AppleScript to run in iTerm2
    // Note: We can't use --wait with iTerm2, so we spawn a dummy child process
    // that will be killed when we detect the editor has exited.
    let apple_script = format!(
        r#"tell application "iTerm2"
            create window with default profile command "{}"
        end tell"#,
        shell_command.replace('"', r#"\""#)
    );

    // Spawn osascript in background
    let child = Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Wait for the PID file to appear
    let mut pid = String::new();
    let mut attempts = 0;
    while pid.is_empty() && attempts < 100 {
        if pid_path.exists() {
            pid = fs::read_to_string(&pid_path)?;
        } else {
            thread::sleep(Duration::from_millis(50));
            attempts += 1;
        }
    }

    if pid.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timeout waiting for iTerm2 to write PID file",
        ));
    }

    let pid = pid.trim().parse::<i32>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to parse PID: {}", e),
        )
    })?;

    Ok((child, Pid::from_raw(pid)))
}

#[derive(Serialize, Deserialize, Debug)]
enum ContentType {
    Plain,
    Html,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "kind")]
enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Begin {
        initial_content: String,
        content_type: ContentType,
    },
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "kind")]
enum HostMessage {
    ReplaceAll { content: String },
}

fn read_message(pipe: &mut impl Read) -> io::Result<ClientMessage> {
    let msg_len = pipe.read_u32::<NativeEndian>()?;

    serde_json::from_reader(pipe.take(msg_len.into())).map_err(|err| err.into())
}

fn write_message(message: &HostMessage, pipe: &mut impl Write) -> io::Result<()> {
    let message: Vec<u8> = serde_json::to_string(&message)?.into_bytes();
    let msg_len = u32::try_from(message.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Tried sending message of more than 2^32 bytes",
        )
    })?;
    pipe.write_u32::<NativeEndian>(msg_len)?;
    pipe.write_all(&message)?;
    pipe.flush()?;
    Ok(())
}

fn disconnect(pipe: &mut impl Write) -> io::Result<()> {
    pipe.write_u32::<NativeEndian>(0)?;
    Ok(())
}

fn write_html_as_markdown(output: &Path, html: &str) -> io::Result<()> {
    let mut pandoc = Command::new("pandoc")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("-")
        .arg("--sandbox")
        .arg("--output")
        .arg(output)
        .arg("--from")
        .arg("html")
        .arg("--to")
        .arg("gfm")
        .spawn()?;

    let sanitized_html: ammonia::Document = ammonia::Builder::new().clean(html);
    sanitized_html.write_to(pandoc.stdin.take().unwrap())?;

    let status = pandoc.wait()?;
    if !status.success() {
        warn!("pandoc exited with status {status}");
    }
    Ok(())
}

fn read_markdown_as_html(input: &Path) -> io::Result<String> {
    let mut pandoc = Command::new("pandoc")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .arg(input)
        .arg("--sandbox")
        .arg("--output")
        .arg("-")
        .arg("--from")
        .arg("gfm")
        .arg("--to")
        .arg("html")
        .spawn()?;
    let html: ammonia::Document =
        ammonia::Builder::new().clean_from_reader(pandoc.stdout.take().unwrap())?;
    let status = pandoc.wait()?;
    if !status.success() {
        warn!("pandoc exited with status {status}");
    }
    Ok(html.to_string())
}

fn handle_messages(tmp_dir: &Path, exit: Sender<io::Result<()>>) -> io::Result<()> {
    let stdin = &mut stdin().lock();
    let mut got_begin_message = false;

    // The process id of the editor if we've started it, and a scope guard to make sure we're
    // terminating the editor if this function exists.
    let editor_pid: Cell<Option<Pid>> = Cell::new(None);
    defer! {
        if let Some(editor_pid) = editor_pid.get() {
            info!("Killing editor process {editor_pid}");
            if kill(editor_pid, Signal::SIGTERM).is_err() {
                error!("Could not kill editor");
            }
        }
    };

    loop {
        let message = match read_message(stdin) {
            Ok(message) => message,
            Err(err) => {
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    info!("Stdin was closed, exiting");
                    return Ok(());
                }
                return Err(err);
            }
        };

        let ClientMessage::Begin {
            initial_content,
            content_type,
        } = message;
        info!("Received \"begin\" message");
        if got_begin_message {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Received \"begin\" message twice",
            ));
        }
        got_begin_message = true;
        let src_path = match content_type {
            ContentType::Html => {
                let src_path = tmp_dir.join("input.md");
                write_html_as_markdown(&src_path, &initial_content)?;
                src_path
            }
            ContentType::Plain => {
                let src_path = tmp_dir.join("input");
                fs::write(&src_path, &initial_content)?;
                src_path
            }
        };
        let (mut child, child_pid) = spawn_editor(&src_path)?;
        editor_pid.set(Some(child_pid));
        spawn_thread(exit.clone(), move || {
            child.wait()?;
            error!("Editor process exited");
            Ok(())
        });

        {
            let src_path = src_path.clone();
            spawn_thread(exit.clone(), move || send_updates(&src_path, content_type));
        }
    }
}

fn send_updates(src_path: &Path, content_type: ContentType) -> io::Result<()> {
    let mut last_html: Option<String> = None;
    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new(
        move |res: Result<notify::Event, _>| {
            let _ = tx.send(res);
        },
        notify::Config::default(),
    )
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    watcher
        .watch(src_path, RecursiveMode::NonRecursive)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    loop {
        info!("Checking for updates in source");
        let html = match content_type {
            ContentType::Html => read_markdown_as_html(src_path)?,
            ContentType::Plain => fs::read_to_string(src_path)?,
        };
        if Some(&html) != last_html.as_ref() {
            info!("Generated HTML changed, sending update");
            last_html = Some(html.clone());
            let message = HostMessage::ReplaceAll { content: html };
            write_message(&message, &mut stdout().lock())?;
        }
        let _ = rx
            .recv()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    }
}

fn main() -> io::Result<()> {
    WriteLogger::init(
        LevelFilter::Warn,
        simplelog::Config::default(),
        // Change to
        //    File::create("/tmp/native-host-log")?,
        // and adjust the level filter for debugging on firefox.
        stderr(),
    )
    .unwrap();

    let (sender, receiver) = channel::<io::Result<()>>();

    let tmp_dir = TempDir::new("vim-compose")?;

    {
        let tmp_dir: PathBuf = tmp_dir.path().into();
        let sender = sender.clone();
        spawn_thread(sender.clone(), move || handle_messages(&tmp_dir, sender));
    }

    let result = receiver.recv().unwrap();
    if let Err(err) = result {
        error!("{err}");
        error!("{}", err.kind());
    }

    disconnect(&mut stdout().lock())?;
    info!("Exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_spawn_editor_gnome_command_structure() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test-file.txt");
        let src_path_disp = src_path.display();
        let pid_dir = temp_dir.path().join("pid-dir");
        fs::create_dir(&pid_dir).unwrap();
        let tmp_pid_path = pid_dir.join("pid-tmp");
        let tmp_pid_path_disp = tmp_pid_path.display();
        let pid_path = pid_dir.join("pid");
        let pid_path_disp = pid_path.display();

        let shell_command = formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "};

        assert!(shell_command.contains(&format!("exec vim '{}'", src_path_disp)));
        assert!(shell_command.contains(&format!("echo $$ > '{}'", tmp_pid_path_disp)));
        assert!(shell_command.contains(&format!("mv '{}' '{}'", tmp_pid_path_disp, pid_path_disp)));
    }

    #[test]
    fn test_spawn_editor_iterm2_applescript_structure() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test-file.txt");
        let src_path_disp = src_path.display();
        let tmp_pid_path = temp_dir.path().join("pid-tmp");
        let tmp_pid_path_disp = tmp_pid_path.display();
        let pid_path = temp_dir.path().join("pid");
        let pid_path_disp = pid_path.display();

        let shell_command = formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "};

        let apple_script = format!(
            r#"tell application "iTerm2"
            create window with default profile command "{}"
        end tell"#,
            shell_command.replace('"', r#"\""#)
        );

        assert!(apple_script.contains("tell application \"iTerm2\""));
        assert!(apple_script.contains("create window with default profile command"));
        assert!(apple_script.contains(&format!("exec vim '{}'", src_path_disp)));
    }

    #[test]
    fn test_send_updates_watches_file() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test-file.txt");
        File::create(&src_path).unwrap();

        let (tx, _rx) = channel();
        let watcher_result: io::Result<RecommendedWatcher> = Watcher::new(
            move |res: Result<notify::Event, _>| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e));

        assert!(watcher_result.is_ok(), "Watcher creation failed");

        let mut watcher = watcher_result.unwrap();
        let watch_result = watcher
            .watch(&src_path, RecursiveMode::NonRecursive)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));

        assert!(watch_result.is_ok(), "Failed to watch file");
    }

    #[test]
    fn test_spawn_editor_platform_selection() {
        let is_macos = cfg!(target_os = "macos");
        let is_linux = cfg!(target_os = "linux");

        assert!(
            is_macos || is_linux,
            "spawn_editor should be tested on macOS or Linux"
        );
    }

    #[test]
    fn test_spawn_editor_iterm2_escaped_quotes() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test-\"quote\".txt");
        let src_path_disp = src_path.display();
        let tmp_pid_path = temp_dir.path().join("pid-tmp");
        let tmp_pid_path_disp = tmp_pid_path.display();
        let pid_path = temp_dir.path().join("pid");
        let pid_path_disp = pid_path.display();

        let shell_command = formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "};

        let apple_script = format!(
            r#"tell application "iTerm2"
            create window with default profile command "{}"
        end tell"#,
            shell_command.replace('"', r#"\""#)
        );

        assert!(!apple_script.contains(r#"test-"quote".txt"#));
        assert!(apple_script.contains(r#"test-\"quote\".txt"#));
    }

    #[test]
    fn test_spawn_editor_gnome_command_arguments() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test.txt");

        let pid_dir = TempDir::new().unwrap();
        let tmp_pid_path = pid_dir.path().join("pid-tmp");
        let tmp_pid_path_disp = tmp_pid_path.display();
        let pid_path = pid_dir.path().join("pid");
        let pid_path_disp = pid_path.display();
        let src_path_disp = src_path.display();

        let shell_command = formatdoc! {"
            echo $$ > '{tmp_pid_path_disp}'
            mv '{tmp_pid_path_disp}' '{pid_path_disp}'
            exec vim '{src_path_disp}'
        "};

        assert!(shell_command.contains("exec"));
        assert!(shell_command.contains("vim"));
        assert!(shell_command.contains(&src_path_disp.to_string()));
    }

    #[test]
    fn test_file_content_reading() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("test.txt");
        let content = "test content";

        let mut file = File::create(&src_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let read_content = fs::read_to_string(&src_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[test]
    fn test_pid_file_creation() {
        let temp_dir = TempDir::new().unwrap();
        let tmp_pid_path = temp_dir.path().join("pid-tmp");
        let pid_path = temp_dir.path().join("pid");

        let pid = "12345";

        let mut file = File::create(&tmp_pid_path).unwrap();
        file.write_all(pid.as_bytes()).unwrap();
        fs::rename(&tmp_pid_path, &pid_path).unwrap();

        assert!(pid_path.exists());
        assert!(!tmp_pid_path.exists());

        let read_pid = fs::read_to_string(&pid_path).unwrap();
        assert_eq!(read_pid.trim(), pid);
    }
}
