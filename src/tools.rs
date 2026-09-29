use anyhow::{bail, Context, Result};
use cap_std::{ambient_authority, fs::Dir};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Component, Path},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub fn schemas(plan: bool) -> Vec<Value> {
    let specs = [
        ("list_files", "List files in the authorized workspace", json!({"path":{"type":"string"}}), vec![]),
        ("read_file", "Read a UTF-8 file in the authorized workspace", json!({"path":{"type":"string"}}), vec!["path"]),
        ("search_files", "Find literal text in workspace files; excludes hidden and generated directories", json!({"query":{"type":"string"}}), vec!["query"]),
        ("write_file", "Write UTF-8 content to a workspace file. Requires explicit approval.", json!({"path":{"type":"string"},"content":{"type":"string"}}), vec!["path","content"]),
        ("run_shell", "Run a command with the user's OS permissions, not in a security sandbox. Requires explicit approval. 30-second limit.", json!({"command":{"type":"string"}}), vec!["command"]),
    ];
    specs.into_iter().filter(|(name, _, _, _)| !plan || !sensitive(name))
        .map(|(name, description, properties, required)| json!({
            "type":"function","function":{"name":name,"description":description,
                "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}
        })).collect()
}

pub fn sensitive(name: &str) -> bool {
    matches!(name, "write_file" | "run_shell")
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("Missing string argument: {key}"))
}

fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        bail!("Path must stay inside the authorized workspace");
    }
    Ok(path)
}

pub async fn execute(
    root: &Path,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    if name == "run_shell" {
        return shell(root, arg(args, "command")?, cancel).await;
    }
    let root = root.to_path_buf();
    let name = name.to_string();
    let args = args.clone();
    tokio::task::spawn_blocking(move || files(&root, &name, &args)).await?
}

fn files(root: &Path, name: &str, args: &Value) -> Result<Value> {
    let dir = Dir::open_ambient_dir(root, ambient_authority())?;
    match name {
        "list_files" => {
            let path = relative(args["path"].as_str().unwrap_or("."))?;
            let mut entries = Vec::new();
            for item in dir.read_dir(path)?.take(500) {
                let item = item?;
                entries.push(json!({"name":item.file_name().to_string_lossy(),"directory":item.file_type()?.is_dir()}));
            }
            Ok(json!({"entries":entries,"limit":500}))
        }
        "read_file" => {
            let path = relative(arg(args, "path")?)?;
            let file = dir.open(path)?;
            if !file.metadata()?.is_file() {
                bail!("Only regular files can be read");
            }
            let mut body = String::new();
            file.take(262145).read_to_string(&mut body)?;
            if body.len() > 262144 {
                bail!("File exceeds the 256 KiB read limit");
            }
            Ok(json!({"path":path,"content":body}))
        }
        "write_file" => {
            let path = relative(arg(args, "path")?)?;
            let body = arg(args, "content")?;
            if body.len() > 262144 {
                bail!("Content exceeds the 256 KiB write limit");
            }
            // Capability-relative opens enforce containment even for symlinks.
            // Existing files are backed up before replacing content.
            if dir.exists(path) {
                let metadata = dir.symlink_metadata(path)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!("Only regular files may be replaced");
                }
                if metadata.len() > 262144 {
                    bail!("Existing file is too large to back up");
                }
                let original = dir.read(path)?;
                let backup = format!(
                    "{}.mutlink-backup-{}",
                    path.to_string_lossy(),
                    crate::model::id()
                );
                dir.write(&backup, original)?;
            }
            let temp = path.with_extension(format!("mutlink-{}", crate::model::id()));
            dir.write(&temp, body)?;
            dir.rename(&temp, &dir, path)?;
            Ok(json!({"path":path,"bytes":body.len(),"written":true}))
        }
        "search_files" => {
            let needle = arg(args, "query")?;
            if needle.is_empty() || needle.len() > 1024 {
                bail!("Search text must contain 1–1024 bytes");
            }
            let mut queue = vec![(std::path::PathBuf::from("."), 0)];
            let mut checked = 0;
            let mut matches = Vec::new();
            while let Some((path, depth)) = queue.pop() {
                if checked >= 2000 || matches.len() >= 100 {
                    break;
                }
                for entry in dir.read_dir(&path)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.')
                        || matches!(name.as_str(), "node_modules" | "target" | "dist")
                    {
                        continue;
                    }
                    let child = path.join(name);
                    let kind = entry.file_type()?;
                    if kind.is_symlink() {
                        continue;
                    }
                    if kind.is_dir() {
                        if depth < 10 {
                            queue.push((child, depth + 1));
                        }
                        continue;
                    }
                    if !kind.is_file() {
                        continue;
                    }
                    checked += 1;
                    if entry.metadata()?.len() > 262144 {
                        continue;
                    }
                    let Ok(body) = dir.read_to_string(&child) else {
                        continue;
                    };
                    for (line, text) in body.lines().enumerate() {
                        if text.contains(needle) {
                            matches.push(json!({"path":child,"line":line+1,"text":text.chars().take(500).collect::<String>()}));
                        }
                        if matches.len() >= 100 {
                            break;
                        }
                    }
                    if checked >= 2000 || matches.len() >= 100 {
                        break;
                    }
                }
            }
            Ok(json!({"matches":matches,"files_checked":checked,"bounded":true}))
        }
        _ => bail!("Unknown tool: {name}"),
    }
}

async fn collect(mut reader: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 4096];
    while let Ok(n) = reader.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        let keep = n.min(65536usize.saturating_sub(out.len()));
        out.extend_from_slice(&buffer[..keep]);
    }
    out
}

async fn shell(root: &Path, command: &str, cancel: CancellationToken) -> Result<Value> {
    if command.len() > 8192 {
        bail!("Command is too long");
    }
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", command]);
        cmd.process_group(0);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.args(["/D", "/S", "/C", command]);
        cmd
    };
    cmd.current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A command receives a minimal environment; provider/session credentials are not inherited.
    let allowed: Vec<_> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "LANG",
        "TERM",
    ]
    .into_iter()
    .filter_map(|k| std::env::var_os(k).map(|v| (k, v)))
    .collect();
    cmd.env_clear().envs(allowed);
    let mut child = cmd.spawn()?;
    let pid = child.id().context("Missing child process ID")?;
    let stdout = tokio::spawn(collect(child.stdout.take().unwrap()));
    let stderr = tokio::spawn(collect(child.stderr.take().unwrap()));
    let status = tokio::select! {
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(Duration::from_secs(30), child.wait()) => result.ok().transpose()?,
    };
    // End descendants too, including background jobs that kept output pipes open.
    #[cfg(unix)]
    {
        let _ = tokio::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(windows)]
    if status.is_none() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    if status.is_none() {
        let _ = child.kill().await;
    }
    let out = tokio::time::timeout(Duration::from_secs(2), stdout).await;
    let err = tokio::time::timeout(Duration::from_secs(2), stderr).await;
    let output =
        |value: Result<Result<Vec<u8>, tokio::task::JoinError>, tokio::time::error::Elapsed>| {
            String::from_utf8_lossy(&value.ok().and_then(Result::ok).unwrap_or_default())
                .into_owned()
        };
    Ok(
        json!({"exit_code":status.and_then(|s| s.code()),"stdout":output(out),"stderr":output(err),
        "interrupted":status.is_none(),"output_limit_bytes":65536}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_the_shell_and_descendants() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let path = root.path().to_path_buf();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            shell(
                &path,
                "touch ready; (sleep 1; printf late > late) & wait",
                token,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !root.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        let output = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(output["interrupted"], true);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!root.path().join("late").exists());
    }

    #[test]
    fn traversal_is_denied_and_writes_are_backed_up() {
        let root = tempfile::tempdir().unwrap();
        assert!(files(root.path(), "read_file", &json!({"path":"../secret"})).is_err());
        files(
            root.path(),
            "write_file",
            &json!({"path":"hello.txt","content":"before"}),
        )
        .unwrap();
        files(
            root.path(),
            "write_file",
            &json!({"path":"hello.txt","content":"after"}),
        )
        .unwrap();
        assert_eq!(
            files(root.path(), "read_file", &json!({"path":"hello.txt"})).unwrap()["content"],
            "after"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
    #[cfg(unix)]
    #[test]
    fn symlink_cannot_escape_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(files(root.path(), "read_file", &json!({"path":"escape/secret"})).is_err());
        assert!(files(
            root.path(),
            "write_file",
            &json!({"path":"escape/new","content":"bad"})
        )
        .is_err());
        assert!(!outside.path().join("new").exists());
    }
}
